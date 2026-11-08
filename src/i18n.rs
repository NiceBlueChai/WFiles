//! 界面层的 i18n 适配：把 core 的纯翻译表接上 GPUI 的当前语言。
//!
//! `wfiles-core::i18n` 里是翻译表、`Message`、`lookup` / `fill` / `say_in` /
//! `tr_in` / `tr_args_in` 这些**不需要界面上下文**的纯函数；这一层只补三件
//! 需要 `cx` 读 `Settings` 全局槽位的事：
//!
//! | 界面层补的 | core 里的纯版本 |
//! |---|---|
//! | `tr(cx, text)` | `lookup(locale, text)` |
//! | `tr_args(cx, text, args)` | `tr_args_in(locale, text, args)` |
//! | `Message::say(cx)`（`Say` 扩展 trait） | `Message::say_in(locale)` |
//!
//! 于是 `crate::i18n::Msg` / `crate::i18n::Locale` / `crate::i18n::tr(cx, …)`
//! 这些既有路径一个字都不用改，TUI 之类的新前端直接用 core 的纯版本即可。

pub use wfiles_core::i18n::*;

use crate::settings::Settings;

/// 按当前设置的语言查一句文案。
///
/// `cx` 取不到设置时回落中文：那说明启动早期还没装 `Global`，
/// 此时用中文保证界面上有字，而不是退化成空串。
pub fn tr(cx: &gpui_kit::App, text: &str) -> String {
    let locale = Settings::peek(cx, |settings| settings.locale());
    lookup(locale, text)
}

/// 查一句带占位符的文案，并把 `{name}` 换成实参。
pub fn tr_args(cx: &gpui_kit::App, text: &str, args: &[(&str, String)]) -> String {
    let locale = Settings::peek(cx, |settings| settings.locale());
    tr_args_in(locale, text, args)
}

/// `Message::say(cx)` 的界面层版本。
///
/// `Message` 住在 core 里，外部 crate 不能给它加 inherent method，所以用扩展
/// trait。调用它的文件加一句 `use crate::i18n::Say as _;` 即可，调用点不变。
pub trait Say {
    fn say(&self, cx: &gpui_kit::App) -> String;
}

impl Say for wfiles_core::i18n::Message {
    fn say(&self, cx: &gpui_kit::App) -> String {
        let locale = Settings::peek(cx, |settings| settings.locale());
        self.say_in(locale)
    }
}
