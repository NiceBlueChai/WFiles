//! 文件内容的三种看法：按行、细到行内的字符、十六进制的字节。
//!
//! 这一层不认识 UI，也不读文件以外的东西：给它两串文本或两段字节，
//! 它还一份"哪一行/哪一个字节不一样"的清单，界面照着画就是了。
//!
//! 三条规矩：
//! - **算不出来就说算不出来**，绝不拿"看起来差不多"糊弄：超大文件、二进制、
//!   两边行数都很多时，宁可退化成"按位置对齐"并把这件事写在结果里；
//! - **先剪掉公共的前后缀**：真实的改动往往只在一处，剪完再做 LCS
//!   （那个是 O(行数×行数) 的），几万行的文件也用得上；
//! - **字符级高亮只取公共前后缀**：它回答的是"这一段改了"，不是"哪个词改了"。
//!   完整的词级 diff 会让结果变得难读，而好处只在极少数场景里看得出来。

use std::path::Path;

/// 一边最多读多少字节。超过就只读前面这一段，并如实报"截断"。
///
/// 两个 1 GB 的文件并排逐字节比，既占内存又没人看得完；
/// 真要比那么大的东西，该用专门的比对工具。
pub const MAX_BYTES: usize = 8 * 1024 * 1024;

/// 判"是不是二进制"只看开头这么多字节。
const SNIFF_BYTES: usize = 8 * 1024;

/// LCS 那张表的格子数上限。
///
/// 剪完公共前后缀后，真正要比的中段通常很小；万一两边中段都很大
/// （比如两份完全不同的日志），宁可退化成按位置对齐，也不能让表格吃掉几百 MB。
const MAX_CELLS: usize = 250_000;

/// 一行在两边的关系。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 两边都有，一模一样。
    Same,
    /// 两边都有，但不一样（行号可能对不上，因为是配对出来的）。
    Changed,
    /// 只有左边有（删掉的、或者右边没有的）。
    LeftOnly,
    /// 只有右边有（新加的）。
    RightOnly,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Same => " ",
            Self::Changed => "改",
            Self::LeftOnly => "左",
            Self::RightOnly => "右",
        }
    }
}

/// 文本比对里的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRow {
    pub kind: Kind,
    /// 行号（从 1 起）。只有一边有的时候另一边是 `None`。
    pub left_line: Option<usize>,
    pub right_line: Option<usize>,
    pub left: String,
    pub right: String,
    /// 行内"改了的那一段"。
    /// 只在 [`Kind::Changed`] 且开着字符级高亮时才有。
    pub left_span: Option<Span>,
    pub right_span: Option<Span>,
}

/// 文本比完的账。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextStats {
    pub same: usize,
    pub changed: usize,
    /// 只有右边有的行数（新加的）。
    pub added: usize,
    /// 只有左边有的行数（少掉的）。
    pub removed: usize,
}

impl TextStats {
    /// 一句话结论。**文案归 core**：界面（GUI / NSH / 以后的前端）只负责画，
    /// 不许各自拼一遍——否则同一份差异在三处会写成三种说法。
    ///
    /// 没说到的项就不出现（"新增 0 行"是噪音）。
    pub fn summary(&self) -> String {
        if self.changed == 0 && self.added == 0 && self.removed == 0 {
            return format!("两边一样，共 {} 行", self.same);
        }
        let mut parts: Vec<String> = Vec::new();
        if self.changed > 0 {
            parts.push(format!("改了 {} 行", self.changed));
        }
        if self.added > 0 {
            parts.push(format!("新增 {} 行", self.added));
        }
        if self.removed > 0 {
            parts.push(format!("少了 {} 行", self.removed));
        }
        parts.join(" · ")
    }
}

/// 十六进制里的一行：16 个字节，或者最后不满 16 个。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HexRow {
    pub offset: usize,
    pub left: Vec<u8>,
    pub right: Vec<u8>,
    /// 第几个字节不一样（下标对着这一行里靠前的那一段）。
    ///
    /// 两边长度不一样时，多出来的那半边整段都算"不一样"——
    /// 它确实在对面没有对应物。
    pub differs: Vec<bool>,
}

/// 字节比完的账。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HexStats {
    /// 不一样的字节数（含"对面没有"的那些）。
    pub different: usize,
    /// 第一处不一样的偏移。
    pub first_difference: Option<usize>,
}

impl HexStats {
    /// 一句话结论，规则同 [`TextStats::summary`]：文案归 core。
    pub fn summary(&self) -> String {
        match self.first_difference {
            None => "两边字节完全一样".to_string(),
            Some(at) => format!("{} 个字节不同，第一处偏移 0x{:x}", self.different, at),
        }
    }
}

/// 比一段文本：按行，并可细到行内的字符。
pub fn diff_text(left: &str, right: &str, char_level: bool) -> (Vec<TextRow>, TextStats) {
    let left_lines: Vec<&str> = split_lines(left);
    let right_lines: Vec<&str> = split_lines(right);
    let ops = ops(&left_lines, &right_lines);
    let rows = rows(&left_lines, &right_lines, &ops, char_level);

    let mut stats = TextStats::default();
    for row in &rows {
        match row.kind {
            Kind::Same => stats.same += 1,
            Kind::Changed => stats.changed += 1,
            Kind::LeftOnly => stats.removed += 1,
            Kind::RightOnly => stats.added += 1,
        }
    }
    (rows, stats)
}

/// 比两段字节：按 16 个一行排开，逐字节标出不一样的地方。
pub fn diff_hex(left: &[u8], right: &[u8]) -> (Vec<HexRow>, HexStats) {
    const WIDTH: usize = 16;
    let rows = left.len().max(right.len()).div_ceil(WIDTH);
    let mut out = Vec::with_capacity(rows);
    let mut stats = HexStats::default();

    for row in 0..rows {
        let offset = row * WIDTH;
        // 一边已经用完（文件更短）时那段就是空的：`get` 拿不到就当没有。
        let left = left
            .get(offset..(offset + WIDTH).min(left.len()))
            .unwrap_or_default();
        let right = right
            .get(offset..(offset + WIDTH).min(right.len()))
            .unwrap_or_default();
        let mut differs = Vec::with_capacity(WIDTH);
        for index in 0..WIDTH {
            let differs_here = match (left.get(index), right.get(index)) {
                (Some(left), Some(right)) => left != right,
                // 一边有、一边没有：确实不一样（多出来的或少了的）。
                (Some(_), None) | (None, Some(_)) => true,
                // 两边都没有：这一行没填满而已，不是差异。
                (None, None) => false,
            };
            if differs_here {
                stats.different += 1;
                if stats.first_difference.is_none() {
                    stats.first_difference = Some(offset + index);
                }
            }
            differs.push(differs_here);
        }
        out.push(HexRow {
            offset,
            left: left.to_vec(),
            right: right.to_vec(),
            differs,
        });
    }
    (out, stats)
}

/// 读一个文件，顺带判它像不像文本。
#[derive(Debug)]
pub struct Loaded {
    pub bytes: Vec<u8>,
    /// 能当文本看时是它的内容；二进制就是 `None`，那时候只能按字节比。
    pub text: Option<String>,
    /// 是不是只看了前面一段（超过 [`MAX_BYTES`]）。
    pub truncated: bool,
}

impl Loaded {
    pub fn is_text(&self) -> bool {
        self.text.is_some()
    }
}

pub fn load(path: &Path) -> Result<Loaded, crate::i18n::Msg> {
    let bytes = std::fs::read(path)
        .map_err(|error| crate::i18n::Message::caused("读不了：{error}", error))?;
    let truncated = bytes.len() > MAX_BYTES;
    let kept = &bytes[..bytes.len().min(MAX_BYTES)];
    let head = &kept[..kept.len().min(SNIFF_BYTES)];
    let text = (!head.contains(&0))
        .then(|| String::from_utf8(kept.to_vec()).ok())
        .flatten();
    Ok(Loaded {
        bytes: kept.to_vec(),
        text,
        truncated,
    })
}

// ---------------------------------------------------------------------------
// 行级 diff
// ---------------------------------------------------------------------------

/// 一条最原始的编辑动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    /// 这两行一样（下标对着左右各自的数组）。
    Equal(usize, usize),
    /// 左边这一行在右边没有。
    Delete(usize),
    /// 右边这一行在左边没有。
    Insert(usize),
}

/// 按 `\n` 分行，顺手去掉 `\r`（Windows 的文本文件）：
/// 否则同一个文件在两边会被判成每行都不一样。
///
/// 末尾那一行后面没有内容时不算一行：`"a\n"` 就是一行。
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines
}

/// 算出两边的编辑动作序列。
///
/// 先把公共的前缀与后缀剪掉（真实的改动通常只在一处），
/// 剩下的中段用 LCS；中段太大就退化成"按位置对齐"——
/// 那时候结果不漂亮，但至少算得出来。
fn ops(left: &[&str], right: &[&str]) -> Vec<Op> {
    let prefix = common_prefix(left, right);
    let left_middle = &left[prefix..];
    let right_middle = &right[prefix..];
    let suffix = common_suffix(left_middle, right_middle);
    let left_middle = &left_middle[..left_middle.len() - suffix];
    let right_middle = &right_middle[..right_middle.len() - suffix];

    let mut out: Vec<Op> = Vec::new();
    for index in 0..prefix {
        out.push(Op::Equal(index, index));
    }
    out.extend(middle_ops(left_middle, right_middle, prefix));
    for index in 0..suffix {
        out.push(Op::Equal(
            prefix + left_middle.len() + index,
            prefix + right_middle.len() + index,
        ));
    }
    out
}

/// 中段怎么比：能算 LCS 就算，算不起就按位置对齐。
fn middle_ops(left: &[&str], right: &[&str], offset: usize) -> Vec<Op> {
    let mut out = Vec::new();
    if left.is_empty() {
        for (index, _) in right.iter().enumerate() {
            out.push(Op::Insert(offset + index));
        }
        return out;
    }
    if right.is_empty() {
        for (index, _) in left.iter().enumerate() {
            out.push(Op::Delete(offset + index));
        }
        return out;
    }

    let cells = left.len().saturating_mul(right.len());
    if cells <= MAX_CELLS {
        out.extend(lcs_ops(left, right, offset));
        return out;
    }

    // 退化：一行对一行，多出来的算单边。
    let shared = left.len().min(right.len());
    for index in 0..shared {
        // 一样就记一样，不一样就一删一插（后面会配成"改了"）。
        if left[index] == right[index] {
            out.push(Op::Equal(offset + index, offset + index));
        } else {
            out.push(Op::Delete(offset + index));
            out.push(Op::Insert(offset + index));
        }
    }
    for index in shared..left.len() {
        out.push(Op::Delete(offset + index));
    }
    for index in shared..right.len() {
        out.push(Op::Insert(offset + index));
    }
    out
}

/// 经典 LCS：先填表（每个格子里是"到这儿为止最多能对齐几行"），再回溯出动作。
fn lcs_ops(left: &[&str], right: &[&str], offset: usize) -> Vec<Op> {
    let (rows, columns) = (left.len(), right.len());
    let mut table = vec![vec![0u32; columns + 1]; rows + 1];
    for row in (0..rows).rev() {
        for column in (0..columns).rev() {
            table[row][column] = if left[row] == right[column] {
                table[row + 1][column + 1] + 1
            } else {
                table[row + 1][column].max(table[row][column + 1])
            };
        }
    }

    let mut out = Vec::with_capacity(rows + columns);
    let (mut row, mut column) = (0usize, 0usize);
    while row < rows && column < columns {
        if left[row] == right[column] {
            out.push(Op::Equal(offset + row, offset + column));
            row += 1;
            column += 1;
        } else if table[row + 1][column] >= table[row][column + 1] {
            out.push(Op::Delete(offset + row));
            row += 1;
        } else {
            out.push(Op::Insert(offset + column));
            column += 1;
        }
    }
    while row < rows {
        out.push(Op::Delete(offset + row));
        row += 1;
    }
    while column < columns {
        out.push(Op::Insert(offset + column));
        column += 1;
    }
    out
}

/// 把动作序列摆成"两边并排"的一行行：
/// 连续的一删一插配成"改了"，剩下落单的就是"只有一边有"。
fn rows(left: &[&str], right: &[&str], ops: &[Op], char_level: bool) -> Vec<TextRow> {
    let mut out: Vec<TextRow> = Vec::new();
    let mut index = 0usize;
    while index < ops.len() {
        match ops[index] {
            Op::Equal(left_index, right_index) => {
                out.push(TextRow {
                    kind: Kind::Same,
                    left_line: Some(left_index + 1),
                    right_line: Some(right_index + 1),
                    left: left[left_index].to_string(),
                    right: right[right_index].to_string(),
                    left_span: None,
                    right_span: None,
                });
                index += 1;
            }
            Op::Delete(_) | Op::Insert(_) => {
                // 把紧跟着的一串删除与一串插入收集起来。
                let mut deleted: Vec<usize> = Vec::new();
                let mut inserted: Vec<usize> = Vec::new();
                while index < ops.len() {
                    match ops[index] {
                        Op::Delete(at) => deleted.push(at),
                        Op::Insert(at) => inserted.push(at),
                        Op::Equal(_, _) => break,
                    }
                    index += 1;
                }
                // 一删一插配成对：并排看的时候"改了"比"删一行、加一行"好懂得多。
                let pairs = deleted.len().min(inserted.len());
                for pair in 0..pairs {
                    let left_text = left[deleted[pair]];
                    let right_text = right[inserted[pair]];
                    let (left_span, right_span) = if char_level {
                        spans(left_text, right_text)
                    } else {
                        (None, None)
                    };
                    out.push(TextRow {
                        kind: Kind::Changed,
                        left_line: Some(deleted[pair] + 1),
                        right_line: Some(inserted[pair] + 1),
                        left: left_text.to_string(),
                        right: right_text.to_string(),
                        left_span,
                        right_span,
                    });
                }
                for at in &deleted[pairs..] {
                    out.push(TextRow {
                        kind: Kind::LeftOnly,
                        left_line: Some(at + 1),
                        right_line: None,
                        left: left[*at].to_string(),
                        right: String::new(),
                        left_span: None,
                        right_span: None,
                    });
                }
                for at in &inserted[pairs..] {
                    out.push(TextRow {
                        kind: Kind::RightOnly,
                        left_line: None,
                        right_line: Some(at + 1),
                        left: String::new(),
                        right: right[*at].to_string(),
                        left_span: None,
                        right_span: None,
                    });
                }
            }
        }
    }
    out
}

/// 一段行内区间：字符下标，左闭右开。
pub type Span = (usize, usize);

/// 行内"改了的那一段"：公共前缀之后、公共后缀之前。
///
/// 两边都是空区间（比如只加了几个字）时返回 `None`——
/// 高亮一段长度为 0 的东西只会画出一个看不见的色块。
fn spans(left: &str, right: &str) -> (Option<Span>, Option<Span>) {
    let left_chars: Vec<char> = left.chars().collect();
    let right_chars: Vec<char> = right.chars().collect();
    let prefix = common_prefix(&left_chars, &right_chars);
    let suffix = common_suffix(&left_chars[prefix..], &right_chars[prefix..]);
    let left_end = left_chars.len() - suffix;
    let right_end = right_chars.len() - suffix;
    let left_span = (prefix < left_end).then_some((prefix, left_end));
    let right_span = (prefix < right_end).then_some((prefix, right_end));
    (left_span, right_span)
}

fn common_prefix<T: PartialEq>(left: &[T], right: &[T]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

fn common_suffix<T: PartialEq>(left: &[T], right: &[T]) -> usize {
    left.iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
}

/// 一行里的一段：要么没变，要么变过（界面据此上色）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub changed: bool,
}

/// 把一行文本按"改了的那一段"切成几段：
/// 界面照着上色，比在整行上盖一层底色清楚得多。
pub fn text_segments(text: &str, span: Option<(usize, usize)>) -> Vec<Segment> {
    let Some((start, end)) = span else {
        return vec![Segment {
            text: text.to_string(),
            changed: false,
        }];
    };
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    push(&mut out, &chars[..start.min(chars.len())], false);
    push(
        &mut out,
        &chars[start.min(chars.len())..end.min(chars.len())],
        true,
    );
    push(&mut out, &chars[end.min(chars.len())..], false);
    out
}

/// 把一行字节按"这个字节变没变"切成几段（连续的同类并成一段）。
pub fn hex_segments(bytes: &[u8], differs: &[bool]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for (index, byte) in bytes.iter().enumerate() {
        let changed = differs.get(index).copied().unwrap_or(false);
        match out.last_mut() {
            // 跟前一段同类就接上去：一行 16 个字节最多也就绘几段。
            Some(last) if last.changed == changed => {
                last.text.push(' ');
                last.text.push_str(&format!("{byte:02x}"));
            }
            _ => out.push(Segment {
                text: format!("{byte:02x}"),
                changed,
            }),
        }
    }
    out
}

fn push(out: &mut Vec<Segment>, chars: &[char], changed: bool) {
    if chars.is_empty() {
        return;
    }
    out.push(Segment {
        text: chars.iter().collect(),
        changed,
    });
}

/// 把一行字节写成十六进制：`48 65 6c 6c 6f`。
pub fn hex_line(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 同一行字节的可读部分：不是可打印字符就画个点。
///
/// 十六进制视图里没有它，就只剩一串看不出所以然的数字。
pub fn ascii_line(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' {
                *byte as char
            } else {
                '.'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// 结论句由 core 出，界面只画。这三条钉的是"0 项不出现"——
    /// 多写一句"新增 0 行"看着像有变化，其实是噪音。
    #[test]
    fn text_summary_lists_only_what_happened() {
        let same = TextStats {
            same: 7,
            ..TextStats::default()
        };
        assert_eq!(same.summary(), "两边一样，共 7 行");

        let mixed = TextStats {
            same: 3,
            changed: 2,
            added: 0,
            removed: 1,
        };
        assert_eq!(mixed.summary(), "改了 2 行 · 少了 1 行");
        assert!(!mixed.summary().contains("新增"), "0 项不许出现：{}", mixed.summary());

        // 三种都有时按「改 → 增 → 少」的固定顺序，别随统计结构体的字段顺序漂。
        let all = TextStats {
            same: 0,
            changed: 1,
            added: 2,
            removed: 3,
        };
        assert_eq!(all.summary(), "改了 1 行 · 新增 2 行 · 少了 3 行");
    }

    #[test]
    fn hex_summary_says_where_the_first_difference_is() {
        let identical = HexStats::default();
        assert_eq!(identical.summary(), "两边字节完全一样");

        let differs = HexStats {
            different: 5,
            first_difference: Some(0x20),
        };
        // 偏移用十六进制报：比对二进制的人按这个去看 dump。
        assert!(differs.summary().contains("0x20"), "{}", differs.summary());
        assert!(differs.summary().contains('5'), "{}", differs.summary());
    }

    fn rows_of(left: &str, right: &str) -> Vec<TextRow> {
        diff_text(left, right, false).0
    }

    fn kinds(rows: &[TextRow]) -> Vec<Kind> {
        rows.iter().map(|row| row.kind).collect()
    }

    #[test]
    fn identical_text_has_only_same_rows() {
        let rows = rows_of("a\nb\nc\n", "a\nb\nc\n");
        assert_eq!(rows.len(), 3);
        assert!(kinds(&rows).iter().all(|kind| *kind == Kind::Same));
    }

    #[test]
    fn an_added_line_show_up_as_right_only() {
        let rows = rows_of("a\nb\n", "a\nb\nc\n");
        assert_eq!(kinds(&rows), [Kind::Same, Kind::Same, Kind::RightOnly]);
        assert_eq!(rows[2].right_line, Some(3));
        assert_eq!(rows[2].left_line, None);
    }

    #[test]
    fn a_removed_line_shows_up_as_left_only() {
        let rows = rows_of("a\nb\nc\n", "a\nc\n");
        assert_eq!(kinds(&rows), [Kind::Same, Kind::LeftOnly, Kind::Same]);
        assert_eq!(rows[1].left, "b");
    }

    #[test]
    fn a_rewritten_line_is_paired_as_changed() {
        let rows = rows_of("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(kinds(&rows), [Kind::Same, Kind::Changed, Kind::Same]);
        assert_eq!(rows[1].left, "b");
        assert_eq!(rows[1].right, "B");
        assert_eq!(rows[1].left_line, Some(2));
        assert_eq!(rows[1].right_line, Some(2));
    }

    #[test]
    fn windows_line_endings_do_not_spoil_every_line() {
        let rows = rows_of("a\nb\n", "a\r\nb\r\n");
        assert!(kinds(&rows).iter().all(|kind| *kind == Kind::Same));
    }

    #[test]
    fn a_trailing_newline_is_not_an_extra_line() {
        assert_eq!(rows_of("a\n", "a").len(), 1);
    }

    #[test]
    fn the_stats_count_each_kind() {
        let (_, stats) = diff_text("a\nb\nc\n", "a\nB\nc\nd\n", false);
        assert_eq!(stats.same, 2);
        assert_eq!(stats.changed, 1);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.removed, 0);
    }

    #[test]
    fn char_level_highlight_marks_the_middle_only() {
        let (rows, _) = diff_text("fn main() {}", "fn main() -> i32 {}", true);
        let row = rows.first().expect("有一行");
        assert_eq!(row.kind, Kind::Changed);
        let (start, end) = row.right_span.expect("右边该有改动区间");
        assert_eq!(&row.right[start..end], "-> i32 ");
        // 左边那一行只是被往里插了一段：它没有"被改掉的部分"，
        // 标一个长度为 0 的区间只会画出看不见的色块。
        assert!(row.left_span.is_none());
    }

    #[test]
    fn char_level_highlight_is_off_until_asked_for() {
        let (rows, _) = diff_text("abc", "abd", false);
        assert!(rows[0].left_span.is_none());
        assert!(rows[0].right_span.is_none());
    }

    #[test]
    fn hex_rows_mark_the_bytes_that_differ() {
        let (rows, stats) = diff_hex(b"hello world!", b"hello there!");
        assert_eq!(rows[0].offset, 0);
        assert_eq!(stats.first_difference, Some(6));
        // "world" vs "there"：5 个字节全不一样。
        assert_eq!(stats.different, 5);
        let differing: Vec<usize> = rows[0]
            .differs
            .iter()
            .enumerate()
            .filter(|(_, differs)| **differs)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(differing, [6, 7, 8, 9, 10]);
    }

    #[test]
    fn hex_rows_cover_the_longer_side() {
        let (rows, stats) = diff_hex(b"abc", b"abcdefghijklmnopqrst");
        assert_eq!(rows.len(), 2, "20 个字节该占两行");
        assert_eq!(
            rows[1].differs.iter().filter(|differs| **differs).count(),
            4,
            "右边多出来的 4 个字节算不一样，剩下的只是这一行没填满"
        );
        assert_eq!(stats.first_difference, Some(3));
    }

    #[test]
    fn identical_bytes_have_no_differences() {
        let (_, stats) = diff_hex(b"same", b"same");
        assert_eq!(stats.different, 0);
        assert_eq!(stats.first_difference, None);
    }

    #[test]
    fn text_segments_split_around_the_changed_part() {
        let segments = text_segments("fn main() -> i32 {}", Some((10, 17)));
        let parts: Vec<(&str, bool)> = segments
            .iter()
            .map(|it| (it.text.as_str(), it.changed))
            .collect();
        assert_eq!(
            parts,
            [("fn main() ", false), ("-> i32 ", true), ("{}", false)]
        );
    }

    #[test]
    fn text_without_a_span_is_one_segment() {
        let segments = text_segments("整行都一样", None);
        assert_eq!(segments.len(), 1);
        assert!(!segments[0].changed);
    }

    #[test]
    fn hex_segments_merge_neighbours_of_the_same_kind() {
        let row = HexRow {
            offset: 0,
            left: b"aabbccdd".to_vec(),
            right: b"aabbccdd".to_vec(),
            differs: vec![false, false, true, true, false, false, false, true],
        };
        let segments = hex_segments(&row.left, &row.differs);
        let parts: Vec<(&str, bool)> = segments
            .iter()
            .map(|it| (it.text.as_str(), it.changed))
            .collect();
        assert_eq!(
            parts,
            // b"aabbccdd" 就是 61 61 62 62 63 63 64 64；连续同类的并成一段。
            [
                ("61 61", false),
                ("62 62", true),
                ("63 63 64", false),
                ("64", true)
            ]
        );
    }

    #[test]
    fn hex_and_ascii_lines_read_the_way_people_expect() {
        assert_eq!(hex_line(b"Hi!"), "48 69 21");
        assert_eq!(ascii_line(b"Hi!\x01"), "Hi!.");
    }

    #[test]
    fn a_text_file_loads_as_text() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("a.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let loaded = load(&path).expect("读得出");
        assert!(loaded.is_text());
        assert_eq!(loaded.text.as_deref(), Some("hello\n"));
        assert!(!loaded.truncated);
    }

    #[test]
    fn a_binary_file_has_no_text() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("a.bin");
        let mut bytes = b"hello \0 world".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 100));
        std::fs::write(&path, bytes).unwrap();
        let loaded = load(&path).expect("读得出");
        assert!(!loaded.is_text(), "带 NUL 的不是文本");
        assert_eq!(loaded.bytes.len(), 113);
    }

    #[test]
    fn a_missing_file_says_why() {
        let error = load(Path::new("/并不存在的文件")).expect_err("该报错");
        assert!(error.to_string().contains("读不了"));
    }

    #[test]
    fn a_huge_file_is_truncated_not_fully_loaded() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("big.txt");
        std::fs::write(&path, "x".repeat(MAX_BYTES + 10)).unwrap();
        let loaded = load(&path).expect("读得出");
        assert!(loaded.truncated);
        assert_eq!(loaded.bytes.len(), MAX_BYTES);
    }

    #[test]
    fn a_big_edit_still_comes_out_sane() {
        // 两边都没有公共前后缀，且行数不少：走 LCS（格子数在限额内）。
        let left: String = (0..200).map(|index| format!("left {index}\n")).collect();
        let right: String = (0..200).map(|index| format!("right {index}\n")).collect();
        let (rows, stats) = diff_text(&left, &right, false);
        assert_eq!(stats.same, 0);
        assert_eq!(stats.changed, 200);
        assert_eq!(rows.len(), 200);
    }
}
