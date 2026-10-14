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

    /// 同一句话里几段之间的分隔符。
    ///
    /// 写死「，」的话，英文界面里会出现 `Copied 5 items，skipped 2 items`
    /// ——全角逗号夹在两个英文句子中间，一眼就看出是没收工的。
    fn clause_joiner(self) -> &'static str {
        match self {
            // 中文用全角逗号且不留空格；英文用半角逗号加一个空格。
            Locale::ZhHans => "，",
            Locale::En => ", ",
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

/// 一句**还没决定用什么语言说**的文案。
///
/// **为什么需要它**：错误信息在很深的地方产生（`ops::valid_name`、
/// `archive::create`、`query::parse`…），那些函数签名是
/// `Result<T, crate::i18n::Msg>`，既拿不到 `cx` 也不知道用户选了哪种语言。
/// 三个走过的路子：
///
/// | 做法 | 为什么不行|
/// |---|---|
/// | 在错误产生处就 `format!` 成中文 | 上屏时只有一坨成品字符串，查表无从下手——key 已经被拼没了 |
/// | 给每个函数加 `locale: Locale` 参数 | 参数会沿调用链传得很深（`ops_entity` → `ops` → `trash::os_limited` → …），而且那些函数本来就是纯逻辑，单测得为一个参数造一个 locale |
/// | **产生 `Message`，上屏时才翻** ✅ | 语言这个决定推迟到最后一刻，恰好是唯一知道语言的地方 |
///
/// `Display` 出来是中文（`tr_args_in(ZhHans, …)`），所以单测里
/// `assert_eq!(err.to_string(), "名称不能为空")` 照样成立——
///
/// ```ignore
/// OpsError::msg("名称不能为空")
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    /// 英文表里的 key，中文原文。空串 = 不是我们自己的措辞，看 `verbatim`。
    key: &'static str,
    /// 带名字的占位符实参。**都是字符串，不含内层消息**——
    /// 内层消息走 [`Self::cause`]。
    args: Vec<(&'static str, String)>,
    /// **别人说的话，原样显示**：IO 错误、远端服务器返回的消息、
    /// 协议报文的正文。查了表也白查——那句英文是 NAS 说的，
    /// 我们翻了它，用户排查时反而认不出这是谁讲的。
    verbatim: Option<String>,
    /// 内层消息，渲染时填进外层的 `{error}`。
    ///
    /// **为什么不给 `args` 加一条能装 `Message` 的实参**：那样就得在构造时
    /// 把它 `to_string()` 掉，而 `Display` 恒给中文——于是
    /// 「下载：写盘失败：{error}」包着「已中止」，等上屏时内层已经是
    /// 中文死符。两个后果：英文界面里漏中文；更糟的是内层的 key 没了，
    /// [`Self::is_aborted`] 认不出中止，那一趟被当成真失败。
    ///
    /// 留在结构里、内层自己到上屏那一刻才渲染，这两件事才都成立。
    cause: Option<Box<Message>>,
    /// 主句后面的补充句，每句各自查表。
    ///
    /// **为的是什么形状的问题**：状态栏那句「拷贝完成 5 项，跳过 2 项，
    /// 1 项失败（a.txt：Permission denied）」有两个麻烦——
    ///
    /// 1. **子句是条件出现**的（没有失败就不显示那半句），所以不能
    ///    把它塞进一条 key 的占位符：填不出来时那句还挂在界面上。
    /// 2. **英文的动词是过去式**（移动完成 → Moved），而子句是分词短语
    ///    （skipped 2 / 1 failed）。两种语言的语序不同，硬编成一条 key
    ///    就得给 2×2×2 种组合各留一条——组合一多，漏一条就是一个只在
    ///    某一种操作下才露出来的中英混排。
    ///
    /// 所以拆开：主句一条 key，子句各一条 key，渲染时按当前语言
    /// 各自查表再用该语言的分句符接起来。
    /// 分句符不存这儿，渲染时问 [`Locale::clause_joiner`] 要：
    /// 它是标点不是文案，跟着语言走，不该由消息自己定。
    clauses: Vec<Message>,
}

impl Message {
    /// 一句不带参数的文案。
    pub fn msg(key: &'static str) -> Self {
        Self {
            key,
            args: Vec::new(),
            verbatim: None,
            cause: None,
            clauses: Vec::new(),
        }
    }

    /// 一句带占位符的文案。实参按给的顺序存，不影响：
    /// 英文的语序由译文自己定，实参对不上号才要紧。
    pub fn args(key: &'static str, args: &[(&'static str, String)]) -> Self {
        Self {
            key,
            args: args.to_vec(),
            verbatim: None,
            cause: None,
            clauses: Vec::new(),
        }
    }

    /// **透传的原文**：IO 错误、远端服务器返回的消息、协议报文的正文
    /// 都走这条路。
    pub fn verbatim(text: impl Into<String>) -> Self {
        Self {
            key: "",
            args: Vec::new(),
            verbatim: Some(text.into()),
            cause: None,
            clauses: Vec::new(),
        }
    }

    /// 在一句自己的措辞里**包住另一个错误**：`key` 里的 `{error}`
    /// 拿 `cause` 填，`key` 里没有别的占位符。
    ///
    /// 这是链式错误的正路：`explain` 之类一层层往上包，每层说清
    /// 「当时在干什么」，内层那句负责说「到底哪儿错了」。内层保持
    /// [`Message`] 而不是提前 `to_string()`，理由见 [`Self::cause`]。
    ///
    /// ```ignore
    /// Message::caused("下载：写盘失败：{error}", io_error)
    /// ```
    pub fn caused(key: &'static str, cause: impl Cause) -> Self {
        Self::caused_with(key, &[], cause)
    }

    /// [`Self::caused`] 加上别的占位符（「建不了 {name}：{error}」那种）。
    ///
    /// `args` 里**不要**再放 `"error"`——它由 `cause` 填，
    /// 放了也是被 [`Self::cause`] 覆盖掉，白写一遍。
    pub fn caused_with(
        key: &'static str,
        args: &[(&'static str, String)],
        cause: impl Cause,
    ) -> Self {
        Self {
            key,
            args: args.to_vec(),
            verbatim: None,
            cause: Some(Box::new(cause.into_message())),
            clauses: Vec::new(),
        }
    }

    /// 在主句后面**挂一句补充**：渲染时按语言各自查表，
    /// 再用该语言的分句符接起来。
    ///
    /// 典型是状态栏那句「拷贝完成 5 项，跳过 2 项」——「跳过 N 项」
    /// 只在真跳过了的时候出现，塞进主句的占位符里会出现
    /// 「拷贝完成 5 项，跳过  项」（占位符留着还是被填成空，
    /// 两种都不对）。用整句查表也有同样的毛病：得为
    /// 「有/无跳过 × 有/无失败 × 移动/拷贝」留 8 条 key，
    /// 漏一条就只在某一种操作下露出来。
    ///
    /// 子句**不带前导标点**——分句符由 [`Locale::clause_joiner`] 给，
    /// 中文和英文的标点不一样。
    ///
    /// ```ignore
    /// Message::msg("拷贝完成 5 项")
    ///     .then_clause(Message::args("跳过 {count} 项", &[("count", "2".into())]))
    /// ```
    pub fn then_clause(mut self, clause: impl Cause) -> Self {
        self.clauses.push(clause.into_message());
        self
    }

    /// 挂一句包着别的错误的补充（「1 项失败（a.txt：权限不足）」那种）。
    ///
    /// 跟 [`Self::then_clause`] 一样的形状，只是内层也是一条 `Msg`，
    /// 所以走 [`Self::caused_with`] 那条路——**不能**写成
    /// `Message::args("…（{error}）", error.to_string())`，那会把内层
    /// 冻成中文死符。
    pub fn then_caused_clause(
        self,
        key: &'static str,
        args: &[(&'static str, String)],
        cause: impl Cause,
    ) -> Self {
        self.then_clause(Message::caused_with(key, args, cause))
    }

    /// 这句话是不是透传的原文（不是我们自己的措辞）。
    pub fn is_verbatim(&self) -> bool {
        self.verbatim.is_some()
    }

    /// 用户按了中止。
    ///
    /// 走 [`Self::msg`] 而不是 `verbatim`：中止是我们自己的措辞，
    /// 而且判据（[`Self::is_aborted`]）全靠它。
    pub fn aborted() -> Self {
        Self::msg(ABORTED)
    }

    /// **这句话是不是"用户叫停了这一趟"**。
    ///
    /// **一层层往内看**：中止常常被包在好几层措辞里
    /// （「下载：写盘失败：已中止」），只看最外层的 key 认不出来。
    ///
    /// 单独给一个方法而不是让调用点写 `is("已中止")`：那个字面量
    /// 在三个文件里各写一遍，改文案就断了一处，而断掉的地方不报编译错误
    /// ——它只是安静地永远返回 false，于是「被叫停」被当成「真失败」。
    pub fn is_aborted(&self) -> bool {
        self.key == ABORTED || self.cause.as_deref().is_some_and(Message::is_aborted)
    }

    /// **这句话是不是某个 key**：给拿文案当判据的地方用。
    ///
    /// 存在的理由是 `ops.rs` 里有一处
    /// `if error.contains("已中止")` ——那是在判"这一趟是被用户叫停的"。
    /// 拿显示文案当条件，在切到英文之后那句判据就永远不成立。
    /// 所以比的是 [`Self::key`]，跟语言无关。
    pub fn is(&self, key: &str) -> bool {
        self.key == key
    }

    /// 按当前语言说这句话。
    pub fn say(&self, cx: &gpui_kit::App) -> String {
        let locale = crate::settings::Settings::peek(cx, |settings| settings.locale());
        self.say_in(locale)
    }

    /// 按指定语言说这句话（纯函数版，给单测与没有 `cx` 的地方用）。
    ///
    /// **内层跟着外层一起翻**：内层是 [`Self::cause`]，
    /// 到这一刻才渲染，所以英文界面里不会出现
    /// 「Download: write failed: 已中止」这种半中半英。
    pub fn say_in(&self, locale: Locale) -> String {
        let head = match &self.verbatim {
            Some(text) => text.clone(),
            None => match &self.cause {
                Some(cause) => {
                    let mut args = self.args.clone();
                    args.push(("error", cause.say_in(locale)));
                    fill(&lookup(locale, self.key), &args)
                }
                None => fill(&lookup(locale, self.key), &self.args),
            },
        };
        if self.clauses.is_empty() {
            return head;
        }
        let joiner = locale.clause_joiner();
        let mut out = head;
        for clause in &self.clauses {
            out.push_str(joiner);
            out.push_str(&clause.say_in(locale));
        }
        out
    }
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // **`Display` 恒给中文**：它存在的意义是"还没上屏的中间态
        // 有个能看的字符串"（`Err` 的错误类型、`eprintln!("{msg}")`）。
        // 上屏处一律走 `say` / `say_in`——那是唯一知道语言的地方。
        f.write_str(&self.say_in(Locale::ZhHans))
    }
}

impl std::error::Error for Message {}

/// 能当 [`Message::cause`] 的东西。
///
/// **为什么不直接用 `impl Into<Message>`**：那样每个第三方错误类型
/// （`ZipError`、`russh::Error`、`suppaftp::FtpError`、`regex::Error`…）
/// 都得写一条 `From`，而它们一共 6 类、分布在 4 个 crate 里——
/// `i18n` 得反过来依赖它们。
///
/// **为什么不能给所有 `Display` 都套 blanket impl**：`Message` 自己
/// 也实现了 `Display`（给 `eprintln!` 和中间态用），泛型 impl 会跟
/// `impl Cause for Message` 撞上，只能靠 negative impl 挡——那更绕。
///
/// 于是只给三类做 impl，其余的（上面那 6 种第三方错误）在调用点
/// `.to_string()`。对它们来说那是**唯一正确的处理**：那些句子是
/// 第三方库说的，我们翻它只会让用户认不出来源。
pub trait Cause {
    fn into_message(self) -> Message;
}

impl Cause for Message {
    fn into_message(self) -> Message {
        self
    }
}

impl Cause for String {
    fn into_message(self) -> Message {
        Message::verbatim(self)
    }
}

impl Cause for &str {
    fn into_message(self) -> Message {
        Message::verbatim(self)
    }
}

impl Cause for std::io::Error {
    /// 走 [`From<std::io::Error>`]：认 [`Aborted`] 哨兵。
    fn into_message(self) -> Message {
        self.into()
    }
}

/// 用户中止的 key。**唯一一份**，别在别处再写一遍字面量。
pub const ABORTED: &str = "已中止";

/// 「用户按了中止」这个哨兵的类型。
///
/// **为什么哨兵要带类型而不是只带一句文案**：搬运链上有个 [`std::io::Read`]
/// 的包装（`ops::Reporting`）在回调说停时只能递 `io::Error`——
/// trait 的返回类型改不了。那条错误一路冒到上层，路上要经过
/// `From<io::Error> for Message`，而那一步默认把错误当**别人的话**原样透传，
/// 于是 key 就丢了；等上层判 `is_aborted()` 时，它对着一个空 key 永远返回
/// false——「用户叫停」被当成「真失败」。
///
/// 认类型的办法是 `downcast_ref`：错误里带一个我们自己的类型，
/// 透传那一步就认得出「这句是我们自己造的」，把它还原成带 key 的 `Message`。
/// 认文案（`text == "已中止"`）也能通，但那跟当初用 `contains("已中止")`
/// 是同一类毛病：把判据绑在一句能改、能翻译的字面上。
#[derive(Debug)]
pub struct Aborted;

impl std::fmt::Display for Aborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(ABORTED)
    }
}

impl std::error::Error for Aborted {}

/// [`Message`] 的别名：给错误签名用，写起来短一点。
pub type Msg = Message;

impl From<Message> for String {
    /// **降级成中文原文**：给还没改成 `Msg` 的旧调用点用
    /// （`Result<_, crate::i18n::Msg>` 的老签名里 `?` 一下就过去了）。
    /// 那些点上屏时还是中文——这正是"改了一半"的样子，
    /// 所以新写的代码别用这条路，直接把签名改成 `Result<_, Msg>`。
    fn from(message: Message) -> String {
        message.to_string()
    }
}

impl From<String> for Message {
    /// **透传的原文**：IO 错误、远端服务器返回的消息、协议报文的正文
    /// 都走这条路。它们是**别人说的话**，不该被我们翻——
    /// 把「Address has no host name」（NAS 那边返回的）翻成中文，
    /// 用户排查时反而认不出这是谁说的。
    fn from(text: String) -> Self {
        Self::verbatim(text)
    }
}

impl From<&str> for Message {
    fn from(text: &str) -> Self {
        Self::verbatim(text.to_string())
    }
}

impl From<std::io::Error> for Message {
    /// IO 错误**原样透传**：`fs::rename` 之类返回的东西里有
    /// "Permission denied"、"No such file or directory"，
    /// 那是操作系统的话，我们翻了它用户反而认不出。
    /// 想加自己的措辞就在调用点包一层 `Message::args("改名失败：{error}", …)`。
    ///
    /// **例外是 [`Aborted`]**：那句话是我们自己造的（`Reporting` 只能递
    /// `io::Error`），不是操作系统的抱怨。认它，把它还原成带 key 的 `Message`，
    /// 否则上层判「这一趟是不是被叫停的」永远不成立。
    fn from(error: std::io::Error) -> Self {
        if error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<Aborted>())
            .is_some()
        {
            return Self::aborted();
        }
        Self::verbatim(error.to_string())
    }
}

/// [`tr`] 的纯函数版：语言已经解析好了，直接查。
///
/// 给那些**拿到的是别人的 `cx`** 的地方用——最常见的是
/// `cx.new(|cx| …)` 闭包里，那个 `cx` 是新建实体的上下文，
/// 拿它 `tr()` 读到的是空设置，界面上一律回落中文。写
/// `let locale = …; cx.new(|cx| tr_in(locale, "…"))` 就绕开了。
pub fn tr_in(locale: Locale, text: &str) -> String {
    lookup(locale, text)
}

/// [`tr_args`] 的纯函数内核，**只查表不填占位符**。
///
/// 给那些"要被单测验文案"的纯函数用：它们没有 `cx`（`cx` 的存在意义就是
/// 从设置里读语言，纯函数读不到），但仍然不该绕开查表——
/// 直接写死中文会让"英文界面里一条中文都不剩"这条判据把它们漏过去。
/// 典型用法见 `pane::MultiSelectionSummary::heading`。
pub fn tr_args_in(locale: Locale, text: &str, args: &[(&str, String)]) -> String {
    fill(&lookup(locale, text), args)
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
    ("文件", "Files"),
    ("快捷键", "Shortcuts"),
    ("改键：{title}", "Rebind: {title}"),
    // ---- 命令面板（文件操作）----
    ("新建文件夹", "New Folder"),
    ("重命名", "Rename"),
    ("批量重命名", "Bulk Rename"),
    ("移到回收站", "Move to Trash"),
    ("拷贝选中项", "Copy"),
    ("剪切选中项", "Cut"),
    ("粘贴到当前目录", "Paste Here"),
    ("撤销上一步操作", "Undo Last Action"),
    ("刷新当前目录", "Refresh Folder"),
    ("前往上一级", "Go Up"),
    ("复制路径", "Copy Path"),
    ("复制文件名", "Copy File Name"),
    ("在这里打开终端", "Open Terminal Here"),
    ("快速查看", "Quick Look"),
    ("搜索当前目录", "Search Folder"),
    ("加标签 / 标颜色", "Tag / Colour"),
    ("查看压缩包内容", "View Archive"),
    ("打包成 Zip", "Create Zip"),
    ("编辑远程文件", "Edit Remote File"),
    ("列表视图", "List View"),
    ("网格视图", "Grid View"),
    ("分列视图", "Columns View"),
    (
        "分析：大小 / 重复 / 校验和",
        "Analyse: Size / Duplicates / Checksums",
    ),
    // ---- 命令面板（应用级命令）----
    // 分组名。英文比中文长，所以面板在英文下右侧留白会多一些——
    // 这没关系，分组标题本来就是拿来扫一眼的。
    ("窗口", "Window"),
    ("分栏显示", "Split Panes"),
    ("取消分栏", "Unsplit"),
    ("标签页", "Tab"),
    ("终端", "Terminal"),
    ("传输", "Transfer"),
    ("远程", "Remote"),
    ("元数据列", "Metadata Column"),
    ("前往", "Go"),
    ("保存的搜索", "Saved Searches"),
    // 命令标题。快捷键一律不译（Cmd+Shift+N 三平台一个样）。
    ("切换到另一栏", "Switch Pane"),
    ("双栏同步浏览", "Sync Browsing"),
    ("关闭双栏同步浏览", "Stop Sync Browsing"),
    ("拷贝到另一栏：{partner}", "Copy to {partner}"),
    ("移动到另一栏：{partner}", "Move to {partner}"),
    ("收起详情板", "Hide Details"),
    ("展开详情板", "Show Details"),
    ("收起终端面板", "Hide Terminal Panel"),
    ("打开终端面板", "Show Terminal Panel"),
    ("比较这两个文件夹", "Compare These Folders"),
    (
        "比较文件内容：{left} ↔ {right}",
        "Compare Contents: {left} ↔ {right}",
    ),
    ("不显示元数据列", "Hide Metadata Column"),
    ("元数据列：{label}", "Metadata: {label}"),
    ("切换到标签页：{label}", "Switch to Tab: {label}"),
    ("新建标签页", "New Tab"),
    ("关闭标签页", "Close Tab"),
    ("下一个标签页", "Next Tab"),
    ("前往{label}", "Go to {label}"),
    ("连接远程服务器…", "Connect to Server…"),
    ("默认终端：跟随系统", "Default Terminal: System Default"),
    ("默认终端：{name}", "Default Terminal: {name}"),
    ("传输限速：{label}", "Transfer Speed: {label}"),
    ("不限速", "No limit"),
    // ---- 嵌在占位符里的值（这些是"值"不是模板，但同样要翻）----
    ("Git 状态", "Git Status"),
    ("图片尺寸", "Image Size"),
    ("媒体时长", "Duration"),
    ("PDF 页数", "PDF Pages"),
    // ---- 窗格：地址栏与工具条----
    ("后退", "Back"),
    ("前进", "Forward"),
    ("刷新", "Refresh"),
    ("更多操作", "More Actions"),
    ("新建文件夹（Cmd+Shift+N）", "New Folder (Cmd+Shift+N)"),
    ("拷贝（Cmd+C）", "Copy (Cmd+C)"),
    ("剪切（Cmd+X）", "Cut (Cmd+X)"),
    ("粘贴（Cmd+V）", "Paste (Cmd+V)"),
    ("筛选当前文件夹", "Filter this folder"),
    // ---- 窗格：列表表头与排序 ----
    ("修改时间", "Modified"),
    ("升序", "ascending"),
    ("降序", "descending"),
    ("按{key}{order}", "by {key} {order}"),
    // ---- 窗格：详情面板 ----
    ("详细信息", "Details"),
    ("修改", "Modified"),
    ("标记", "Tags"),
    ("云盘", "Cloud"),
    ("选择一个项目查看详情", "Select an item to see its details"),
    (
        "批量操作会按列表顺序逐个应用。",
        "Batch actions apply to items in list order.",
    ),
    // ---- 窗格：空态 ----
    ("这个文件夹是空的", "This folder is empty"),
    ("没有匹配的项目", "No matching items"),
    // ---- 窗格：状态栏 ----
    ("{total} 个项目", "{total} items"),
    ("{visible} / {total} 个项目", "{visible} of {total} items"),
    ("，已选中 {count} 项", ", {count} selected"),
    ("远程位置 · {text}", "Remote · {text}"),
    ("远程位置 · {sub} · {text}", "Remote · {sub} · {text}"),
    (
        "连不上服务器，显示 {ago} 列的这份",
        "Can't reach server, showing the copy from {ago}",
    ),
    ("无法打开：{error}", "Can't open: {error}"),
    // ---- 窗格：标签与标记 ----
    (
        "点一下标色，再点一下去掉",
        "Click to colour, click again to remove",
    ),
    ("清除标记", "Clear Tags"),
    ("标签，空格分开", "Tags, separated by spaces"),
    ("用过的标签：{tags}", "Used tags: {tags}"),
    ("完成", "Done"),
    // ---- 窗格：改键浮层 ----
    ("给「{title}」按个键", "Press a key for “{title}”"),
    ("现在是：{current}", "Now: {current}"),
    ("现在是：没有按键", "Now: no key"),
    (
        "退格 = 恢复出厂，Esc = 取消",
        "Backspace = factory reset, Esc = cancel",
    ),
    ("{title} 已恢复出厂按键", "{title} reset to default"),
    (
        "{title} 改为 {key}，重启后生效",
        "{title} is now {key} (takes effect after restart)",
    ),
    // ---- 窗格：压缩包 ----
    ("{files} 个文件 · 共 {total}", "{files} files · {total}"),
    ("这个包是空的", "This archive is empty"),
    ("解压到「{stem}」", "Extract to “{stem}”"),
    ("{target}：{count} 个文件", "{target}: {count} files"),
    ("归档", "Archive"),
    ("正在打包 {name}…", "Creating {name}…"),
    ("正在解压 {name}…", "Extracting {name}…"),
    // ---- 窗格：远程与复制 ----
    ("先选中一个远程文件", "Select a remote file first"),
    ("已在{name}中打开 {path}", "Opened {path} in {name}"),
    ("已复制 {count} 个路径", "Copied {count} paths"),
    ("已复制 {count} 个文件名", "Copied {count} file names"),
    // ---- 窗格：其它 ----
    // 拖动卡片与拖放状态栏。`{action}` 是"移动/拷贝"那对动词
    // （`DropAction::label`），它跟文件操作那条线上的是同一个词。
    ("{count} 个项目", "{count} items"),
    (
        "{action} {count} 项到 {target}",
        "{action} {count} items to {target}",
    ),
    ("移动", "Move"),
    ("拷贝", "Copy"),
    // 三种视图的名字（`ViewMode::label`，不带"视图"两个字，
    // 因为要拼进"{view}视图（Cmd+1）"里）。
    ("列表", "List"),
    ("网格", "Grid"),
    ("分列", "Columns"),
    ("{view}视图（{shortcut}）", "{view} view ({shortcut})"),
    // 详情板多选态
    ("已选中 {count} 项", "{count} selected"),
    ("…… 还有 {rest} 项", "… and {rest} more"),
    // 网格视图里目录那一格的图注
    ("文件夹", "Folder"),
    ("上一级", "Up"),
    ("回到上一级", "Go Up"),
    ("关闭（Esc）", "Close (Esc)"),
    ("收起详情板（Cmd+Alt+O）", "Hide Details (Cmd+Alt+O)"),
    ("展开详情板（Cmd+Alt+O）", "Show Details (Cmd+Alt+O)"),
    ("正在取这个目录…", "Reading folder…"),
    // ---- 侧栏的三个分区标题 ----
    // 它们是 `render_volumes_section` / `render_cloud_section` /
    // `render_remote_section` 里那三行 `.child("…")`，字号一样、位置一样，
    // 英文下面对不齐会很显眼，所以放在一组。
    ("磁盘", "Disks"),
    // ---- 连接入口（侧栏「远程」那一节下面那一行）----
    ("连接服务器", "Connect to Server"),
    ("连接服务器…", "Connect to Server…"),
    // ---- 命令面板浮层 ----
    // 占位符要翻：它就在输入框里，用户一直在看。
    ("搜索命令、位置或标签页", "Search commands, places, or tabs"),
    ("没有匹配的命令", "No matching command"),
    // 底部那排按键提示。`↑↓` / `Enter` / `Esc` 是**键名本身**，不翻；
    // 翻的是它后面那个动作词。
    ("↑↓ 选择", "↑↓ to select"),
    ("Enter 执行", "Enter to run"),
    ("Esc 关闭", "Esc to close"),
    // ---- 标签栏与状态栏 ----
    ("新建标签页（Cmd+T）", "New Tab (Cmd+T)"),
    ("命令面板（Cmd+P）", "Command Palette (Cmd+P)"),
    ("已暂停", "Paused"),
    ("中止", "Cancel"),
    (
        "{count} 个远程文件编辑中（保存即回传）",
        "{count} remote files being edited (saved back on save)",
    ),
    // ---- 对话框的公共按钮 ----
    // "取消"在确认框、冲突框、输入框、批量重命名、连接框里各有一处，
    // 全部走这一个 key——五个地方各写一份，迟早有一处漏翻。
    ("取消", "Cancel"),
    // ---- 冲突对话框（四个策略）----
    // 这四个词是**策略名**，用户要靠它们判断自己的文件会怎样，
    // 不能随手译得含糊。"保留两者"是两个文件名都留下（自动加后缀）。
    ("保留两者", "Keep Both"),
    ("替换", "Replace"),
    ("跳过", "Skip"),
    (
        "选择会应用到本次操作的剩余全部项目。",
        "This choice applies to all remaining items in this operation.",
    ),
    // ---- 批量重命名对话框 ----
    ("名称冲突", "Name conflict"),
    ("无变化", "No change"),
    ("将执行", "Will run"),
    (
        "占位符：{name} 原主干 · {ext} 原扩展名（含点）· {n} 序号",
        "Placeholders: {name} stem · {ext} extension (with dot) · {n} number",
    ),
    (
        "共 {total} 项，其中 {executable} 项会被执行",
        "{total} items, {executable} of them will run",
    ),
    ("执行重命名", "Rename"),
    // 输入框占位符。
    ("名称", "Name"),
    // ---- 连接对话框 ----
    ("协议", "Protocol"),
    ("常用服务", "Common Providers"),
    // 「坚果云」是**一家公司的名字**（Nutstore），不翻成 "Nutstore Cloud"：
    // 它在中文界面里也是这三个字，用户按这三个字认它。
    ("坚果云", "坚果云"),
    ("正在连接…", "Connecting…"),
    ("连接", "Connect"),
    // ---- 侧栏的"位置"（`places()` 的第四项是路径，不翻）----
    // 终端名翻不了：`Option::name` 用来在面板里显示，但它同时是**用户装的那个
    // 程序的名字**——Windows 上的"Windows Terminal"、Linux 上的"GNOME 终端"
    // 各有各的本地化版本，这里硬编码一份英文只会跟系统对不上。
    // 而 App Store / Homebrew 那些名字本身就该按原样显示（iTerm2 就是 iTerm2）。

    // ---- 错误信息 ----
    //
    // 这一批全是**错误**，跟上面那些操作项不一样的地方有两点：
    //
    // 1. 一半是自己造的措辞（翻），一半是别人说的话（不翻，见 `Message::verbatim`）。
    // 2. 键里带占位符——`{error}` 是内层那句，`caused` 的 `cause` 字段负责填。
    //    英文的语序跟中文不同，所以每条译文都得自己决定 `{error}` 放哪儿。
    ("已中止", "Stopped"),
    ("落不了盘：{error}", "Couldn't save to disk: {error}"),
    (
        "源和目标在同一个文件上",
        "Source and target are the same file",
    ),
    (
        "没有父目录，无法重命名",
        "No parent directory, can't rename",
    ),
    // ---- 名字与路径 ----
    ("名称不能为空", "Name can't be empty"),
    (
        "名称不能包含路径分隔符",
        "Name can't contain a path separator",
    ),
    ("名称为空", "Name is empty"),
    ("名称非法", "Name is not allowed"),
    ("“{trimmed}”已存在", "“{trimmed}” already exists"),
    (
        "无法创建文件夹：{error}",
        "Couldn't create the folder: {error}",
    ),
    ("重命名失败：{error}", "Rename failed: {error}"),
    (
        "已上传但删除本地源失败：{error}",
        "Uploaded, but deleting the local source failed: {error}",
    ),
    (
        "已拷贝但删除源文件失败：{error}",
        "Copied, but deleting the source failed: {error}",
    ),
    (
        "读不了源文件：{error}",
        "Couldn't read the source file: {error}",
    ),
    ("读不了：{error}", "Couldn't read: {error}"),
    ("删除失败：{error}", "Delete failed: {error}"),
    ("删目录失败：{error}", "Couldn't delete the folder: {error}"),
    ("上传失败：{error}", "Upload failed: {error}"),
    ("下载失败：{error}", "Download failed: {error}"),
    ("续传失败：{error}", "Resuming failed: {error}"),
    (
        "回传失败：{error}",
        "Uploading the changes back failed: {error}",
    ),
    (
        "写不进本地文件：{error}",
        "Couldn't write to the local file: {error}",
    ),
    (
        "写不了本地文件：{error}",
        "Couldn't write the local file: {error}",
    ),
    (
        "写不了临时文件：{error}",
        "Couldn't write the temporary file: {error}",
    ),
    (
        "看不了临时文件：{error}",
        "Couldn't inspect the temporary file: {error}",
    ),
    (
        "读不了临时文件：{error}",
        "Couldn't read the temporary file: {error}",
    ),
    (
        "建不了临时文件：{error}",
        "Couldn't create the temporary file: {error}",
    ),
    (
        "建不了临时目录：{error}",
        "Couldn't create the temporary folder: {error}",
    ),
    ("建目录失败：{error}", "Couldn't create the folder: {error}"),
    (
        "建不了目标目录：{error}",
        "Couldn't create the target folder: {error}",
    ),
    ("不能把文件夹移进它自己", "Can't move a folder into itself"),
    ("这一项已经不存在了", "This item no longer exists"),
    ("远程那一项已经不在了", "That remote item is gone"),
    (
        "起不了后台线程：{error}",
        "Couldn't start the background thread: {error}",
    ),
    // ---- 撤销 ----
    ("没有可撤销的操作", "Nothing to undo"),
    (
        "原位置已经有同名项目，先移走它再撤销",
        "Something already sits at the original spot; move it first, then undo",
    ),
    (
        "文件夹里已经有了新内容，先清空再撤销",
        "The folder is no longer empty; clear it before undoing",
    ),
    (
        "原文件已经不在改名后的位置了",
        "The original file is no longer at its renamed spot",
    ),
    ("回收站里找不到这一项", "That item isn't in the trash"),
    (
        "找不到个人文件夹，无法定位回收站",
        "Can't locate your home folder, so the trash is out of reach",
    ),
    (
        "回收站里找不到这次删掉的项，请手动打开回收站放回",
        "That item isn't in the trash; put it back from the trash yourself",
    ),
    (
        "这个平台不支持从回收站撤销，请手动打开回收站放回",
        "This platform can't undo from the trash; put it back yourself",
    ),
    // ---- 连接 ----
    ("没有这条连接", "There's no such connection"),
    ("这条远程连接已经不在了", "That remote connection is gone"),
    ("这条连接不支持续传", "This connection can't resume"),
    ("这条连接正在忙", "This connection is busy"),
    ("服务器地址不能为空", "Server address can't be empty"),
    (
        "地址要写成 https://… 这样",
        "Write the address like https://…",
    ),
    ("地址里没有主机名", "The address has no host name"),
    ("端口号不对：{port}", "Wrong port: {port}"),
    (
        "连不上 {address}：{error}",
        "Can't connect to {address}: {error}",
    ),
    (
        "连不上 {host}:{port}：{error}",
        "Can't connect to {host}:{port}: {error}",
    ),
    (
        "进不去起始目录 {start}：{error}",
        "Can't enter the start folder {start}: {error}",
    ),
    (
        "进不去起始目录 {target}：{error}",
        "Can't enter the start folder {target}: {error}",
    ),
    ("登录失败：{error}", "Sign-in failed: {error}"),
    (
        "开不了会话通道：{error}",
        "Couldn't open the session channel: {error}",
    ),
    (
        "服务器不给开 SFTP：{error}",
        "The server wouldn't open SFTP: {error}",
    ),
    ("SFTP 握手失败：{error}", "SFTP handshake failed: {error}"),
    (
        "打不开钥匙串：{error}",
        "Couldn't open the keychain: {error}",
    ),
    (
        "找不到设置文件位置",
        "Can't work out where the settings file lives",
    ),
    (
        "找不到会话文件位置",
        "Can't work out where the session file lives",
    ),
    (
        "找不到恢复文件位置",
        "Can't work out where the recovery file lives",
    ),
    ("序列化失败：{error}", "Serializing failed: {error}"),
    // ---- 列目录与查询 ----
    ("列目录失败：{error}", "Listing the folder failed: {error}"),
    (
        "列目录：读不回响应：{error}",
        "Listing the folder: couldn't read the reply: {error}",
    ),
    (
        "列目录：服务器回 {status}",
        "Listing the folder: the server returned {status}",
    ),
    (
        "列目录：服务器回 {status}：{reason}",
        "Listing the folder: the server returned {status}: {reason}",
    ),
    (
        "列目录：这份 XML 读不了：{error}",
        "Listing the folder: can't read this XML: {error}",
    ),
    (
        "查询：服务器回 {status}",
        "Querying: the server returned {status}",
    ),
    (
        "查询：读不回响应：{error}",
        "Querying: couldn't read the reply: {error}",
    ),
    ("查询：{error}", "Querying: {error}"),
    ("服务器回 {status}", "The server returned {status}"),
    (
        "删除：服务器回 {status}：{reason}",
        "Deleting: the server returned {status}: {reason}",
    ),
    (
        "服务器回的东西里没有文件",
        "The server's reply has no file in it",
    ),
    (
        "列不出 {source:?}：{error}",
        "Couldn't list {source:?}: {error}",
    ),
    (
        "看不了 {source:?}：{error}",
        "Couldn't inspect {source:?}: {error}",
    ),
    (
        "读不了 {source:?}：{error}",
        "Couldn't read {source:?}: {error}",
    ),
    (
        "XML 读不懂：{error}",
        "Can't make sense of the XML: {error}",
    ),
    // ---- S3 / 对象存储 ----
    (
        "认不出桶名：把桶名写在地址里，如 https://host/bucket/",
        "Can't tell which bucket: put it in the address, like https://host/bucket/",
    ),
    // ---- 压缩包 ----
    ("不是 zip 压缩包", "Not a zip archive"),
    (
        "读不了这个压缩包：{error}",
        "Couldn't read this archive: {error}",
    ),
    ("打不开：{error}", "Couldn't open: {error}"),
    (
        "建不了 {zip:?}：{error}",
        "Couldn't create {zip:?}: {error}",
    ),
    ("收尾写坏了：{error}", "The final write went wrong: {error}"),
    ("先选中要打包的东西", "Select something to package first"),
    ("没有可打包的内容", "There's nothing to package"),
    ("没有“{field}:”这个字段", "There's no “{field}:” field"),
    // ---- 传输细节 ----
    (
        "下载：写盘失败：{error}",
        "Downloading: couldn't write to disk: {error}",
    ),
    (
        "下载：写不下去：{error}",
        "Downloading: couldn't keep writing: {error}",
    ),
    (
        "下载：读不回来：{error}",
        "Downloading: couldn't read the reply: {error}",
    ),
    (
        "下载：服务器回 {status}（要的是 {wanted}）",
        "Downloading: the server returned {status} (wanted {wanted})",
    ),
    (
        "续传：定位不到 {offset}：{error}",
        "Resuming: can't position at {offset}: {error}",
    ),
    (
        "打不开 {absolute}：{error}",
        "Couldn't open {absolute}: {error}",
    ),
    ("建不了 {name}：{error}", "Couldn't create {name}: {error}"),
    (
        "建不了 {name} 的上级目录：{error}",
        "Couldn't create {name}'s parent folder: {error}",
    ),
    ("写不了 {name}：{error}", "Couldn't write {name}: {error}"),
    (
        "写不了目录 {name}：{error}",
        "Couldn't write the folder {name}: {error}",
    ),
    (
        "写 {name} 写到一半：{error}",
        "Writing {name}: it broke halfway: {error}",
    ),
    (
        "解 {name} 解到一半：{error}",
        "Unpacking {name}: it broke halfway: {error}",
    ),
    // ---- 终端 ----
    ("没找到可用的终端程序", "No usable terminal program found"),
    ("写不进终端", "Couldn't write to the terminal"),
    ("终端没在跑", "The terminal isn't running"),
    ("{path} 不是目录", "{path} isn't a directory"),
    // ---- 杂项 ----
    (
        "关不掉远程文件：{error}",
        "Couldn't close the remote file: {error}",
    ),
    (
        "切二进制模式失败：{error}",
        "Couldn't switch to binary mode: {error}",
    ),
    ("正则写错了：{error}", "The pattern is wrong: {error}"),
    (
        "起不了网络运行时：{error}",
        "Couldn't start the network runtime: {error}",
    ),
    ("{doing}失败：{detail}", "{doing} failed: {detail}"),
    // ---- 搜索语法（`query.rs`）----
    //
    // 提示翻，语法关键词不翻：`OR` / `NOT` / `content:` / `tag:` 在任何界面里
    // 都得是用户能照着敲的那几个字符——译成 "或者" 的话，用户看懂了提示，
    // 敲进去却搜不出东西。键里带上关键词本身，译文里也原样留着它，
    // 这样用户看提示时看到的字，跟他要敲的字是同一批。
    ("OR 后面少了条件", "OR needs something to its right"),
    (
        "NOT 后面少了要排除的东西",
        "NOT needs something to exclude after it",
    ),
    (
        "- 后面少了要排除的东西",
        "- needs something to exclude after it",
    ),
    (
        "! 后面少了要排除的东西",
        "! needs something to exclude after it",
    ),
    (
        "content: 后面要写要找的词",
        "content: needs the word to look for",
    ),
    ("tag: 后面要写标签", "tag: needs a tag"),
    (
        "引号没配对：{quote}{rest}",
        "Unmatched quote: {quote}{rest}",
    ),
    ("看不懂：{rest}", "Can't make sense of this: {rest}"),
    (
        "大小看不懂：{value}",
        "Can't make sense of this size: {value}",
    ),
    (
        "大小的单位只认 B/K/M/G/T，不是 {unit}",
        "Size units are only B/K/M/G/T, not {unit}",
    ),
    (
        "时间看不懂：{value}",
        "Can't make sense of this time: {value}",
    ),
    (
        "年份看不懂：{value}",
        "Can't make sense of this year: {value}",
    ),
    (
        "月份看不懂：{value}",
        "Can't make sense of this month: {value}",
    ),
    (
        "日期看不懂：{value}",
        "Can't make sense of this date: {value}",
    ),
    (
        "日期不在日历上：{value}",
        "That date isn't on the calendar: {value}",
    ),
    (
        "type 只认 dir/file/image/video/audio/text/archive/code，不是“{bare}”",
        "type only takes dir/file/image/video/audio/text/archive/code, not “{bare}”",
    ),
    (
        "颜色只认 {names}，不是“{bare}”",
        "Colors are only {names}, not “{bare}”",
    ),
    // ---- 协议层零散 ----
    ("无法解析原文件名", "Can't work out the original name"),
    (
        "读不了本地文件：{error}",
        "Couldn't read the local file: {error}",
    ),
    ("读不回响应：{error}", "Couldn't read the reply: {error}"),
    (
        "问不到当前目录：{error}",
        "Couldn't ask for the current folder: {error}",
    ),
    (
        "问不到登录目录：{error}",
        "Couldn't ask for the sign-in folder: {error}",
    ),
    ("改名失败：{error}", "Rename failed: {error}"),
    (
        "钥匙串写不进去：{error}",
        "Couldn't write to the keychain: {error}",
    ),
    (
        "建不了本地目录：{error}",
        "Couldn't create the local folder: {error}",
    ),
    (
        "建不了暂存目录：{error}",
        "Couldn't create the staging folder: {error}",
    ),
    (
        "列不了本地目录：{error}",
        "Couldn't list the local folder: {error}",
    ),
    (
        "已下载但删除远程源失败：{error}",
        "Downloaded, but deleting the remote source failed: {error}",
    ),
    ("不是远程路径：{path}", "Not a remote path: {path}"),
    (
        "不支持在两个不同的服务器之间直接搬运",
        "Can't move between two different servers directly",
    ),
    (
        "上传：服务器回 {status}：{reason}",
        "Uploading: the server returned {status}: {reason}",
    ),
    (
        "下载：服务器回 {status}（要的是 {wanted}）：{reason}",
        "Downloading: the server returned {status} (wanted {wanted}): {reason}",
    ),
    (
        "复制：服务器回 {status}：{reason}",
        "Copying: the server returned {status}: {reason}",
    ),
    (
        "建目录：服务器回 {status}：{reason}",
        "Creating the folder: the server returned {status}: {reason}",
    ),
    (
        "落不了盘（{from} → {to}）：{rename_error}",
        "Couldn't save to disk ({from} → {to}): {rename_error}",
    ),
    (
        "“{name}”已经不在改名后的位置了",
        "“{name}” is no longer at its renamed spot",
    ),
    ("启动{name}失败：{error}", "Couldn't start {name}: {error}"),
    (
        "文件太大，只比了前 {size}",
        "File too large, only the first {size} were compared",
    ),
    // ---- 状态栏的结论 / 对话框 ----
    //
    // **这一段里有两个动作名，形态故意不同，别顺手改成一致**：
    //
    // 1. `HistoryEntry::describe()` 是个**动宾短语**（"重命名 a 为 b"），
    //    它是给「已撤销{what}」「确定撤销“{what}”吗？」当内层用的，
    //    英文那边要当 "Undone: …" 的宾语，所以也是过去式分词。
    // 2. `summarize()` 是个**完成态句子**（"移动完成 5 项"），
    //    英文过去式自己就把动词说完了，不需要 "completed" 之类垫料。
    //
    // 两条凑巧都译成 `Moved {count} items`，但它们是两行表项：
    // 中文 key 不同就是不同的句子，将来有一边改了措辞另一边不用跟。
    ("新建“{name}”", "Created “{name}”"),
    ("重命名“{from}”为“{to}”", "Renamed “{from}” to “{to}”"),
    ("复制 {count} 项", "Copied {count} items"),
    ("移动 {count} 项", "Moved {count} items"),
    ("批量重命名 {count} 项", "Bulk-renamed {count} items"),
    ("移到回收站 {count} 项", "Moved {count} items to the Trash"),
    ("移动完成 {count} 项", "Moved {count} items"),
    ("拷贝完成 {count} 项", "Copied {count} items"),
    (
        "已中止：移动完成 {count} 项",
        "Stopped: moved {count} items",
    ),
    (
        "已中止：拷贝完成 {count} 项",
        "Stopped: copied {count} items",
    ),
    //
    // 下面两条是 `summarize()` 的**子句**，它们跟在主句后面当分词短语用。
    // 主句是过去式（"Copied 5 items"），子句跟着也得是，
    // 否则 "Copied 5 items, 2 items were skipped" 读着像两句拼的。
    ("跳过 {count} 项", "skipped {count}"),
    (
        "{count} 项失败（{name}：{error}）",
        "{count} failed ({name}: {error})",
    ),
    ("已重命名 {count} 项", "Renamed {count} items"),
    ("没有可执行的重命名", "Nothing to rename"),
    ("批量重命名（{total} 项）", "Bulk Rename ({total} items)"),
    ("已创建“{name}”", "Created “{name}”"),
    ("已重命名为“{name}”", "Renamed to “{name}”"),
    (
        "已移到回收站 {count} 项",
        "Moved {count} items to the Trash",
    ),
    (
        "没有项目能移进回收站",
        "Nothing could be moved to the Trash",
    ),
    //
    // 剪贴板：中文用逗号接一句提示，英文不能照抄逗号。
    // 用破折号，因为这里的"提示"在两种语言里都是插进去的旁白。
    (
        "已剪切 {count} 项，到目标文件夹粘贴",
        "Cut {count} items — paste in the destination folder",
    ),
    (
        "已复制 {count} 项，到目标文件夹粘贴",
        "Copied {count} items — paste in the destination folder",
    ),
    ("已取消粘贴", "Paste cancelled"),
    ("已取消限速", "Speed limit off"),
    ("限速 {speed}", "Speed limit {speed}"),
    ("正在中止…", "Stopping…"),
    ("重试 {count} 项", "Retry {count} items"),
    // ---- 撤销 ----
    //
    // `{what}` 里塞的是上面 `describe()` 那些动宾短语。
    // 「已撤销」用冒号而不是像中文那样直接接：
    // 英文动词已经过去式了，后面再接一个过去式分词会读成两个动作。
    ("已撤销{what}", "Undone: {what}"),
    ("撤销失败：{error}", "Undo failed: {error}"),
    ("确定撤销“{what}”吗？", "Undo “{what}”?"),
    ("撤销", "Undo"),
    // 没有历史记录时的兜底描述。正常走不到这里（确认框只在有历史时弹），
    // 但万一走到了也不能显示成空白。
    ("上一步操作", "the last action"),
    // ---- 对话框 ----
    ("新名称", "New name"),
    (
        "确定把 {count} 个项目移到回收站吗？之后还可以撤销。",
        "Move {count} items to the Trash? This can still be undone.",
    ),
    ("连接远程服务器", "Connect to Server"),
    // ---- 隐藏项开关 ----
    //
    // 两条的中文一样但**用途不同**，所以不合并成一条：
    // `显示 / 隐藏隐藏项` 是命令面板里那个中性说法（那条路径拿不到
    // 窗格当前的 `listing`，报不出状态）；另两条是工具栏按钮与
    // 更多菜单上的"按下之后"，靠 `Pane::hidden_switch_title` 选。
    // 英文面板里写 "Show / hide hidden items" 那一条**不用动脑子**，
    // 但混进一个英文界面里就扎眼——所以分开翻。
    ("显示 / 隐藏隐藏项", "Show / Hide Hidden Items"),
    ("显示隐藏项", "Show Hidden Items"),
    ("隐藏隐藏项", "Hide Hidden Items"),
    // 工具栏那颗按钮把动作跟快捷键拼在一起。
    ("{action}（Cmd+Shift+.）", "{action} (Cmd+Shift+.)"),
    // ---- 收藏 ----
    //
    // 分段标题用「收藏夹」而不是「收藏」：分段标题与菜单项如果共用一个
    // 中文 key，英文表就只能给一个答案（按 key 索引，重复条目后写的
    // 静默赢）——"Favorites"当标题对、"Add to Favorites"当菜单项也对，
    // 二选一就是另一个界面上写错。分成两个词就绕开了。
    ("收藏夹", "Favorites"),
    ("收藏 / 取消收藏", "Add / Remove from Favorites"),
    ("收藏", "Add to Favorites"),
    ("取消收藏", "Remove from Favorites"),
    ("已收藏 {name}", "Added “{name}” to Favorites"),
    ("已取消收藏 {name}", "Removed “{name}” from Favorites"),
    // 这两条都只在状态栏出现，区分"没选东西"与"选了一堆"：
    // 后者是"这次操作没法做"，不是"你还没选"。
    (
        "先选中一个项目再收藏",
        "Select one item to add it to Favorites",
    ),
    (
        "一次只能收藏一个项目",
        "Only one item can be favorited at a time",
    ),
];

/// 英文表里有没有漏掉不该出现的条目：译文等于原文（等于没翻），
/// 或者译文是空的。前者在界面上就是"那句话没变"，后者是"那句话没了"。
#[cfg(test)]
fn suspect_entries() -> Vec<&'static str> {
    EN.iter()
        .filter(|(key, text)| text.is_empty() || (*key == *text && !KEPT_AS_IS.contains(key)))
        .map(|(key, _)| *key)
        .collect()
}

/// **故意跟原文一样的条目**：这些不是漏翻，是原样显示才对。
///
/// 列出来而不是放宽判据（"只要 key 在这个名单里就算过"）的原因：
/// 名单一多，就会有人把该翻的往里塞，而那条测试再也拦不住漏翻。
/// 所以每加一条都要在这里写清为什么。
#[cfg(test)]
const KEPT_AS_IS: &[&str] = &[
    // 「坚果云」是一家公司的名字，中文界面里也是这三个字。
    // 译成 "Nutstore Cloud" 的话，反倒跟官网、SaaS 文档里的写法对不上。
    "坚果云",
];

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
    use std::fs;

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

    /// **同一个 key 在表里只能有一条。**
    ///
    /// 表是按 key 索引的（[`index_of`] 建 `HashMap`），重复条目**不会
    /// 报错**——后写的静默覆盖先写的，界面上显示哪一个取决于它们在源码里
    /// 的物理位置。改代码时挪一下位置，译文就变了，没有任何东西会拦住你。
    ///
    /// 顺带说明这个洞原本装了什么：中文 key「列表视图」在表里出现两次，
    /// 一次译 `List View`（菜单项，Title Case 是英语菜单词的规矩），
    /// 一次译 `List view`（切换动作的句子中段）。**两条各有各的道理，
    /// 而 key-indexed 表只能给一个答案。**
    /// 现在留下的是 `List View`——菜单那一处是用户直接点的字，
    /// 无障碍名那处读出来是 "list view"，大小写差别听不出来。
    /// 如果哪天菜单改成小写，这条会挡住你，那才是该重新想 key 的时候。
    #[test]
    fn no_key_appears_twice_in_the_english_table() {
        let mut seen: Vec<&str> = Vec::new();
        let mut repeated = Vec::new();
        for (key, _) in EN {
            if seen.contains(key) {
                repeated.push(*key);
            } else {
                seen.push(key);
            }
        }
        assert!(
            repeated.is_empty(),
            "英文表里这些 key 出现了不止一次：{repeated:?}\
             （HashMap 会让后写的静默覆盖先写的）"
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

    // ---- Message ----

    /// 源码里用到的每一个 `Message` key 都得在英文表里。
    ///
    /// **为什么这条测试值得有**：查不到译文时 `lookup` 会回落中文原文，
    /// 界面上只是「那一句还是中文」——**不会崩、不会报警**。
    /// 于是漏翻一条的代价是几个月后才被人在英文界面里发现，
    /// 而那条错误信息只有触发那个错误的人才看得到。
    /// 换个键名或者抄错一个字，效果跟漏翻一模一样，也一样查不出来。
    ///
    /// 扫源码而不是让人来登记：登记表一定会漏，而且漏了没人知道。
    /// 认不出的构造方式（`explain` 拼的 `{doing}失败：{detail}` 之类）
    /// 单独列在 [`EXEMPT_KEYS`] 里，每条写清为什么。
    #[test]
    fn every_message_key_has_an_english_entry() {
        let found = message_keys_in_src();
        let mut missing = Vec::new();
        for (key, where_) in &found {
            if !EN.iter().any(|(k, _)| k == key) && !EXEMPT_KEYS.contains(key) {
                missing.push(format!("{key}（{where_}）"));
            }
        }
        assert!(
            missing.is_empty(),
            "这些错误信息还没有英文译文：\n  {}",
            missing.join("\n  ")
        );
        // **下限断言是这个测试自己的护栏**。上面那条只查「扫到的都翻了」，
        // 扫描器一旦坏了（改了形式名、括号配对写错、正则不再匹配中文），
        // 扫到 0 条，上面照样绿——「扫不到」和「都翻好了」长得一模一样。
        // 跟 `the_abort_phrase_is_translated` 那条一样：这个测试自己也会有洞。
        //
        // 185 是 2026-10-14 实测数（去掉重复的）。以后真加了新 key，
        // 这个数只会变大；**变小**就是扫描器出问题了，别直接改小它。
        assert!(
            found.len() >= 185,
            "只扫到 {} 条 key，比已知的 185 条还少——扫描器多半坏了，\
             「扫到的都有译文」这条正在空转",
            found.len()
        );
    }

    #[test]
    fn a_message_speaks_english_when_english_is_chosen() {
        let message = Message::msg("名称不能为空");
        assert_eq!(message.say_in(Locale::En), "Name can't be empty");
        // 中文下就是原文（key 自己就是中文，恒等）。
        assert_eq!(message.say_in(Locale::ZhHans), "名称不能为空");
    }

    /// 透传的原文**不查表**：那句英文是 NAS / zip / 操作系统说的，
    /// 翻了它用户排查时认不出是谁讲的。
    #[test]
    fn a_verbatim_message_is_never_translated() {
        let message = Message::verbatim("Permission denied (os error 13)");
        assert!(message.is_verbatim());
        assert_eq!(
            message.say_in(Locale::En),
            "Permission denied (os error 13)"
        );
        assert_eq!(
            message.say_in(Locale::ZhHans),
            "Permission denied (os error 13)"
        );
    }

    /// **链式错误的内层跟着外层一起翻**。
    ///
    /// 这条是 `caused` 存在的全部理由：内层如果是提前 `to_string()`
    /// 冻住的，英文界面里就会露出半中半英的
    /// 「Download: couldn't write to disk: 已中止」。
    #[test]
    fn a_wrapped_message_speaks_english_all_the_way_down() {
        let wrapped = Message::caused("下载：写盘失败：{error}", Message::msg("已中止"));
        assert_eq!(
            wrapped.say_in(Locale::En),
            "Downloading: couldn't write to disk: Stopped"
        );
        // 中文下还是那句中文，一层套一层。
        assert_eq!(wrapped.say_in(Locale::ZhHans), "下载：写盘失败：已中止");
    }

    /// 带别的占位符的那一层，`{name}` 和 `{error}` 各填各的。
    #[test]
    fn a_wrapper_keeps_its_other_placeholders() {
        let wrapped = Message::caused_with(
            "建不了 {name}：{error}",
            &[("name", "报告.txt".to_string())],
            Message::verbatim("Permission denied"),
        );
        assert_eq!(
            wrapped.say_in(Locale::En),
            "Couldn't create 报告.txt: Permission denied"
        );
    }

    /// 主句 + 条件子句：中文用「，」，英文用 ", "。
    ///
    /// **分句符是跟着语言走的，不是消息自己带的**。写死中文逗号的话，
    /// 英文界面里会出现 `Copied 5 items，skipped 2`——一眼就是没收工。
    /// 而 `clause_joiner` 若返回同一个值，这条测试立刻报出来。
    #[test]
    fn a_clause_is_joined_with_the_punctuation_of_its_language() {
        let message = Message::args("拷贝完成 {count} 项", &[("count", "5".into())])
            .then_clause(Message::args("跳过 {count} 项", &[("count", "2".into())]));
        assert_eq!(message.say_in(Locale::ZhHans), "拷贝完成 5 项，跳过 2 项");
        assert_eq!(message.say_in(Locale::En), "Copied 5 items, skipped 2");
    }

    /// 子句里的内层错误也跟着翻。
    ///
    /// 「1 项失败（a.txt：权限不足）」的内层是 `Msg`，走
    /// `then_caused_clause` 而不是 `args`——写成 `args` 就把内层
    /// 冻成中文死符，英文界面里那半句还是中文。
    #[test]
    fn a_clause_translates_its_own_inner_error_too() {
        let message = Message::args("拷贝完成 {count} 项", &[("count", "5".into())])
            .then_caused_clause(
                "{count} 项失败（{name}：{error}）",
                &[("count", "1".into()), ("name", "a.txt".into())],
                Message::msg("已中止"),
            );
        assert_eq!(
            message.say_in(Locale::En),
            "Copied 5 items, 1 failed (a.txt: Stopped)"
        );
        // 中文下三段全中文。
        assert_eq!(
            message.say_in(Locale::ZhHans),
            "拷贝完成 5 项，1 项失败（a.txt：已中止）"
        );
    }

    /// 子句可以有多个，按挂上去的顺序接。
    #[test]
    fn several_clauses_keep_the_order_they_were_attached_in() {
        let message = Message::args("已重命名 {count} 项", &[("count", "3".into())])
            .then_clause(Message::args("跳过 {count} 项", &[("count", "1".into())]))
            .then_caused_clause(
                "{count} 项失败（{name}：{error}）",
                &[("count", "1".into()), ("name", "a.txt".into())],
                Message::verbatim("Permission denied"),
            );
        assert_eq!(
            message.say_in(Locale::En),
            "Renamed 3 items, skipped 1, 1 failed (a.txt: Permission denied)"
        );
    }

    /// 中止哨兵穿过 `io::Error` 那一跳之后，key 还在。
    ///
    /// **为什么值得单测**：`ops::Reporting` 实现的是 `std::io::Read`，
    /// 回调说停时只能递 `io::Error`，那一步默认把错误当**别人的话**原样透传。
    /// key 一丢，上层判 `is_aborted()` 就永远不成立——
    /// 「用户叫停」被当成「真失败」，暂存被扫掉、服务器上的残骸被删。
    #[test]
    fn the_abort_sentinel_survives_being_wrapped_in_an_io_error() {
        let as_io: std::io::Error = std::io::Error::other(Aborted);
        let message: Message = as_io.into();
        assert!(
            message.is_aborted(),
            "哨兵经过 io::Error 之后就不该再被当成 verbatim"
        );
        assert!(!message.is_verbatim());

        // 包一层之后依然认得出：一层套几层都不该丢。
        let wrapped = Message::caused("上传失败：{error}", std::io::Error::other(Aborted));
        assert!(wrapped.is_aborted(), "包了别的措辞之后仍要认得出是被叫停");

        // 而**普通的 IO 错误不该**被误判成中止——那会让真失败白走一遍。
        let genuine: Message = std::io::Error::from(std::io::ErrorKind::NotFound).into();
        assert!(!genuine.is_aborted(), "「文件不存在」不是「用户叫停」");
    }

    #[test]
    fn a_plain_message_is_not_mistaken_for_an_abort() {
        assert!(!Message::msg("下载失败：{error}").is_aborted());
        assert!(
            !Message::verbatim("已中止").is_aborted(),
            "透传的「已中止」是我们的话吗？不是"
        );
        assert!(Message::aborted().is_aborted());
    }

    /// `Display` 恒给中文——它的存在是让还没上屏的中间态有个能看的字符串。
    #[test]
    fn display_is_always_chinese_whatever_the_language() {
        assert_eq!(Message::msg("名称不能为空").to_string(), "名称不能为空");
        assert_eq!(
            Message::caused("下载：写盘失败：{error}", Message::msg("已中止")).to_string(),
            "下载：写盘失败：已中止"
        );
        // 透传的原文在 `Display` 里也原样（那是别人说的话）。
        assert_eq!(
            Message::verbatim("Address has no host name").to_string(),
            "Address has no host name"
        );
    }

    /// 扫源码里所有 `Message::{msg,args,caused,caused_with}` /
    /// `then_caused_clause` 调用**括号内**的中文字面量。
    ///
    /// 返回 (key, 位置) —— 位置进报错信息，让人能直接跳过去补。
    ///
    /// **为什么扫「括号内的全部字面量」而不是「括号后紧跟的第一个」**：
    /// 第一版只取形式名紧跟的那一个字面量，于是漏掉了
    /// `summarize()` 里 `if cut { "已中止：移动完成…" } else { … }`
    /// 这种形状——4 条分支字面量一条都扫不到，测试照样绿。
    /// 「扫不到」和「扫到了但没翻」在测试里长得一模一样，都是绿。
    /// 括号里出现的每个中文字面量都是 key（`verbatim` 单独排除，
    /// 它按约定就是原文），所以宁可多扫。
    fn message_keys_in_src() -> Vec<(&'static str, String)> {
        // 用 `env!("CARGO_MANIFEST_DIR")` 定位源码：`cargo test` 时 CWD
        // 不保证是项目根，靠相对路径读文件会时灵时不灵。编译器给的路径可信。
        let root = env!("CARGO_MANIFEST_DIR");
        let src = format!("{root}/src");
        let mut out = Vec::new();
        // 第一个参数是 key 的构造方式。
        const FORMS: &[&str] = &[
            "Message::msg(",
            "Message::args(",
            "Message::caused(",
            "Message::caused_with(",
            ".then_caused_clause(",
        ];
        for name in fs::read_dir(&src).expect("读得到 src 目录") {
            let path = name.expect("目录项").path();
            if path.extension().and_then(|it| it.to_str()) != Some("rs") {
                continue;
            }
            let file = path
                .file_name()
                .and_then(|it| it.to_str())
                .unwrap_or_default();
            // `i18n.rs` 自己是表，不扫（否则每条表项都变成"没翻"）。
            if file == "i18n.rs" {
                continue;
            }
            // macOS 在外置卷（这里是 Ventoy）上会留下一堆 `._xxx.rs`
            // 资源叉残片：不是 UTF-8，读它会 panic，扫它也没意义。
            // 按名字跳过而不是读失败就算了——后者会把**真的**源码文件
            // 编码坏了这件事一起吞掉，那种情况该报错而不是安静地少扫一个。
            if file.starts_with("._") {
                continue;
            }
            let text = fs::read_to_string(&path).expect("源码是 UTF-8");
            // `verbatim` 按约定就是别人的原话，不查表。它们的调用范围
            // 先标出来，扫别的形式时落在里面的一律跳过——
            // `Message::caused("…", Message::verbatim("服务器说了句中文"))`
            // 这种写法里，那句中文是内层原话，不该被要求有译文。
            let verbatim_spans: Vec<(usize, usize)> = occurrences(&text, VERBATIM_FORM)
                .map(|at| (at, call_end(&text, at + VERBATIM_FORM.len() - 1)))
                .collect();
            for form in FORMS {
                for at in occurrences(&text, form) {
                    if verbatim_spans
                        .iter()
                        .any(|(from, to)| *from <= at && at < *to)
                    {
                        continue;
                    }
                    let end = call_end(&text, at + form.len() - 1);
                    for literal in chinese_literals(&text, at, end) {
                        let leaked: &'static str = Box::leak(literal.to_string().into_boxed_str());
                        out.push((leaked, format!("{file}:{}", line_of(&text, at))));
                    }
                }
            }
        }
        out
    }

    const VERBATIM_FORM: &str = "Message::verbatim(";

    /// `needle` 在 `hay` 里的所有**字节**下标。
    ///
    /// 返回字节下标而不是字符下标：后面要拿它去切 `&str`，
    /// 拿字符下标切会 panic 在非字符边界上。
    fn occurrences<'a>(hay: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
        hay.match_indices(needle).map(|(at, _)| at)
    }

    /// 从 `open`（一个左括号的字节下标）开始，找到配对的右括号之后的位置。
    ///
    /// 跳过字符串字面量里的括号：`Message::args("已完成（{count} 项）", …)`
    /// 里那个全角括号不是问题，但 `Message::verbatim("a) b")` 里的半角括号是——
    /// 不跳过就会数错配对，把后面一大段源码当成调用内容扫进来。
    fn call_end(text: &str, open: usize) -> usize {
        let bytes = text.as_bytes();
        let mut depth = 0usize;
        let mut i = open;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    i += 1;
                    while i < bytes.len() {
                        match bytes[i] {
                            b'\\' => i += 2,
                            b'"' => break,
                            _ => i += 1,
                        }
                    }
                }
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        text.len()
    }

    /// `from..to` 区间里所有**含中文**的字符串字面量（已解转义）。
    ///
    /// 只收含中文的：key 恒为中文原文，纯英文的实参不是 key
    /// （比如 `("name", display_name(path))` 里的格式名）。
    fn chinese_literals(text: &str, from: usize, to: usize) -> Vec<&str> {
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut i = from;
        while i < to {
            if bytes[i] != b'"' {
                i += 1;
                continue;
            }
            let start = i + 1;
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2,
                    b'"' => break,
                    _ => i += 1,
                }
            }
            let end = i.min(bytes.len());
            if end <= to {
                let raw = &text[start..end];
                if raw.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) {
                    out.push(raw);
                }
            }
            i += 1;
        }
        out
    }

    /// 字节下标所在行的**一基**行号，报错信息里给人看的。
    fn line_of(text: &str, at: usize) -> usize {
        text[..at.min(text.len())].matches('\n').count() + 1
    }

    /// 扫不出来的 key，及原因。
    ///
    /// `explain` 拼的键（`{doing}失败：{detail}`）是运行时用实参拼出来的，
    /// 源码里只有格式串——那条的英文走的是同一个格式串，
    /// 所以表里留着它。
    const EXEMPT_KEYS: &[&str] = &[
        "{doing}失败：{detail}",
        // 键是 `i18n::ABORTED` 这个 const，不是字面量。
        ABORTED,
    ];
}
