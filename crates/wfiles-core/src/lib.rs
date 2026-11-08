//! WFiles 核心：`wfiles-core`。
//!
//! # 这一层是什么
//!
//! 与界面无关的全部业务逻辑。它**不认识**任何界面框架：这个 crate 的依赖表里
//! 没有 GUI 库，源码里也不许出现 UI 框架的导入（`tests/boundary.rs` 会在编译期
//! 之外再加一道源码与依赖表的护栏）。GUI（当前是 GPUI Kit）、后续 TUI、以及
//! 任何换框架后的新界面，都只通过 [`contract`] 或这里的公开模块调用它。
//!
//! # 职责边界
//!
//! | 层 | 归谁 | 允许依赖 |
//! |---|---|---|
//! | `wfiles-core` | 目录与条目模型、排序与过滤、搬运/撤销/续传、远程协议、搜索、比较、差异、查询、i18n 纯函数、设置与会话数据、日志、单实例 | 标准库 + 与界面无关的现有 crate |
//! | 界面层（根 crate `wfiles`） | 窗口、窗格、渲染、焦点与选中态、拖放手势、快捷键、菜单、缩略图解码、内嵌终端绘制、`Settings` 的 GPUI 全局槽位 | `wfiles-core` + 界面框架 |
//!
//! 方向只有一条：**界面 → core**。core 里没有反向通道，也不存 `Entity` / `Context`
//! 一类的界面句柄。界面要跟随 core 的状态变化，就自己订阅并重新取快照。
//!
//! # 稳定调用契约
//!
//! [`contract`] 给出 `Command` → `Event` 的窄接口：新前端只认这几个类型，
//! 不需要知道 `model` / `ops` / `remote` 怎么组织。既有 GUI 仍可直接用下面的
//! 细粒度模块（`wfiles` 把它们 `pub use` 回 `crate::xxx`，路径没变）。
//!
//! # 跨平台
//!
//! 这里没有 `#[cfg(target_os = ...)]` 的界面假设。平台差异只留在两处：
//! 依赖表里按平台选钥匙串后端，以及 `trash` / `dirs` 这些库自己处理系统差异。

pub mod analyze;
pub mod archive;
pub mod bulk_rename;
pub mod cli;
pub mod clipboard;
pub mod cloud;
pub mod compare;
pub mod diff;
pub mod favorites;
pub mod format;
pub mod ftp;
pub mod i18n;
pub mod instance;
pub mod logfile;
pub mod marks;
pub mod model;
pub mod offline;
pub mod ops;
pub mod palette;
pub mod path_input;
pub mod preview;
pub mod query;
pub mod remote;
pub mod restore;
pub mod resume;
pub mod s3;
pub mod search;
pub mod session;
/// SFTP。默认编进来；`default-features = false` 的构建（如 NSH 客户端）
/// 不带它，理由见 `Cargo.toml` 的 `[features]`。
#[cfg(feature = "sftp")]
pub mod sftp;
/// 新建文件的可选模板（纯数据）。
pub mod templates;
pub mod terminal;
pub mod volumes;
pub mod watch;

/// 性能基准只归 core：它测的是纯逻辑的复杂度，与界面无关。
/// `FileEntry::for_test` 是 `#[cfg(test)]` 的，必须和 `model` 同一个 crate 才看得见。
#[cfg(test)]
mod perf;

/// 文件系统特殊对象（链接 / 联接点 / 硬链接 / 快捷方式 / 云占位）的统一语义。
/// 平台探测集中在这里，其余模块只认 `EntryKind`。
pub mod entry_kind;

/// 操作侧状态机与事件流：撤销栈的唯一持有者，GUI 与 TUI 共用。
pub mod kernel;

/// 给所有前端（GUI / TUI / 换框架后的新界面）用的稳定契约。
pub mod contract;
