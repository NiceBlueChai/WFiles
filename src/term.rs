//! 内嵌终端的纯逻辑：按键怎么变成字节、屏幕怎么存、回滚怎么翻、选中怎么取文本。
//!
//! 这一层既不认识 GPUI 也不认识 PTY：
//! - [`Screen`] 包住 `vt100` 的解析结果，负责喂字节、改尺寸、翻回滚；
//! - [`encode_key`] / [`paste`] 把一次按键或一段粘贴变成要写进 PTY 的字节；
//! - [`Selection`] 只管"哪几格被选中"。
//!
//! 真正起进程、收字节、画格子在 `term_entity.rs`。这样"Ctrl+C 发出去的是 0x03"
//! 这种事能在单测里钉死，不必真的开一个 shell。

use gpui_kit::{Keystroke, Modifiers};
use std::path::{Path, PathBuf};

/// 回滚里最多留多少行。
///
/// 再多是拿内存换"往上翻很久"——日常看编译输出几百行足够，
/// 真要翻一整天的日志那是用 `less` 的场合。
pub const SCROLLBACK: usize = 2000;

/// 屏幕再小也不能小到这两条线以下，否则连提示符都画不出来。
pub const MIN_ROWS: u16 = 2;
pub const MIN_COLS: u16 = 20;

/// 一个格子的前景或背景色。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    /// 终端自己的默认色。界面上按主题的前/背景画，不假装知道是哪个色号。
    #[default]
    Default,
    /// 256 色表里的一个下标；0-15 是标准 ANSI 十六色。
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    fn of(color: vt100::Color) -> Self {
        match color {
            vt100::Color::Default => Self::Default,
            vt100::Color::Idx(index) => Self::Indexed(index),
            vt100::Color::Rgb(red, green, blue) => Self::Rgb(red, green, blue),
        }
    }
}

/// 一个格子的样式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Face {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Face {
    fn of(cell: &vt100::Cell) -> Self {
        Self {
            fg: Color::of(cell.fgcolor()),
            bg: Color::of(cell.bgcolor()),
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }
}

/// 一行里连续几格样式相同：界面一次画一段，而不是一格一个元素。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub face: Face,
}

/// 屏幕坐标里的一个格子。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub row: u16,
    pub col: u16,
}

/// 鼠标拖出来的选中区：两个角，先后顺序无所谓。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Cell,
    pub head: Cell,
}

impl Selection {
    pub fn new(anchor: Cell, head: Cell) -> Self {
        Self { anchor, head }
    }

    /// 从左上到右下的两个角，闭区间。
    pub fn bounds(self) -> (Cell, Cell) {
        let (row_from, row_to) = ordered(self.anchor.row, self.head.row);
        let (col_from, col_to) = ordered(self.anchor.col, self.head.col);
        (
            Cell {
                row: row_from,
                col: col_from,
            },
            Cell {
                row: row_to,
                col: col_to,
            },
        )
    }

    /// 只点了一下没拖动：那不是"选中了一格"，是取消选中。
    pub fn is_empty(self) -> bool {
        self.anchor == self.head
    }
}

fn ordered(left: u16, right: u16) -> (u16, u16) {
    if left <= right {
        (left, right)
    } else {
        (right, left)
    }
}

/// 一块屏幕：解析出来的格子、光标、回滚。
pub struct Screen {
    parser: vt100::Parser,
    /// 回滚偏移：0 = 贴着最新的输出，越大越往上翻。
    offset: usize,
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows.max(MIN_ROWS), cols.max(MIN_COLS), SCROLLBACK),
            offset: 0,
        }
    }

    /// 喂一段子进程输出。
    ///
    /// 刻意不去动回滚偏移：正在翻历史的用户不该被新输出拽回底部，
    /// 而贴着底部的用户偏移本来就是 0，新行照样看得见。
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// （行，列）。
    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    /// 改尺寸。真正生效还要把新尺寸告诉 PTY，那是实体的事。
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(MIN_ROWS);
        let cols = cols.max(MIN_COLS);
        if self.size() == (rows, cols) {
            return;
        }
        self.parser.screen_mut().set_size(rows, cols);
        // 尺寸一变，旧的偏移对应的就不是同一段历史了，回到底部最不容易困惑。
        self.parser.screen_mut().set_scrollback(0);
        self.offset = 0;
    }

    /// 光标位置。翻着回滚的时候光标不在可见区里，别画。
    pub fn cursor(&self) -> Option<(u16, u16)> {
        if self.offset > 0 || self.parser.screen().hide_cursor() {
            None
        } else {
            Some(self.parser.screen().cursor_position())
        }
    }

    /// 程序开了"应用光标"模式：方向键要用另一套写法。
    pub fn application_cursor(&self) -> bool {
        self.parser.screen().application_cursor()
    }

    /// 程序开了括号粘贴模式：粘贴要包起来，否则多行文本会被当成一条条命令。
    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    /// 程序切到了备用屏幕（`vim`、`less` 那类）：这时候回滚翻的是另一个缓冲区。
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.go(self.offset.saturating_add(lines));
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.go(self.offset.saturating_sub(lines));
    }

    pub fn scroll_home(&mut self) {
        self.go(0);
    }

    fn go(&mut self, offset: usize) {
        self.parser.screen_mut().set_scrollback(offset);
        // vt100 会把偏移夹到实际的历史长度上，这里照抄它夹完的值：
        // "按了往上翻但已经到顶"不该让偏移一直往上加。
        self.offset = self.parser.screen().scrollback();
    }

    /// 可见的一行，按样式切成几段。
    pub fn line(&self, row: u16) -> Vec<Run> {
        let (_, cols) = self.size();
        let screen = self.parser.screen();
        let mut runs: Vec<Run> = Vec::new();
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            // 宽字符占两格，第二格没有自己的内容。
            if cell.is_wide_continuation() {
                continue;
            }
            let face = Face::of(cell);
            match runs.last_mut() {
                Some(run) if run.face == face => run.text.push_str(cell.contents()),
                _ => runs.push(Run {
                    text: cell.contents().to_string(),
                    face,
                }),
            }
        }
        runs
    }

    /// 可见内容的纯文本（行尾空白去掉）。"复制可见内容"用的就是它。
    pub fn plain(&self) -> String {
        self.lines().join("\n")
    }

    /// 可见内容按行拆开，行尾空白去掉。
    pub fn lines(&self) -> Vec<String> {
        let (rows, _) = self.size();
        (0..rows).map(|row| self.row_text(row)).collect()
    }

    /// 选中区里的文本。行尾空白一律去掉：终端里右侧那一长串空格是填充，不是内容。
    pub fn selected_text(&self, selection: Selection) -> String {
        let (from, to) = selection.bounds();
        (from.row..=to.row)
            .map(|row| {
                let mut line = String::new();
                for col in from.col..=to.col {
                    if let Some(cell) = self.parser.screen().cell(row, col)
                        && !cell.is_wide_continuation()
                    {
                        line.push_str(cell.contents());
                    }
                }
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn row_text(&self, row: u16) -> String {
        let (_, cols) = self.size();
        let screen = self.parser.screen();
        let mut text = String::new();
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col)
                && !cell.is_wide_continuation()
            {
                text.push_str(cell.contents());
            }
        }
        text.trim_end().to_string()
    }
}

/// 一次按键要写进 PTY 的字节。
///
/// `None` 表示这一下不往终端里送：按了 Cmd（那是宿主应用的键，
/// 复制、粘贴、切标签都归它），或者按了个我们不认识的键。
pub fn encode_key(key: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    // Cmd/Super 永远留给宿主：Cmd+C 是复制而不是中断进程，这是平台惯例。
    if key.modifiers.platform || key.modifiers.function {
        return None;
    }

    // 打出来的那个字符才是终端要的。`Keystroke::key` 是"忽略修饰键的那个键"：
    // macOS 上按 Shift+A 给的是 key="a" + shift，照 `key` 发过去就全成小写了。
    // Enter/Tab/空格在 `key_char` 里分别是 "\n" / "\t" / " "，正好也走这条路。
    if let Some(text) = key.key_char.as_deref().filter(|text| !text.is_empty()) {
        return typed(text, key.modifiers);
    }

    // 剩下的都是打不出字符的键：方向键、Home/End、F1-F12，以及所有 Ctrl 组合
    // （按住 Ctrl 时平台不给 `key_char`）。
    named(&key.key, app_cursor, key.modifiers)
}

/// 打出来的字符：Ctrl 组合变成控制码，Alt 前缀一个 ESC，回车一律是 \r。
fn typed(text: &str, modifiers: Modifiers) -> Option<Vec<u8>> {
    if modifiers.control {
        let code = control_code(text.chars().next()?)?;
        return Some(with_alt(modifiers.alt, std::slice::from_ref(&code)));
    }
    let mut out = Vec::new();
    if modifiers.alt {
        out.push(0x1b);
    }
    for character in text.chars() {
        // 终端的行编辑器认的是 \r，不是 \n。
        if character == '\n' {
            out.push(b'\r');
        } else {
            push_utf8(&mut out, character);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// 打不出字符的键。
fn named(key: &str, app_cursor: bool, modifiers: Modifiers) -> Option<Vec<u8>> {
    let Modifiers {
        control,
        alt,
        shift,
        ..
    } = modifiers;

    // Ctrl+C 这类：平台没给 key_char，但 `key` 就是那个字符。
    if let Some(character) = single_char(key) {
        if control {
            return control_code(character).map(|code| with_alt(alt, std::slice::from_ref(&code)));
        }
        let mut out = Vec::new();
        if alt {
            out.push(0x1b);
        }
        push_utf8(&mut out, character);
        return Some(out);
    }

    let parameter = modifier_parameter(control, alt, shift);
    let bytes = match key {
        "enter" => with_alt(alt, b"\r"),
        "tab" if shift => b"\x1b[Z".to_vec(),
        "tab" => vec![b'\t'],
        "backspace" => with_alt(
            alt,
            std::slice::from_ref(if control { &0x08 } else { &0x7f }),
        ),
        "escape" => vec![0x1b],
        "space" if control => vec![0x00],
        "space" => vec![b' '],
        "up" => cursor_key(b'A', app_cursor, parameter)?,
        "down" => cursor_key(b'B', app_cursor, parameter)?,
        "right" => cursor_key(b'C', app_cursor, parameter)?,
        "left" => cursor_key(b'D', app_cursor, parameter)?,
        "home" => edge_key(b'H', app_cursor, parameter),
        "end" => edge_key(b'F', app_cursor, parameter),
        "delete" => tilde(3, parameter),
        "insert" => tilde(2, parameter),
        "pageup" => tilde(5, parameter),
        "pagedown" => tilde(6, parameter),
        "f1" => function_key(b'P', 11, parameter),
        "f2" => function_key(b'Q', 12, parameter),
        "f3" => function_key(b'R', 13, parameter),
        "f4" => function_key(b'S', 14, parameter),
        "f5" => tilde(15, parameter),
        "f6" => tilde(17, parameter),
        "f7" => tilde(18, parameter),
        "f8" => tilde(19, parameter),
        "f9" => tilde(20, parameter),
        "f10" => tilde(21, parameter),
        "f11" => tilde(23, parameter),
        "f12" => tilde(24, parameter),
        _ => return None,
    };
    Some(bytes)
}

/// 方向键：程序开了"应用光标"模式且没按修饰键时用 ESC O + 字母，
/// 否则用 CSI——带修饰参数只有 CSI 那种写法有。
fn cursor_key(final_byte: u8, app_cursor: bool, parameter: Option<u8>) -> Option<Vec<u8>> {
    if parameter.is_none() && app_cursor {
        return Some(vec![0x1b, b'O', final_byte]);
    }
    Some(csi(parameter, final_byte))
}

/// Home/End：同上，只是尾字母不同。
fn edge_key(final_byte: u8, app_cursor: bool, parameter: Option<u8>) -> Vec<u8> {
    match (parameter, app_cursor) {
        (None, true) => vec![0x1b, b'O', final_byte],
        (None, false) => vec![0x1b, b'[', final_byte],
        (Some(value), _) => format!("\x1b[1;{value}{}", final_byte as char).into_bytes(),
    }
}

/// F1-F4：平时是 SS3 写法，带修饰键时退回 CSI。
fn function_key(ss3: u8, _number: u8, parameter: Option<u8>) -> Vec<u8> {
    match parameter {
        None => vec![0x1b, b'O', ss3],
        Some(value) => format!("\x1b[1;{value}{}", ss3 as char).into_bytes(),
    }
}

/// `CSI <n> ~` 那一类（Delete、Insert、PageUp/PageDown、F5 以上）。
fn tilde(number: u8, parameter: Option<u8>) -> Vec<u8> {
    match parameter {
        None => format!("\x1b[{number}~").into_bytes(),
        Some(value) => format!("\x1b[{number};{value}~").into_bytes(),
    }
}

fn csi(parameter: Option<u8>, final_byte: u8) -> Vec<u8> {
    match parameter {
        None => vec![0x1b, b'[', final_byte],
        Some(value) => format!("\x1b[1;{value}{}", final_byte as char).into_bytes(),
    }
}

/// xterm 的修饰参数：1 + Shift + 2×Alt + 4×Ctrl。全是 1 就是没按修饰键。
fn modifier_parameter(control: bool, alt: bool, shift: bool) -> Option<u8> {
    let value = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(control);
    (value != 1).then_some(value)
}

/// Ctrl + 字符 → 控制码。只认终端世界里真的有定义的那几个。
fn control_code(character: char) -> Option<u8> {
    let code = match character {
        'a'..='z' => character as u8 - b'a' + 1,
        'A'..='Z' => character as u8 - b'A' + 1,
        '@' | ' ' => 0,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    };
    Some(code)
}

fn with_alt(alt: bool, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

fn single_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(character), None) => Some(character),
        _ => None,
    }
}

fn push_utf8(out: &mut Vec<u8>, character: char) {
    let mut buffer = [0u8; 4];
    out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
}

/// 粘贴一段文本。程序开了括号粘贴模式就包起来，
/// 这样多行文本只会被当成一次输入，而不是一条条命令。
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
    }
    for character in text.chars() {
        if character == '\n' {
            out.push(b'\r');
        } else {
            push_utf8(&mut out, character);
        }
    }
    if bracketed {
        out.extend_from_slice(b"\x1b[201~");
    }
    out
}

/// 起哪个 shell。找不到就退回一个一定在的，绝不返回空。
pub fn shell() -> String {
    #[cfg(windows)]
    {
        // PowerShell 是 Windows 上的日常；pwsh 装了就先用它。
        // 返回找到的完整路径：GUI 进程拿到的 PATH 比终端里短，
        // 光给个名字可能起不来。
        for candidate in ["pwsh", "powershell", "cmd"] {
            if let Some(found) = crate::terminal::find_program(candidate) {
                return found.to_string_lossy().into_owned();
            }
        }
        "cmd".to_string()
    }

    #[cfg(not(windows))]
    {
        // $SHELL 是登录 shell，可能已经不在了（换过 shell、卸载过、容器里没配）。
        if let Some(found) = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|path| path.is_file())
        {
            return found.to_string_lossy().into_owned();
        }
        for candidate in ["/bin/bash", "/bin/zsh", "/bin/sh"] {
            if Path::new(candidate).is_file() {
                return candidate.to_string();
            }
        }
        "sh".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source: &str) -> Keystroke {
        Keystroke::parse(source).expect("按键写得不对")
    }

    fn send(source: &str) -> Vec<u8> {
        encode_key(&key(source), false).unwrap_or_else(|| panic!("{source} 没被送进终端"))
    }

    fn text(source: &str) -> String {
        String::from_utf8(send(source)).expect("发出来的不是 UTF-8")
    }

    // ---- 按键：字符 ----

    #[test]
    fn a_letter_goes_in_as_itself() {
        assert_eq!(text("a"), "a");
        assert_eq!(text("z"), "z");
    }

    #[test]
    fn ctrl_c_is_the_interrupt_byte_not_a_letter() {
        // 这一条是终端的命根子：Ctrl+C 必须是 0x03，不是字符 'c'。
        assert_eq!(send("ctrl-c"), vec![0x03]);
        assert_eq!(send("ctrl-a"), vec![0x01]);
        assert_eq!(send("ctrl-z"), vec![0x1a]);
    }

    #[test]
    fn the_punctuation_control_codes_follow_xterm() {
        assert_eq!(send("ctrl-["), vec![0x1b]);
        assert_eq!(send("ctrl-]"), vec![0x1d]);
        assert_eq!(send("ctrl-_"), vec![0x1f]);
        assert_eq!(send("ctrl-space"), vec![0x00]);
    }

    #[test]
    fn alt_prefixes_an_escape() {
        assert_eq!(send("alt-a"), vec![0x1b, b'a']);
    }

    #[test]
    fn cmd_is_left_to_the_host() {
        // Cmd+C 是复制、Cmd+V 是粘贴。这些不能漏进终端。
        assert_eq!(encode_key(&key("cmd-c"), false), None);
        assert_eq!(encode_key(&key("cmd-alt-t"), false), None);
    }

    // ---- 按键：回车、Tab、退格 ----

    #[test]
    fn enter_is_carriage_return() {
        // 终端的行编辑器认 \r；发 \n 过去光标只会往下走不回车。
        let keystroke = Keystroke {
            modifiers: Modifiers::none(),
            key: "enter".to_string(),
            key_char: Some("\n".to_string()),
        };
        assert_eq!(encode_key(&keystroke, false), Some(vec![b'\r']));
        // 平台没给 key_char 时走命名键那条路，结果必须一样。
        assert_eq!(send("enter"), vec![b'\r']);
    }

    #[test]
    fn tab_is_a_tab_and_shift_tab_goes_backwards() {
        assert_eq!(send("tab"), vec![b'\t']);
        assert_eq!(send("shift-tab"), b"\x1b[Z".to_vec());
    }

    #[test]
    fn backspace_is_delete_and_ctrl_backspace_is_backspace() {
        assert_eq!(send("backspace"), vec![0x7f]);
        assert_eq!(send("ctrl-backspace"), vec![0x08]);
    }

    #[test]
    fn escape_is_a_lone_escape() {
        assert_eq!(send("escape"), vec![0x1b]);
    }

    // ---- 按键：方向键与编辑键 ----

    #[test]
    fn arrows_use_csi_by_default() {
        assert_eq!(text("up"), "\u{1b}[A");
        assert_eq!(text("down"), "\u{1b}[B");
        assert_eq!(text("right"), "\u{1b}[C");
        assert_eq!(text("left"), "\u{1b}[D");
    }

    #[test]
    fn arrows_switch_to_ss3_when_the_program_asks_for_it() {
        // vim/less 会打开应用光标模式，那时候方向键是 ESC O + 字母。
        assert_eq!(
            encode_key(&key("up"), true),
            Some(vec![0x1b, b'O', b'A']),
            "应用光标模式下方向键的写法不一样"
        );
    }

    #[test]
    fn a_modifier_on_an_arrow_comes_along() {
        assert_eq!(text("ctrl-up"), "\u{1b}[1;5A");
        assert_eq!(text("shift-left"), "\u{1b}[1;2D");
        assert_eq!(text("alt-right"), "\u{1b}[1;3C");
    }

    #[test]
    fn home_and_end_follow_the_same_two_dialects() {
        assert_eq!(text("home"), "\u{1b}[H");
        assert_eq!(text("end"), "\u{1b}[F");
        assert_eq!(encode_key(&key("home"), true), Some(b"\x1bOH".to_vec()));
        assert_eq!(text("ctrl-home"), "\u{1b}[1;5H");
    }

    #[test]
    fn delete_insert_and_paging_use_tilde_sequences() {
        assert_eq!(text("delete"), "\u{1b}[3~");
        assert_eq!(text("insert"), "\u{1b}[2~");
        assert_eq!(text("pageup"), "\u{1b}[5~");
        assert_eq!(text("pagedown"), "\u{1b}[6~");
    }

    #[test]
    fn function_keys_keep_their_two_families() {
        assert_eq!(text("f1"), "\u{1b}OP");
        assert_eq!(text("f4"), "\u{1b}OS");
        assert_eq!(text("f5"), "\u{1b}[15~");
        assert_eq!(text("f12"), "\u{1b}[24~");
    }

    #[test]
    fn a_key_we_do_not_know_is_not_sent() {
        assert_eq!(encode_key(&key("f35"), false), None);
    }

    // ---- 粘贴 ----

    #[test]
    fn pasted_newlines_become_carriage_returns() {
        assert_eq!(paste("a\nb", false), b"a\rb".to_vec());
    }

    #[test]
    fn bracketed_paste_wraps_the_text() {
        // 少了这对包裹，多行粘贴会被 shell 当成一条条命令立刻执行。
        assert_eq!(paste("a\nb", true), b"\x1b[200~a\rb\x1b[201~".to_vec());
    }

    // ---- 屏幕 ----

    #[test]
    fn output_shows_up_on_the_screen() {
        let mut screen = Screen::new(4, 20);
        screen.feed(b"hello");
        assert_eq!(screen.lines()[0], "hello");
    }

    #[test]
    fn a_wide_character_occupies_two_columns() {
        let mut screen = Screen::new(2, 10);
        screen.feed("你好".as_bytes());
        let runs = screen.line(0);
        let shown: String = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(shown, "你好");
        // 第二格是宽字符的后半，不能有内容，否则会把字画两遍。
        assert!(screen.line(0).iter().all(|run| !run.text.is_empty()));
    }

    #[test]
    fn ansi_color_comes_through_as_a_face() {
        let mut screen = Screen::new(2, 20);
        screen.feed(b"\x1b[31mred\x1b[0m plain");
        let runs = screen.line(0);
        assert_eq!(runs.len(), 2, "两段颜色不同，不能并成一段：{runs:?}");
        assert_eq!(runs[0].text, "red");
        assert_eq!(runs[0].face.fg, Color::Indexed(1));
        assert_eq!(runs[1].text, " plain");
        assert_eq!(runs[1].face.fg, Color::Default);
    }

    #[test]
    fn resizing_reports_the_new_size() {
        let mut screen = Screen::new(10, 40);
        screen.resize(24, 80);
        assert_eq!(screen.size(), (24, 80));
    }

    #[test]
    fn a_tiny_size_is_refused() {
        // 0 行会把 vt100 和界面一起搞乱，夹到最小尺寸。
        let screen = Screen::new(0, 0);
        assert_eq!(screen.size(), (MIN_ROWS, MIN_COLS));
    }

    #[test]
    fn scrolling_back_sees_older_lines() {
        // 行号补零：不然 "L1" 是 "L10" 的前缀，断言会假通过。
        let mut screen = Screen::new(3, 20);
        for index in 0..30 {
            screen.feed(format!("L{index:02}\r\n").as_bytes());
        }
        assert!(screen.plain().contains("L29"));
        assert!(!screen.plain().contains("L00"));

        screen.scroll_up(10);
        assert_eq!(screen.offset(), 10);
        assert!(
            !screen.plain().contains("L29"),
            "翻上去就不该还看到最新的行"
        );

        // 一路翻到顶：最早那行必须能看见。
        screen.scroll_up(1000);
        assert!(screen.plain().contains("L00"));

        screen.scroll_home();
        assert_eq!(screen.offset(), 0);
        assert!(screen.plain().contains("L29"));
    }

    #[test]
    fn scrolling_past_the_top_stops_there() {
        let mut screen = Screen::new(3, 20);
        screen.scroll_up(500);
        // 没有历史也能翻，只是翻不动；偏移不该变成一个吓人的数字。
        assert_eq!(screen.offset(), 0);
    }

    #[test]
    fn the_cursor_is_hidden_while_reading_history() {
        let mut screen = Screen::new(3, 20);
        for index in 0..10 {
            screen.feed(format!("line {index}\r\n").as_bytes());
        }
        assert!(screen.cursor().is_some());
        screen.scroll_up(3);
        assert_eq!(screen.cursor(), None, "翻着回滚时光标不在可见区里");
    }

    #[test]
    fn the_alternate_screen_is_noticed() {
        let mut screen = Screen::new(3, 20);
        assert!(!screen.alternate_screen());
        screen.feed(b"\x1b[?1049h");
        assert!(screen.alternate_screen());
    }

    #[test]
    fn bracketed_paste_is_reported_from_the_program() {
        let mut screen = Screen::new(3, 20);
        assert!(!screen.bracketed_paste());
        screen.feed(b"\x1b[?2004h");
        assert!(screen.bracketed_paste());
    }

    // ---- 选中 ----

    #[test]
    fn a_selection_covers_a_rectangle_whichever_way_it_was_dragged() {
        // 从右下往左上拖也得是个正经矩形：界面按 from..=to 铺色块，
        // 反过来的话色块会画不出来。
        let selection = Selection::new(Cell { row: 5, col: 7 }, Cell { row: 2, col: 3 });
        let (from, to) = selection.bounds();
        assert_eq!(from, Cell { row: 2, col: 3 });
        assert_eq!(to, Cell { row: 5, col: 7 });
    }

    #[test]
    fn a_click_without_dragging_is_not_a_selection() {
        let cell = Cell { row: 1, col: 1 };
        assert!(Selection::new(cell, cell).is_empty());
    }

    #[test]
    fn selected_text_drops_the_padding_on_the_right() {
        let mut screen = Screen::new(3, 20);
        screen.feed(b"alpha\nbeta");
        let selection = Selection::new(Cell { row: 0, col: 0 }, Cell { row: 1, col: 10 });
        assert_eq!(screen.selected_text(selection), "alpha\nbeta");
    }

    #[test]
    fn the_shell_is_never_empty() {
        assert!(!shell().is_empty());
    }
}
