//! 界面文案：中文原文 → 译文。
//!
//! **为什么 key 就是中文原文，不另编一套 `toolbar.copy` 这样的名字**：
//! 这是 gettext 的做法（msgid 就是源串），对"源语言是中文、只有两种语言"的项目
//! 最省事——
//!
//! - **迁移是机械的**：把 `"复制"` 改成 `tr(cx, "复制")`，一个字符串一处，
//!   抄错不可能发生。而另编一套 key 的话，513 处每个都得新起名字，
//!   起错了编译期发现不了，界面上会冒出 `toolbar.copys` 这种东西。
//! - **漏翻不会变成乱码**：英文表里没有的 key，中文表天然有（key 自己就是中文），
//!   于是缺一条翻译的后果是"那一句还是中文"，而不是界面上出现一个 key。
//! - **只维护一张表**：中文表就是源码，不需要第二份副本去对齐。
//!
//! 代价是改中文措辞要连着改所有引用点。这条代价在只有两种语言时很便宜——
//! 真要加第三种语言时再换 key 方案，那次改动是机械的（一个脚本 + 一次 review）。
//!
//! **为什么不引一个运行时框架**（`fluent` 之类）：八百来条短句、两种语言，
//! 全是短句。框架要多一个依赖、多一层配置格式、多一类
//! "key 拼错了但编译期发现不了"的错误，换来的能力（复数规则、参数插值、
//! 复数语言）一条都用不上。

use std::collections::HashMap;
use std::sync::OnceLock;

/// 界面语言。
///
/// 只两种，不是"无限可扩展"：语言列表每加一种，表里就得有一份完整翻译，
/// 否则那半边界面会是中文。宁可承认只有两种，也不要摆一个空架子。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Locale {
    /// 简体中文。缺译文时的回落目标。
    #[default]
    ZhHans,
    /// 英文。
    En,
}

impl Locale {
    /// 界面上显示的名字。
    ///
    /// **用该语言自己写**，跟设置列表里那条一致：一个人在英文界面里
    /// 找 "English" 比找 "英文（简体）" 容易得多，而中文用户在中文界面里
    /// 看到 "简体中文" 也不需要任何解释。
    pub fn label(self) -> &'static str {
        match self {
            Locale::ZhHans => "简体中文",
            Locale::En => "English",
        }
    }

    /// 稳定标识，存进设置文件。
    pub fn code(self) -> &'static str {
        match self {
            Locale::ZhHans => "zh-Hans",
            Locale::En => "en",
        }
    }

    /// 全部语言，命令面板里列得出来。
    pub const ALL: &'static [Locale] = &[Locale::ZhHans, Locale::En];

    /// 认一个语言标识。认不出来返回 `None`，由调用方决定回落成什么。
    ///
    /// 认得的形式：`en`、`en-US`、`zh`、`zh-Hans`、`zh-CN`、`zh_CN`。
    /// **只比前缀不比整串**：系统给的是 `zh-Hans-CN`、`en-US` 这类完整标签，
    /// 而配置文件是人手写的，两边都得能对上。
    ///
    /// 中文只认 `zh` 系（简繁都落到简体：一个文件管理器分繁简两套文案，
    /// 收益不抵维护成本）；`ja` / `ko` 这类不猜——猜错了比不翻译更糟，
    /// 那会让用户以为界面本来就有日语，只是坏了。
    pub fn parse(code: &str) -> Option<Locale> {
        let normalized = code.trim().replace('_', "-").to_lowercase();
        if normalized.starts_with("zh") {
            return Some(Locale::ZhHans);
        }
        if normalized.starts_with("en") {
            return Some(Locale::En);
        }
        None
    }

    /// 从系统环境推一个出来：`LC_ALL` / `LC_MESSAGES` / `LANG` 里第一个认得的。
    ///
    /// 认不出来就是 `None`（不是硬落到中文）：调用方可能想按别的规则定，
    /// 比如设置里已经选过语言，那就不该被环境变量盖掉。
    pub fn from_env() -> Option<Locale> {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            let Ok(value) = std::env::var(key) else {
                continue;
            };
            // `LANG=zh_CN.UTF-8` 这种带编码后缀的，只取点前面那段。
            let code = value.split('.').next().unwrap_or(&value);
            if let Some(locale) = Locale::parse(code) {
                return Some(locale);
            }
        }
        None
    }

    /// 该语言的译文表，按原文索引。
    ///
    /// **中文不建索引**：查中文就是返回原文，一个哈希都不必碰。
    /// 用 `OnceLock` 而不是每次重建：这一段在**每一帧**的渲染里被调用几百次
    /// （每个按钮、每个表头各一次），线性扫八百条就是每帧几十万次比较。
    fn index(self) -> Option<&'static HashMap<&'static str, &'static str>> {
        static EN_INDEX: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
        match self {
            Locale::ZhHans => None,
            Locale::En => Some(EN_INDEX.get_or_init(|| index_of(EN))),
        }
    }
}

fn index_of(table: &[(&'static str, &'static str)]) -> HashMap<&'static str, &'static str> {
    table.iter().copied().collect()
}

/// 按当前设置的语言查一句文案。
///
/// `cx` 取不到设置时回落中文：那说明启动早期还没装 `Global`，
/// 此时用中文保证界面上有字，而不是退化成空串。
pub fn tr(cx: &gpui_kit::App, text: &str) -> String {
    let locale = crate::settings::Settings::peek(cx, |settings| settings.locale());
    lookup(locale, text)
}

/// 不依赖 `Settings` 的直接查表。测试与默认值用这个。
///
/// **缺译文回落中文原文**——key 本身就是中文，所以"回落"就是把 key 原样返回。
/// 真正找不到（key 不在源码里，那是调用方的 bug）也只能返回 key：
/// 返回空串会让界面上出现一个点了没反应的按钮，而返回原文至少还能读。
pub fn lookup(locale: Locale, text: &str) -> String {
    match locale.index().and_then(|index| index.get(text)) {
        Some(translated) => (*translated).to_string(),
        // 没有译文就是显示原文。中文 locale 恒等落这一支；
        // 英文 locale 也可能落这里——表里没有的句子（漏翻，或者干脆是
        // 一句刚加上还没翻的话）。界面上一句能读的中文，好过一段乱码。
        None => text.to_string(),
    }
}

/// 查一句带占位符的文案，并把 `{name}` 换成实参。
///
/// **占位符必须带名字**（`{name}` 而不是 `{}`）：英文的语序跟中文不同，
/// "已在 foo 中打开 3 项" 英文是 "Opened 3 items in foo"——两个实参得能换位置。
/// 位置不同的代价是每种语言一套拼接代码，而带名字的占位符一套就够。
///
/// 实参是 `String` 而不是 `&str`：值多半是刚 `format!` 出来的，
/// 收 `String` 省掉一次克隆，而这里不在渲染热路径的紧循环里。
pub fn tr_args(cx: &gpui_kit::App, text: &str, args: &[(&str, String)]) -> String {
    fill(&tr(cx, text), args)
}

/// [`tr_args`] 的纯函数内核：在一句已经查好的话里填占位符。
///
/// 认不出来的占位符**原样留着**（`{foo}` 留在结果里）：填错了要在输出里看得见，
/// 悄悄吞掉会让一句"已复制 N 个路径"变成"已复制的路径"。
fn fill(template: &str, args: &[(&str, String)]) -> String {
    if !template.contains('{') {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // 没有配对的 `}`：剩下的原样搬过去，别把句子截断。
            out.push_str(&rest[open..]);
            return out;
        };
        let name = &after[..close];
        match args.iter().find(|(key, _)| *key == name) {
            Some((_, value)) => out.push_str(value),
            None => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// 英文译文表：中文原文 → 英文。
///
/// 键就是中文原文（见模块注释）。查不到的回落原文，所以**漏一条不会崩，
/// 只是那一句还是中文**——这是故意的：宁可显示一种能读的语言，
/// 不要在文件管理器里混进 `tr(cx, "…")` 这种东西。
///
/// **译文跟原文的标点一起改**：中文的「（Cmd+B）」括号，英文得写成 "(Cmd+B)"，
/// 直接搬过去会在英文句子里留一对全角括号，肉眼能看出是没收工。
static EN: &[(&str, &str)] = &[
    // ---- 侧栏 ----
    ("快速、清楚、可撤销", "Fast, clear, undoable"),
    ("收起侧栏（Cmd+B）", "Collapse sidebar (Cmd+B)"),
    ("展开侧栏（Cmd+B）", "Expand sidebar (Cmd+B)"),
    ("收起侧栏", "Collapse sidebar"),
    ("展开侧栏", "Expand sidebar"),
    ("位置", "Places"),
    ("本地文件系统", "Local file system"),
    ("个人文件夹", "Home"),
    ("桌面", "Desktop"),
    ("文稿", "Documents"),
    ("下载", "Downloads"),
    // ---- 命令面板（设置类）----
    // 语言名不翻（`Locale::label` 用该语言自己写），要翻的是它前后那两个字。
    ("界面语言：跟随系统", "Interface language: follow system"),
    ("界面语言：{label}", "Interface language: {label}"),
    ("当前", "Current"),
    ("视图", "View"),
];

/// 英文表里有没有漏掉不该出现的条目：译文等于原文（等于没翻），
/// 或者译文是空的。前者在界面上就是"那句话没变"，后者是"那句话没了"。
#[cfg(test)]
fn suspect_entries() -> Vec<&'static str> {
    EN.iter()
        .filter(|(key, text)| text.is_empty() || *key == *text)
        .map(|(key, _)| *key)
        .collect()
}

/// 命令行 `--lang` 指定的语言。
///
/// **存成进程级的一个值而不是顺着参数往下传**：读它的
/// [`crate::settings::Settings::install`] 在 `WFilesApp::new` 里被调用，
/// 那已经是 `application.run` 闭包深处了——从 `main` 一路穿到那里要动
/// `WFilesApp::new` 的签名，而那是个十几个参数的地方。
/// "启动时定一次"的东西用进程级存储是对的，不是绕过依赖注入。
static CLI_LOCALE: OnceLock<Option<Locale>> = OnceLock::new();

/// 记下命令行指定的语言。`None` = 命令行没提。
///
/// 只能生效一次：`set` 失败说明已经有值了，那就是真的重复调用，
/// 静默忽略跟 `OnceLock` 自己的行为一致。
pub fn set_cli_locale(locale: Option<Locale>) {
    let _ = CLI_LOCALE.set(locale);
}

/// 命令行指定的语言。`main` 在读设置之前调 [`set_cli_locale`]。
///
/// 测试里恒为 `None`——只有 `main` 调 [`set_cli_locale`]，而测试不走 `main`。
/// 正好是 UI 测试需要的：两条 i18n 测试用 `Settings::update` 切语言，
/// 若哪天有测试顺手调了 [`set_cli_locale`]，命令行会盖掉设置，
/// 那两条会一起挂，而且是"改了一处挂在别处"的那种难查。
pub fn cli_locale() -> Option<Locale> {
    CLI_LOCALE.get().copied().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes_round_trip() {
        for locale in [Locale::ZhHans, Locale::En] {
            assert_eq!(
                Locale::parse(locale.code()),
                Some(locale),
                "{:?} 的标识自己解析不回来",
                locale
            );
        }
    }

    #[test]
    fn common_locale_tags_are_recognised() {
        // 系统与配置文件里实际会出现的形式。
        assert_eq!(Locale::parse("en"), Some(Locale::En));
        assert_eq!(Locale::parse("en-US"), Some(Locale::En));
        assert_eq!(Locale::parse("EN_gb"), Some(Locale::En));
        assert_eq!(Locale::parse("zh"), Some(Locale::ZhHans));
        assert_eq!(Locale::parse("zh-CN"), Some(Locale::ZhHans));
        assert_eq!(Locale::parse("zh_Hans_CN"), Some(Locale::ZhHans));
        // 繁体也落简体：分繁简两套文案收益不抵维护成本。
        assert_eq!(Locale::parse("zh-TW"), Some(Locale::ZhHans));
        // 认不出来就老实说认不出来，不猜。
        assert_eq!(Locale::parse("ja"), None);
        assert_eq!(Locale::parse(""), None);
        assert_eq!(Locale::parse("   "), None);
    }

    #[test]
    fn chinese_is_the_identity_lookup() {
        // 中文不需要表：查不到就是原文。
        assert_eq!(lookup(Locale::ZhHans, "复制"), "复制");
        assert_eq!(
            lookup(Locale::ZhHans, "任何没翻过的句子"),
            "任何没翻过的句子"
        );
    }

    #[test]
    fn a_missing_translation_shows_chinese_rather_than_the_key() {
        // 英文表里没有的句子回落中文原文——界面上一句能读的中文，
        // 好过一段 `tr(cx, …)` 形状的乱码。
        assert_eq!(lookup(Locale::En, "这一句还没翻"), "这一句还没翻");
    }

    #[test]
    fn every_translation_is_actually_translated() {
        assert!(
            suspect_entries().is_empty(),
            "英文表里有等于原文或空串的条目：{:?}",
            suspect_entries()
        );
    }

    #[test]
    fn named_arguments_are_filled_in_and_the_order_is_the_translation_s() {
        // 英文把两个实参换了位置——这正是不用 `{}` 顺序占位符的理由。
        assert_eq!(
            fill(
                "已在{name}中打开 {count}",
                &[("name", "foo".into()), ("count", "3".into())]
            ),
            "已在foo中打开 3"
        );
        assert_eq!(
            fill(
                "Opened {count} items in {name}",
                &[("name", "foo".into()), ("count", "3".into())]
            ),
            "Opened 3 items in foo"
        );
    }

    #[test]
    fn an_unknown_placeholder_stays_visible() {
        // 填错名字要在输出里看得见，不能悄悄吞掉变成一句缺字的病话。
        assert_eq!(
            fill("撤销：{label}（Cmd+Z）", &[("titel", "复制".into())]),
            "撤销：{label}（Cmd+Z）"
        );
        // 没有配对的 `}`：整段原样搬过去，不截断。
        assert_eq!(
            fill("未闭合 {label", &[("label", "复制".into())]),
            "未闭合 {label"
        );
        assert_eq!(fill("没有占位符", &[]), "没有占位符");
    }
}
