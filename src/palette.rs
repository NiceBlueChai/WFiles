//! 命令面板的纯逻辑：命令模型与打分过滤。
//!
//! 界面只负责把命令列表和查询词喂进来，这里返回排好序的下标。
//! 打分规则刻意简单可预测：标题精确 > 标题前缀 > 标题包含 > 关键词命中，
//! 多个查询词之间是“与”的关系——少打一个字也能搜到，多打一个词能收窄。

/// 一条可被面板执行的命令。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// 稳定标识，应用层据此知道该执行什么。
    pub id: String,
    /// 显示名，也是主要匹配对象。
    pub title: String,
    /// 右侧灰字：快捷键或补充说明。
    pub hint: String,
    /// 分组名，用于在列表里分段。
    pub group: String,
    /// 额外匹配词，空格分隔。
    pub keywords: String,
}

impl Command {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        hint: impl Into<String>,
        group: impl Into<String>,
        keywords: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            hint: hint.into(),
            group: group.into(),
            keywords: keywords.into(),
        }
    }
}

/// 空查询时给所有命令的统一保底分：足够低，任何真实匹配都排在它前面。
const EMPTY_QUERY_SCORE: u32 = 1;

const SCORE_EXACT: u32 = 1000;
const SCORE_PREFIX: u32 = 600;
const SCORE_TITLE_CONTAINS: u32 = 300;
const SCORE_KEYWORD_EXACT: u32 = 200;
const SCORE_KEYWORD_CONTAINS: u32 = 100;

/// 单个查询词对一条命令的贡献分；0 表示不匹配。
fn token_score(command: &Command, token: &str) -> u32 {
    let title = command.title.to_lowercase();
    if title == token {
        return SCORE_EXACT;
    }
    if title.starts_with(token) {
        return SCORE_PREFIX;
    }
    if title.contains(token) {
        return SCORE_TITLE_CONTAINS;
    }
    if command
        .keywords
        .to_lowercase()
        .split_whitespace()
        .any(|keyword| keyword == token)
    {
        return SCORE_KEYWORD_EXACT;
    }
    if command.keywords.to_lowercase().contains(token) {
        return SCORE_KEYWORD_CONTAINS;
    }
    0
}

/// 一条命令对整条查询的总分；0 表示不匹配，应被过滤掉。
///
/// 查询按空白切词，每个词都必须命中——这是“与”语义，
/// 既能用「回收 站」这种断续输入，也能用「新建 文件夹」精确收窄。
pub fn score(command: &Command, query: &str) -> u32 {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|token| token.to_lowercase())
        .collect();
    if tokens.is_empty() {
        return EMPTY_QUERY_SCORE;
    }
    let mut total = 0;
    for token in &tokens {
        let contribution = token_score(command, token);
        if contribution == 0 {
            return 0;
        }
        total += contribution;
    }
    total
}

/// 过滤并排序：按总分降序，同分保持命令的原始顺序（稳定）。
pub fn filter(commands: &[Command], query: &str) -> Vec<usize> {
    let mut scored: Vec<(usize, u32)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            let value = score(command, query);
            (value > 0).then_some((index, value))
        })
        .collect();
    scored.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    scored.into_iter().map(|(index, _)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands() -> Vec<Command> {
        vec![
            Command::new(
                "go-home",
                "前往个人文件夹",
                "Cmd+Shift+H",
                "前往",
                "home 主目录",
            ),
            Command::new("go-desktop", "前往桌面", "", "前往", "desktop"),
            Command::new(
                "op-new-folder",
                "新建文件夹",
                "Cmd+Shift+N",
                "文件",
                "folder 创建 目录",
            ),
            Command::new(
                "op-trash",
                "移到回收站",
                "Cmd+Backspace",
                "文件",
                "delete 删除",
            ),
            Command::new(
                "op-copy-path",
                "复制路径",
                "Cmd+Alt+C",
                "文件",
                "path 剪贴板",
            ),
        ]
    }

    #[test]
    fn empty_query_keeps_everything_in_order() {
        let list = commands();
        assert_eq!(filter(&list, ""), vec![0, 1, 2, 3, 4]);
        assert_eq!(filter(&list, "   "), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn exact_title_beats_prefix_and_contains() {
        let list = commands();
        // "复制路径" 精确命中第 4 条；"复制" 只能前缀命中它。
        assert_eq!(filter(&list, "复制路径")[0], 4);
        assert_eq!(filter(&list, "复制")[0], 4);
    }

    #[test]
    fn prefix_beats_plain_contains() {
        let list = vec![
            Command::new("a", "重命名文件", "", "文件", ""),
            Command::new("b", "批量重命名", "", "文件", ""),
        ];
        // 两条都含"重命名"，但只有"重命名文件"是前缀命中，分数更高。
        assert_eq!(filter(&list, "重命名"), vec![0, 1]);
    }

    #[test]
    fn keywords_are_searched_too() {
        let list = commands();
        // 标题里没有 "delete"，但关键词里有。
        assert_eq!(filter(&list, "delete"), vec![3]);
        // "目录" 精确命中一条关键词，同时是另一条关键词的子串，精确的排前面。
        assert_eq!(filter(&list, "目录"), vec![2, 0]);
    }

    #[test]
    fn all_tokens_must_match() {
        let list = commands();
        assert_eq!(filter(&list, "新建 文件夹"), vec![2]);
        // "桌面" 和 "文件夹" 不在同一条命令里，搜不到东西。
        assert!(filter(&list, "桌面 文件夹").is_empty());
    }

    #[test]
    fn chinese_query_needs_no_word_boundaries() {
        let list = commands();
        assert_eq!(filter(&list, "回收"), vec![3]);
        assert_eq!(filter(&list, "回收站"), vec![3]);
    }

    #[test]
    fn english_query_is_case_insensitive() {
        let list = commands();
        assert_eq!(filter(&list, "HOME"), vec![0]);
        assert_eq!(filter(&list, "Path"), vec![4]);
    }

    #[test]
    fn no_match_yields_nothing() {
        let list = commands();
        assert!(filter(&list, "zzz不存在").is_empty());
    }

    #[test]
    fn scores_are_ordered_exact_over_prefix_over_contains() {
        let list = vec![
            Command::new("a", "打开终端面板", "", "工具", ""),
            Command::new("b", "终端", "", "工具", ""),
            Command::new("c", "在终端中打开", "", "工具", ""),
        ];
        // b 精确 > a 前缀 > c 包含。
        assert_eq!(filter(&list, "终端"), vec![1, 0, 2]);
    }

    #[test]
    fn ties_keep_the_original_order() {
        let list = vec![
            Command::new("a", "刷新", "", "视图", ""),
            Command::new("b", "刷新", "", "视图", ""),
            Command::new("c", "刷新", "", "视图", ""),
        ];
        assert_eq!(filter(&list, "刷新"), vec![0, 1, 2]);
    }
}
