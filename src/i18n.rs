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
    ("收起侧栏", "Collapse sidebar"),
    ("展开侧栏", "Expand sidebar"),
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
    // 三种视图的无障碍名。读屏软件靠它报"现在是列表还是网格"，
    // 所以这三个词也算界面文案，不是纯测试用的钩子。
    ("列表视图", "List view"),
    ("网格视图", "Grid view"),
    ("分列视图", "Columns view"),
    // ---- 侧栏的三个分区标题 ----
    // 它们是 `render_volumes_section` / `render_cloud_section` /
    // `render_remote_section` 里那三行 `.child("…")`，字号一样、位置一样，
    // 英文下面对不齐会很显眼，所以放在一组。
    ("磁盘", "Disks"),
    ("云盘", "Cloud"),
    ("远程", "Remote"),
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
    ("关闭（Esc）", "Close (Esc)"),
    // ---- 侧栏的"位置"（`places()` 的第四项是路径，不翻）----
    // 终端名翻不了：`Option::name` 用来在面板里显示，但它同时是**用户装的那个
    // 程序的名字**——Windows 上的"Windows Terminal"、Linux 上的"GNOME 终端"
    // 各有各的本地化版本，这里硬编码一份英文只会跟系统对不上。
    // 而 App Store / Homebrew 那些名字本身就该按原样显示（iTerm2 就是 iTerm2）。
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
