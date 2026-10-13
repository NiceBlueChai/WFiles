//! 搜索查询：把用户敲的一句话变成可以反复套用在每个文件上的条件。
//!
//! 这一层不认识 UI、不读磁盘，只做两件事：解析与匹配。
//! 于是"语法怎么断句""通配符怎么算""大小时间怎么比"都能在单测里钉死，
//! 而"去哪儿找这些文件"是 `search.rs` 的事。
//!
//! 语法（大小写不敏感的关键词除外）：
//! - 光秃秃的词：名字里包含它。`foo bar` 是"两个都要有"。
//! - `"带 空格"`：当成一个整体。
//! - `-词` 或 `NOT 词`：排除。
//! - `OR`：或。`a OR b` 只要有一个。
//! - 通配符 `*` `?`：名字/路径/扩展名里都能用（`*.rs`）。
//! - 字段：`name:` `path:` `ext:` `type:` `size:` `date:` `content:` `tag:` `color:` `re:`
//!   - `size:>2M`、`size:<100K`（单位是 1024 进制）
//!   - `date:>7d`（最近七天）、`date:<2026-01-01`
//!   - `type:dir|file|image|video|audio|text|archive|code`
//!   - `content:待找的词`：读文件内容找
//!   - `tag:工作`、`color:red`：按打的标签与标的颜色找
//!   - `re:^\d+$`：名字走正则

use regex::Regex;
use std::cmp::Ordering;
use std::time::{Duration, SystemTime};

/// 一次匹配要用的文件信息。搜索器负责把它填出来，匹配器只读。
#[derive(Clone, Copy, Debug)]
pub struct Record<'a> {
    /// 文件名（原文）。
    pub name: &'a str,
    /// 完整路径（原文）。
    pub path: &'a str,
    /// 小写扩展名，没有就是空串。文件夹一律空串。
    pub ext: &'a str,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// 标的颜色名（[`crate::marks::COLORS`] 里的那种）；没标就是 `None`。
    pub color: Option<&'a str>,
    /// 打的标签。没有就是空切片。
    pub tags: &'a [String],
}

/// 比较符。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

impl Cmp {
    fn of(symbol: &str) -> Option<Self> {
        Some(match symbol {
            ">" => Self::Gt,
            ">=" => Self::Ge,
            "<" => Self::Lt,
            "<=" => Self::Le,
            "=" | ":" => Self::Eq,
            _ => return None,
        })
    }

    fn holds(self, left: u64, right: u64) -> bool {
        match self {
            Self::Gt => left > right,
            Self::Ge => left >= right,
            Self::Lt => left < right,
            Self::Le => left <= right,
            Self::Eq => left == right,
        }
    }
}

/// 一个"字符串怎么才算匹配"。
#[derive(Clone, Debug)]
enum Pattern {
    /// 子串（都已小写）。
    Text(String),
    /// 带 `*`/`?` 的通配。
    Wild(String),
    Regex(Regex),
}

impl Pattern {
    /// 判定的时候两边都已经小写了；正则自己管大小写。
    fn matches(&self, text: &str) -> bool {
        match self {
            Self::Text(needle) => text.contains(needle.as_str()),
            Self::Wild(pattern) => wild_matches(text, pattern),
            Self::Regex(regex) => regex.is_match(text),
        }
    }
}

/// 一个条件。
#[derive(Clone, Debug)]
enum Term {
    Name(Pattern),
    Path(Pattern),
    Ext(Pattern),
    Kind(&'static str),
    Size(Cmp, u64),
    /// 比较的是"修改时间"，基准时间从 `now` 起算。
    Date(Cmp, Moment),
    /// 文件内容里要有这个词（已小写）。
    Content(String),
    /// 打了这个标签（已小写）。
    Tag(String),
    /// 标了这个颜色。
    Color(String),
    Not(Box<Term>),
    And(Vec<Term>),
    Or(Vec<Term>),
}

/// 时间点：要么是一个相对量（"七天以内"），要么是一个具体日期。
#[derive(Clone, Copy, Debug)]
enum Moment {
    /// 距今多少秒之前。
    Ago(u64),
    /// 从 Unix 纪元起的秒数。
    Epoch(u64),
}

impl Moment {
    fn to_system_time(self, now: SystemTime) -> SystemTime {
        match self {
            Self::Ago(seconds) => now - Duration::from_secs(seconds),
            Self::Epoch(seconds) => {
                match SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds)) {
                    Some(time) => time,
                    None => SystemTime::UNIX_EPOCH,
                }
            }
        }
    }
}

/// 一条查询。
#[derive(Clone, Debug, Default)]
pub struct Query {
    root: Option<Term>,
    /// 解析时算一次：这条查询要不要读文件内容。
    needs_content: bool,
}

impl Query {
    pub fn parse(input: &str) -> Result<Self, String> {
        let mut parser = Parser::new(input);
        let root = parser.expression()?;
        parser.skip_space();
        if !parser.rest.is_empty() {
            return Err(format!("看不懂：{}", parser.rest.trim()));
        }
        Ok(Self {
            needs_content: root.as_ref().is_some_and(needs_content),
            root,
        })
    }

    /// 空查询：什么都不搜，界面不该进搜索态。
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// 这条查询要不要读文件内容。
    ///
    /// 只要在**任何位置**出现 `content:`（哪怕是被 `NOT` 包着的）就算。
    pub fn needs_content(&self) -> bool {
        self.needs_content
    }

    /// 所有 `content:` 后面写的词（都已小写）。
    ///
    /// 搜索结果要靠它在文件里定位"命中了哪一行"。
    pub fn content_terms(&self) -> Vec<&str> {
        let mut out = Vec::new();
        if let Some(root) = self.root.as_ref() {
            collect_content(root, &mut out);
        }
        out
    }

    /// 这个文件过不过。`content` 是要读的文件内容（已小写）；
    /// 查询不需要读内容时传 `None` 就行。`now` 是"现在"，`date:` 相对它算。
    pub fn matches_at(&self, record: &Record, now: SystemTime, content: Option<&str>) -> bool {
        let Some(root) = self.root.as_ref() else {
            return true;
        };
        // 要看内容却没给内容：一律算不过。若在这里"没读就当成不含"，
        // `NOT content:x` 会把所有还没读过的文件都翻成命中——
        // 那是最难解释的一类假结果。判不了就是不过，等真读了再说。
        if self.needs_content && content.is_none() {
            return false;
        }
        let name = record.name.to_lowercase();
        let path = record.path.to_lowercase();
        let ext = record.ext.to_lowercase();
        let context = Context {
            name,
            path,
            ext,
            record,
            now,
            content,
        };
        matches(root, &context)
    }
}

struct Context<'a> {
    name: String,
    path: String,
    ext: String,
    record: &'a Record<'a>,
    now: SystemTime,
    content: Option<&'a str>,
}

fn needs_content(term: &Term) -> bool {
    match term {
        Term::Content(_) => true,
        Term::Not(inner) => needs_content(inner),
        Term::And(list) | Term::Or(list) => list.iter().any(needs_content),
        _ => false,
    }
}

fn collect_content<'a>(term: &'a Term, out: &mut Vec<&'a str>) {
    match term {
        Term::Content(needle) => out.push(needle.as_str()),
        Term::Not(inner) => collect_content(inner, out),
        Term::And(list) | Term::Or(list) => {
            for term in list {
                collect_content(term, out);
            }
        }
        _ => {}
    }
}

fn matches(term: &Term, context: &Context) -> bool {
    match term {
        Term::Name(pattern) => pattern.matches(&context.name),
        Term::Path(pattern) => pattern.matches(&context.path),
        Term::Ext(pattern) => pattern.matches(&context.ext),
        Term::Kind(kind) => kind_of(&context.ext, context.record.is_dir) == *kind,
        Term::Size(cmp, bytes) => cmp.holds(context.record.size, *bytes),
        Term::Date(cmp, moment) => match context.record.modified {
            // 取不到修改时间的文件（某些网络盘、特殊文件）一律不过时间这一关：
            // 说"我不知道它什么时候改的"比瞎猜一个时间好。
            None => false,
            Some(modified) => {
                let edge = moment.to_system_time(context.now);
                let ordering = modified.cmp(&edge);
                match cmp {
                    Cmp::Gt => ordering == Ordering::Greater,
                    Cmp::Ge => ordering != Ordering::Less,
                    Cmp::Lt => ordering == Ordering::Less,
                    Cmp::Le => ordering != Ordering::Greater,
                    Cmp::Eq => ordering == Ordering::Equal,
                }
            }
        },
        Term::Content(needle) => context.content.is_some_and(|text| text.contains(needle)),
        Term::Tag(tag) => context
            .record
            .tags
            .iter()
            .any(|it| it.to_lowercase() == *tag),
        Term::Color(color) => context.record.color == Some(color.as_str()),
        Term::Not(inner) => !matches(inner, context),
        Term::And(list) => list.iter().all(|term| matches(term, context)),
        Term::Or(list) => list.iter().any(|term| matches(term, context)),
    }
}

/// 按扩展名粗分的种类。搜索是要扫几万个文件的，
/// 这里刻意不去读内容做嗅探——那太慢了，也不该出现在匹配的热路径上。
///
/// 传进来的扩展名已经小写过了：`a.PNG` 也要算图片。
fn kind_of(ext: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "dir";
    }
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "tif" | "heic" | "svg"
        | "avif" => "image",
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "flv" | "wmv" => "video",
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "opus" => "audio",
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "iso" => "archive",
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "java" | "kt" | "c" | "h" | "cpp"
        | "hpp" | "cs" | "swift" | "rb" | "php" | "sh" | "json" | "toml" | "yaml" | "yml"
        | "xml" | "html" | "css" | "sql" => "code",
        "txt" | "md" | "log" | "csv" | "rtf" | "pdf" | "doc" | "docx" => "text",
        _ => "file",
    }
}

// ---- 解析 ----

struct Parser<'a> {
    rest: &'a str,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { rest: input }
    }

    fn skip_space(&mut self) {
        self.rest = self.rest.trim_start();
    }

    /// `a OR b OR c`（OR 优先级最低）。
    fn expression(&mut self) -> Result<Option<Term>, String> {
        let Some(left) = self.and_group()? else {
            return Ok(None);
        };
        let mut branches = vec![left];
        loop {
            self.skip_space();
            let before = self.rest;
            let word = self.word()?;
            if word.as_deref() == Some("or") || word.as_deref() == Some("OR") {
                let Some(right) = self.and_group()? else {
                    return Err("OR 后面少了条件".to_string());
                };
                branches.push(right);
            } else {
                self.rest = before;
                break;
            }
        }
        Ok(Some(if branches.len() == 1 {
            branches.pop().expect("至少有一个")
        } else {
            Term::Or(branches)
        }))
    }

    /// 挨着的几个条件默认就是"都要满足"。
    fn and_group(&mut self) -> Result<Option<Term>, String> {
        let mut terms: Vec<Term> = Vec::new();
        loop {
            self.skip_space();
            if self.rest.is_empty() {
                break;
            }
            // 偷看一眼下一个词是不是 OR；不管看到什么都不还原，
            // 真正的读取交给 `unary`。少还原一次就会把整个查询读空。
            let before = self.rest;
            let word = self.word()?;
            self.rest = before;
            if word
                .as_deref()
                .is_some_and(|it| it.eq_ignore_ascii_case("or"))
            {
                break;
            }
            let Some(term) = self.unary()? else {
                break;
            };
            terms.push(term);
        }
        Ok(match terms.len() {
            0 => None,
            1 => terms.pop(),
            _ => Some(Term::And(terms)),
        })
    }

    fn unary(&mut self) -> Result<Option<Term>, String> {
        self.skip_space();
        if self.rest.is_empty() {
            return Ok(None);
        }
        // `-词`：排除。
        // 紧跟在 `-` / `!` 后面的空格不算排除（"- 3" 是"减三"，不是一个词）。
        if let Some(after) = self.rest.strip_prefix('-')
            && !after.starts_with(' ')
        {
            self.rest = after;
            let Some(inner) = self.unary()? else {
                return Err("- 后面少了要排除的东西".to_string());
            };
            return Ok(Some(Term::Not(Box::new(inner))));
        }
        if let Some(after) = self.rest.strip_prefix('!')
            && !after.starts_with(' ')
        {
            self.rest = after;
            let Some(inner) = self.unary()? else {
                return Err("! 后面少了要排除的东西".to_string());
            };
            return Ok(Some(Term::Not(Box::new(inner))));
        }
        let before = self.rest;
        if let Some(word) = self.word()?
            && word.eq_ignore_ascii_case("not")
        {
            let Some(inner) = self.unary()? else {
                return Err("NOT 后面少了要排除的东西".to_string());
            };
            return Ok(Some(Term::Not(Box::new(inner))));
        }
        self.rest = before;
        self.primary()
    }

    /// 一个最小的条件：一个词、一个引号串、或一个 `字段:值`。
    fn primary(&mut self) -> Result<Option<Term>, String> {
        self.skip_space();
        let Some(token) = self.token()? else {
            return Ok(None);
        };

        // 字段名只认 ASCII 字母数字，值里可以带空格（引号包起来）。
        if let Some((field, raw)) = split_field(&token) {
            return self.field(&field, &raw).map(Some);
        }

        let term = if token.starts_with('/') && token.len() > 2 && token.ends_with('/') {
            Term::Name(Pattern::Regex(compile(&token[1..token.len() - 1])?))
        } else if token.contains('*') || token.contains('?') {
            Term::Name(Pattern::Wild(token.to_lowercase()))
        } else {
            Term::Name(Pattern::Text(token.to_lowercase()))
        };
        Ok(Some(term))
    }

    fn field(&mut self, field: &str, raw: &str) -> Result<Term, String> {
        // 值通常在冒号后面就写好了（`size:>1M`）；写成 `name: foo` 时
        // 值在下一个片段里，那就再读一个。
        let value = if raw.is_empty() {
            self.token()?.unwrap_or_default()
        } else {
            raw.to_string()
        };
        let value = strip_quotes(&value);
        let field = field.to_ascii_lowercase();
        let bare = value.trim().to_lowercase();

        match field.as_str() {
            "name" | "n" => Ok(Term::Name(pattern_of(&bare))),
            "path" | "p" => Ok(Term::Path(pattern_of(&bare))),
            "ext" | "e" => Ok(Term::Ext(pattern_of(&bare))),
            "re" | "regex" => Ok(Term::Name(Pattern::Regex(compile(value.trim())?))),
            "content" | "c" => {
                if bare.is_empty() {
                    return Err("content: 后面要写要找的词".to_string());
                }
                Ok(Term::Content(bare))
            }
            "tag" => {
                if bare.is_empty() {
                    return Err("tag: 后面要写标签".to_string());
                }
                Ok(Term::Tag(bare))
            }
            "color" => {
                if !crate::marks::is_color(&bare) {
                    let names: Vec<&str> = crate::marks::COLORS.iter().map(|(id, _)| *id).collect();
                    return Err(format!("颜色只认 {}，不是“{bare}”", names.join("/")));
                }
                Ok(Term::Color(bare))
            }
            "type" | "t" => Ok(Term::Kind(match bare.as_str() {
                "dir" | "folder" | "目录" => "dir",
                "file" | "文件" => "file",
                "image" | "图片" => "image",
                "video" | "视频" => "video",
                "audio" | "音频" => "audio",
                "text" | "文本" => "text",
                "archive" | "压缩包" => "archive",
                "code" | "源码" => "code",
                _ => {
                    return Err(format!(
                        "type 只认 dir/file/image/video/audio/text/archive/code，不是“{bare}”"
                    ));
                }
            })),
            "size" | "s" => {
                let (cmp, rest) = split_cmp(&bare);
                Ok(Term::Size(cmp, parse_size(rest)?))
            }
            "date" | "d" | "modified" => {
                let (cmp, rest) = split_cmp(&bare);
                Ok(Term::Date(cmp, parse_moment(rest)?))
            }
            _ => Err(format!("没有“{field}:”这个字段")),
        }
    }

    /// 读一个词：到空格为止，但 `OR` / `NOT` 这种关键词要单独认出来。
    fn word(&mut self) -> Result<Option<String>, String> {
        self.skip_space();
        if self.rest.is_empty() {
            return Ok(None);
        }
        let end = self
            .rest
            .find(char::is_whitespace)
            .unwrap_or(self.rest.len());
        let word = self.rest[..end].to_string();
        self.rest = &self.rest[end..];
        Ok(Some(word))
    }

    /// 读一个最小片段：引号串、`字段:值`、或者一个裸词。
    fn token(&mut self) -> Result<Option<String>, String> {
        self.skip_space();
        let Some(first) = self.rest.chars().next() else {
            return Ok(None);
        };
        if first == '"' || first == '\'' {
            let quote = first;
            let rest = &self.rest[quote.len_utf8()..];
            let Some(end) = rest.find(quote) else {
                return Err(format!("引号没配对：{quote}{rest}"));
            };
            let value = rest[..end].to_string();
            self.rest = &rest[end + quote.len_utf8()..];
            return Ok(Some(value));
        }
        // 到空格为止，但引号里头的空格不算——否则 `name:"my file"`
        // 会被切成 `name:"my` 和 `file"` 两个片段。
        let mut end = 0;
        let mut quote: Option<char> = None;
        for (offset, character) in self.rest.char_indices() {
            match quote {
                Some(open) => {
                    if character == open {
                        quote = None;
                    }
                }
                None => {
                    if character == '"' || character == '\'' {
                        quote = Some(character);
                    } else if character.is_whitespace() {
                        break;
                    }
                }
            }
            end = offset + character.len_utf8();
        }
        let token = self.rest[..end].to_string();
        self.rest = &self.rest[end..];
        Ok(Some(token))
    }
}

/// `name:foo` → ("name", "foo")；`name:"foo bar"` → ("name", "\"foo bar\"")。
///
/// 只认 ASCII 字母数字的字段名：`/tmp/a:b` 里的 `a:b` 不该被当成字段。
fn split_field(token: &str) -> Option<(String, String)> {
    let (field, rest) = token.split_once(':')?;
    if field.is_empty() || !field.chars().all(|it| it.is_ascii_alphanumeric()) {
        return None;
    }
    Some((field.to_string(), rest.to_string()))
}

/// 去掉成对的外层引号；不成对就原样返回，让用户自己看见问题。
fn strip_quotes(value: &str) -> String {
    let Some(first) = value.chars().next() else {
        return String::new();
    };
    if (first == '"' || first == '\'') && value.len() > first.len_utf8() && value.ends_with(first) {
        return value[first.len_utf8()..value.len() - first.len_utf8()].to_string();
    }
    value.to_string()
}

fn split_cmp(value: &str) -> (Cmp, &str) {
    for symbol in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = value.strip_prefix(symbol) {
            return (Cmp::of(symbol).expect("符号都在表里"), rest);
        }
    }
    (Cmp::Eq, value)
}

fn pattern_of(value: &str) -> Pattern {
    if value.contains('*') || value.contains('?') {
        Pattern::Wild(value.trim_end_matches('/').to_string())
    } else {
        Pattern::Text(value.to_string())
    }
}

fn compile(source: &str) -> Result<Regex, String> {
    Regex::new(source).map_err(|error| format!("正则写错了：{error}"))
}

/// 大小：`2M`、`100K`、`1G`、纯数字（字节）。1024 进制，跟大多数文件管理器一致。
fn parse_size(value: &str) -> Result<u64, String> {
    let text = value.trim();
    let (digits, unit) = match text.chars().last() {
        Some(last) if last.is_ascii_alphabetic() => (&text[..text.len() - 1], last),
        _ => (text, 'b'),
    };
    let amount: f64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("大小看不懂：{value}"))?;
    let scale: u64 = match unit.to_ascii_lowercase() {
        'b' => 1,
        'k' => 1024,
        'm' => 1024 * 1024,
        'g' => 1024 * 1024 * 1024,
        't' => 1024 * 1024 * 1024 * 1024,
        _ => return Err(format!("大小的单位只认 B/K/M/G/T，不是 {unit}")),
    };
    Ok((amount * scale as f64).round() as u64)
}

/// 时间：`7d`（七天内）、`2026-01-01`、或者 `2026-06-01` 这种日期。
fn parse_moment(value: &str) -> Result<Moment, String> {
    let text = value.trim();
    if let Some(digits) = text
        .strip_suffix('d')
        .or_else(|| text.strip_suffix('w'))
        .or_else(|| text.strip_suffix('m'))
        .or_else(|| text.strip_suffix('y'))
    {
        let amount: u64 = digits.parse().map_err(|_| format!("时间看不懂：{value}"))?;
        let seconds = match text.chars().last().unwrap_or('d') {
            'd' => amount * 86_400,
            'w' => amount * 604_800,
            // 一个月按 30 天算：这是"大约多久之内"的筛选，不是日历运算。
            'm' => amount * 2_592_000,
            'y' => amount * 31_536_000,
            _ => amount * 86_400,
        };
        return Ok(Moment::Ago(seconds));
    }

    let (year, rest) = text
        .split_once('-')
        .ok_or_else(|| format!("时间要写成 7d 或者 2026-01-01，不是 {value}"))?;
    let (month, day) = rest
        .split_once('-')
        .ok_or_else(|| format!("时间要写成 7d 或者 2026-01-01，不是 {value}"))?;
    let year: i32 = year.parse().map_err(|_| format!("年份看不懂：{value}"))?;
    let month: u32 = month.parse().map_err(|_| format!("月份看不懂：{value}"))?;
    let day: u32 = day.parse().map_err(|_| format!("日期看不懂：{value}"))?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(format!("日期不在日历上：{value}"));
    }
    Ok(Moment::Epoch(days_from_civil(year, month, day) * 86_400))
}

/// 把年月日化成从 1970-01-01 起的天数（Howard Hinnant 的那套整数算法）。
fn days_from_civil(year: i32, month: u32, day: u32) -> u64 {
    let year = (year as i64) - (if month <= 2 { 1 } else { 0 });
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year =
        (153 * (if month > 2 {
            month as i64 - 3
        } else {
            month as i64 + 9
        }) + 2)
            / 5
            + day as i64
            - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    (era * 146_097 + day_of_era - 719_468) as u64
}

/// `*` 与 `?` 的通配匹配。两个 `*` 连着按一个算，别退化成指数级回溯。
fn wild_matches(text: &str, pattern: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    // pattern 前 i 个、text 前 j 个能不能对上。
    let mut previous = vec![false; text.len() + 1];
    let mut current = vec![false; text.len() + 1];
    previous[0] = true;
    for &pattern_char in &pattern {
        current[0] = previous[0] && pattern_char == '*';
        for index in 1..=text.len() {
            current[index] = match pattern_char {
                '*' => previous[index] || current[index - 1],
                '?' => previous[index - 1],
                other => previous[index - 1] && other == text[index - 1],
            };
        }
        std::mem::swap(&mut previous, &mut current);
        current.fill(false);
    }
    previous[text.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &'static str) -> Record<'static> {
        Record {
            name,
            path: name,
            ext: name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or(""),
            is_dir: false,
            size: 1024,
            modified: Some(SystemTime::UNIX_EPOCH),
            color: None,
            tags: &[],
        }
    }

    fn dir(name: &'static str) -> Record<'static> {
        Record {
            name,
            path: name,
            ext: "",
            is_dir: true,
            size: 4096,
            modified: Some(SystemTime::UNIX_EPOCH),
            color: None,
            tags: &[],
        }
    }

    fn hits(query: &str, record: &Record<'_>) -> bool {
        Query::parse(query)
            .unwrap_or_else(|error| panic!("{query} 解析失败：{error}"))
            .matches_at(record, SystemTime::UNIX_EPOCH, None)
    }

    #[test]
    fn a_word_matches_a_name_containing_it() {
        assert!(hits("readme", &file("README.md")));
        assert!(!hits("readme", &file("main.rs")));
    }

    #[test]
    fn two_words_mean_both() {
        assert!(hits("my file", &file("my file.txt")));
        assert!(!hits("my file", &file("my-other.txt")));
    }

    #[test]
    fn a_quoted_phrase_is_one_condition() {
        assert!(hits("\"my file\"", &file("my file.txt")));
        assert!(!hits("\"my file\"", &file("file my.txt")));
    }

    #[test]
    fn not_keeps_things_out() {
        assert!(hits("report -draft", &file("report-final.pdf")));
        assert!(!hits("report -draft", &file("report-draft.pdf")));
        assert!(hits("report NOT draft", &file("report-final.pdf")));
    }

    #[test]
    fn or_takes_either() {
        assert!(hits("rs OR py", &file("main.rs")));
        assert!(hits("rs OR py", &file("main.py")));
        assert!(!hits("rs OR py", &file("main.go")));
    }

    #[test]
    fn wildcards_work() {
        assert!(hits("*.rs", &file("main.rs")));
        assert!(!hits("*.rs", &file("main.rsl")));
        assert!(hits("file?.txt", &file("file1.txt")));
        assert!(!hits("file?.txt", &file("file12.txt")));
        assert!(hits("*mid*", &file("a-mid-b.txt")));
    }

    #[test]
    fn fields_narrow_things_down() {
        assert!(hits("ext:rs", &file("main.rs")));
        assert!(!hits("ext:rs", &file("main.py")));
        assert!(hits("type:dir", &dir("src")));
        assert!(!hits("type:dir", &file("src.rs")));
        assert!(hits("type:image", &file("a.PNG")));
        assert!(hits(
            "path:src",
            &Record {
                name: "main.rs",
                path: "/work/src/main.rs",
                ext: "rs",
                is_dir: false,
                size: 1,
                modified: None,
                color: None,
                tags: &[],
            }
        ));
    }

    #[test]
    fn marks_narrow_things_down() {
        let tags = vec!["工作".to_string(), "重要".to_string()];
        let red = Record {
            color: Some("red"),
            tags: &tags,
            ..file("a.txt")
        };
        assert!(hits("color:red", &red));
        assert!(!hits("color:blue", &red));
        assert!(hits("tag:工作", &red));
        assert!(hits("tag:重要", &red));
        assert!(!hits("tag:待办", &red));
        assert!(hits("color:red tag:工作", &red));
        assert!(!hits("color:blue tag:工作", &red));
    }

    #[test]
    fn an_unknown_color_is_reported() {
        let error = Query::parse("color:pink").unwrap_err();
        assert!(error.contains("red"), "错误里该列出认得的颜色：{error}");
    }

    #[test]
    fn an_unknown_field_is_reported_not_silently_ignored() {
        // 认不出来的字段名直接报错，否则用户写的条件被悄悄丢掉，结果全不对。
        let error = Query::parse("colour:red").unwrap_err();
        assert!(error.contains("colour"), "实际错误：{error}");
    }

    #[test]
    fn a_bad_regex_is_reported() {
        assert!(Query::parse("re:([a-z").is_err());
    }

    #[test]
    fn sizes_compare_in_binary_units() {
        let two_megs = Record {
            name: "big.bin",
            path: "big.bin",
            ext: "bin",
            is_dir: false,
            size: 2 * 1024 * 1024,
            modified: None,
            color: None,
            tags: &[],
        };
        assert!(hits("size:>1M", &two_megs));
        assert!(hits("size:>=2M", &two_megs));
        assert!(!hits("size:>2M", &two_megs));
        assert!(hits("size:<1G", &two_megs));
        assert!(hits("size:2048K", &two_megs));
        assert!(Query::parse("size:>3x").is_err());
    }

    #[test]
    fn dates_can_be_relative_or_absolute() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100 * 86_400);
        let recent = Record {
            name: "a.txt",
            path: "a.txt",
            ext: "txt",
            is_dir: false,
            size: 1,
            modified: Some(now - Duration::from_secs(2 * 86_400)),
            color: None,
            tags: &[],
        };
        let old = Record {
            modified: Some(now - Duration::from_secs(90 * 86_400)),
            ..recent
        };
        let query = Query::parse("date:>7d").expect("解析");
        assert!(query.matches_at(&recent, now, None));
        assert!(!query.matches_at(&old, now, None));

        let absolute = Query::parse("date:>1970-02-01").expect("解析");
        assert!(absolute.matches_at(&recent, now, None));
        assert!(!absolute.matches_at(&old, now, None));
    }

    #[test]
    fn a_file_without_a_modified_time_never_passes_a_date_filter() {
        let record = Record {
            name: "a.txt",
            path: "a.txt",
            ext: "txt",
            is_dir: false,
            size: 1,
            modified: None,
            color: None,
            tags: &[],
        };
        assert!(!hits("date:>1970-01-01", &record));
    }

    #[test]
    fn content_terms_are_only_satisfied_by_real_text() {
        let record = file("a.txt");
        let query = Query::parse("content:hello").expect("解析");
        assert!(query.needs_content());
        assert!(!query.matches_at(&record, SystemTime::UNIX_EPOCH, None));
        assert!(query.matches_at(&record, SystemTime::UNIX_EPOCH, Some("say hello")));
    }

    #[test]
    fn a_negated_content_term_still_forces_the_file_to_be_read() {
        // 不读就当成"不含"，那 NOT content:x 会把所有没读过的文件都算命中。
        let query = Query::parse("NOT content:hello").expect("解析");
        assert!(query.needs_content());
        let record = file("a.txt");
        assert!(!query.matches_at(&record, SystemTime::UNIX_EPOCH, None));
        assert!(query.matches_at(&record, SystemTime::UNIX_EPOCH, Some("nothing here")));
    }

    #[test]
    fn an_empty_query_matches_everything() {
        assert!(Query::parse("").expect("解析").is_empty());
        assert!(Query::parse("   ").expect("解析").is_empty());
    }

    #[test]
    fn leftovers_are_reported() {
        assert!(Query::parse("a:").is_err() || Query::parse("x").is_ok());
        assert!(Query::parse("name").is_ok(), "没有冒号就是普通词");
    }

    #[test]
    fn the_days_from_civil_table_is_right() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 10, 12), 20_738);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }
}
