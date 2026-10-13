//! 拖放的纯逻辑：拖的是什么、能不能落到这儿、落下去是移动还是拷贝。
//!
//! 这一层不认识 GPUI，判定规则可以单独测；
//! 界面只负责把这里给出的结论交给 [`crate::ops_entity::Ops`] 去执行。
//!
//! 之所以不直接复用"剪切 + 粘贴"那条链路：拖放不该动剪贴板。
//! 用户剪贴板里放着待粘的东西时，拖一次就把那批待粘的顶掉，
//! 这是最难解释的一类数据丢失。

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 拖动时在某个文件夹上停多久就把它"弹开"（弹簧文件夹）。
///
/// 想拖到三层深的目录里去，不用先一层层点开再回来拖——
/// 停一下它自己就开了。600 毫秒是 macOS Finder 用久了的那个手感：
/// 短了扫过一排文件夹时会一路乱开，长了就失去了这个功能的意义。
pub const HOVER_EXPAND_AFTER: Duration = Duration::from_millis(600);

/// 从窗格里拖出来的那批文件。
///
/// 存路径而不是下标：拖动过程中目录可能因为监听而刷新，
/// 下标会指到别人身上，路径不会。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraggedPaths {
    pub paths: Vec<PathBuf>,
}

/// 落下去要做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropAction {
    /// 剪切语义：跨卷走"复制成功后删除源文件"，撤销靠回收站。
    Move,
    Copy,
}

impl DropAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Move => "移动",
            Self::Copy => "拷贝",
        }
    }
}

/// 这次落下去是移动还是拷贝。
///
/// 从系统（Finder、资源管理器）拖进来的**只能**拷贝：
/// 那些文件不属于我们，我们没有权限删，也不该删。
/// 自己拖自己的默认是移动，按住 Alt 变成拷贝——
/// 和按住 Alt 拖是 macOS/Windows 上多年不变的约定。
pub fn drop_action(from_outside: bool, alt_held: bool) -> DropAction {
    if from_outside || alt_held {
        DropAction::Copy
    } else {
        DropAction::Move
    }
}

/// 这批文件能落到这个目录里吗。
///
/// 两种"拖了等于没拖"要挡掉：
/// 一是本来就在这个目录里，二是把文件夹拖进它自己的子孙里
/// （那样目录树会变成自己的后代）。挡掉比事后弹冲突询问省事得多。
pub fn can_drop(destination: &Path, paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }

    paths.iter().all(|path| {
        let already_there = path.parent() == Some(destination);
        let into_itself = destination.starts_with(path);
        !already_there && !into_itself
    })
}

/// 鼠标停在它上面时该不该"弹开"。
///
/// 两个条件缺一不可：得是文件夹（文件没有"里面"可进），
/// 得确实在拖动中（平时只是把鼠标扫过去，弹开就是自己乱跳目录）。
pub fn wants_to_expand(is_dir: bool, dragging: bool) -> bool {
    is_dir && dragging
}

/// 拖动时跟在鼠标后面那张小卡片上写什么。
///
/// 一个文件报名字（长了截断），多个报数量——
/// 卡片就那么大，列十个名字反而认不出自己拖的是什么。
pub fn label(paths: &[PathBuf], cx: &gpui_kit::App) -> String {
    match what(paths) {
        DragLabel::Name(name) => name,
        // 空批不写卡片。这条分支实际上到不了（没东西可拖时压根没有拖动），
        // 但留着比让编译器逼我 `unreachable!()` 好——那样哪天真拖了个空的
        // 上来就会 panic，而这里只是画一张空卡片。
        DragLabel::None => String::new(),
        // 数量那一支查表：这张卡片上的字直接摆在屏幕上，
        // 英文用户看到 "3 个项目" 是漏翻。
        DragLabel::Count(count) => {
            crate::i18n::tr_args(cx, "{count} 个项目", &[("count", count.to_string())])
        }
    }
}

/// 卡片上写什么，**跟语言无关的那一半**。
///
/// 分出来是因为"一个报名字、多个报数量"这条性质本身不用查表就能验：
/// 塞进 [`label`] 的话单测得先造一个 GPUI 测试运行时，而这条性质跟界面
/// 一点关系都没有（跟 `throttle::presets` 拆成 `rates()` + `rate_label`
/// 同一个理由）。反过来，多报数量那一支要查表，就留在 [`label`] 里。
#[derive(Debug, PartialEq, Eq)]
pub enum DragLabel {
    /// 空批：卡片什么都不写。
    None,
    /// 单个文件：写它的名字。
    Name(String),
    /// 一批文件：写数量。
    Count(usize),
}

/// 这批路径在卡片上该报什么。
pub fn what(paths: &[PathBuf]) -> DragLabel {
    match paths {
        [] => DragLabel::None,
        [only] => DragLabel::Name(
            only.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| only.display().to_string()),
        ),
        many => DragLabel::Count(many.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn dragging_inside_the_app_moves_until_alt_says_otherwise() {
        assert_eq!(drop_action(false, false), DropAction::Move);
        assert_eq!(
            drop_action(false, true),
            DropAction::Copy,
            "按住 Alt 是拷贝"
        );
    }

    #[test]
    fn files_from_outside_are_only_ever_copied() {
        // 别人拖过来的文件我们不能删，所以 Alt 也不该把它变成移动。
        assert_eq!(drop_action(true, false), DropAction::Copy);
        assert_eq!(drop_action(true, true), DropAction::Copy);
    }

    #[test]
    fn dropping_into_the_same_folder_is_a_no_op() {
        let dest = Path::new("/a");
        assert!(
            !can_drop(dest, &paths(&["/a/x.txt"])),
            "本来就在里面，拖过去等于没动"
        );
        assert!(can_drop(dest, &paths(&["/b/x.txt"])));
    }

    #[test]
    fn a_folder_cannot_be_dropped_into_its_own_descendant() {
        assert!(
            !can_drop(Path::new("/a/b/c"), &paths(&["/a/b"])),
            "把 b 拖进 b/c 会让目录树变成自己的后代"
        );
        // 反过来：把 c 拖进它的祖先目录 /a 是正常的（它现在不在 /a 里）。
        // 注意不能拿 /a/b 当目的地——c 本来就在 /a/b 里，那属于"没动"。
        assert!(can_drop(Path::new("/a"), &paths(&["/a/b/c"])), "反过来可以");
    }

    #[test]
    fn a_mixed_batch_is_rejected_as_a_whole() {
        // 一批里只要有一个落不下去就全部拒绝：
        // 半批移动半批不动比整批拒绝更难解释。
        assert!(!can_drop(Path::new("/a"), &paths(&["/b/x", "/a/y"])));
    }

    #[test]
    fn an_empty_drag_drops_nowhere() {
        assert!(!can_drop(Path::new("/a"), &[]));
    }

    #[test]
    fn the_drag_label_names_one_file_and_counts_many() {
        // 验的是 `what`（跟语言无关的那一半），不是 `label`——
        // 后者要查表，单测它就得造 GPUI 测试运行时。
        // 中文那一支由 `i18n::lookup(En, "{count} 个项目", …)` 的用例管着。
        assert_eq!(
            what(&paths(&["/a/photo.png"])),
            DragLabel::Name("photo.png".into())
        );
        assert_eq!(what(&paths(&["/a/1", "/a/2", "/a/3"])), DragLabel::Count(3));
        assert_eq!(what(&[]), DragLabel::None);
    }

    #[test]
    fn only_a_folder_expands_and_only_while_dragging() {
        assert!(wants_to_expand(true, true));
        assert!(!wants_to_expand(false, true), "文件没有里面可进");
        assert!(!wants_to_expand(true, false), "平时扫过去不该自己跳目录");
    }

    #[test]
    fn the_hover_delay_is_a_deliberate_pause_not_a_brush() {
        // 扫过一排文件夹不会误开，但也不用等太久才开。
        assert!(HOVER_EXPAND_AFTER >= Duration::from_millis(300));
        assert!(HOVER_EXPAND_AFTER <= Duration::from_millis(1500));
    }

    #[test]
    fn both_actions_have_a_label() {
        assert_eq!(DropAction::Move.label(), "移动");
        assert_eq!(DropAction::Copy.label(), "拷贝");
    }
}
