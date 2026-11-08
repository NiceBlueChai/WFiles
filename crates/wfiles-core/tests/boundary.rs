//! core 的边界护栏。
//!
//! 「core 不依赖界面实现」不能只写在文档里：这条测试钉住两件事——
//! 源码里不许出现任何界面框架的导入或反向模块引用，依赖表里不许出现界面库。
//!
//! 按项目里那条吃过亏的纪律，**先证明扫描器是活的**：把一段明知违规的文本
//! 喂进去，它必须报出来。少了这一步，「扫不到」和「都干净」在测试里长得
//! 一模一样，全是绿。

use std::fs;
use std::path::PathBuf;

/// 界面框架的 crate 名 / 导入前缀。
const UI_FRAMEWORKS: &[&str] = &[
    "gpui_kit",
    "gpui::",
    "ratatui",
    "crossterm",
    "egui",
    "iced::",
    "slint",
    "winit",
    "tauri",
];

/// 只属于界面层的模块名。core 里出现 `crate::<name>::` 就是反向依赖。
const UI_MODULES: &[&str] = &[
    "app",
    "pane",
    "ops_entity",
    "remotes",
    "settings",
    "meta",
    "thumbnails",
    "keymap",
    "drag",
    "throttle",
    "term",
    "term_entity",
    "search_entity",
    "compare_entity",
    "diff_entity",
    "analyze_entity",
    "ui_tests",
];

fn source_files() -> Vec<PathBuf> {
    fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
        .expect("读得到 core/src")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
        // 外置卷上的 `._xxx.rs` 资源叉残片不是源码，也不是 UTF-8。
        .filter(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .starts_with("._")
        })
        .collect()
}

/// 注释不算：文档里当然要写清楚「不依赖某个界面框架」，那不是导入。
fn code_lines(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn frameworks_in(text: &str) -> Vec<&'static str> {
    UI_FRAMEWORKS
        .iter()
        .copied()
        .filter(|needle| text.contains(needle))
        .collect()
}

fn reverse_refs_in(text: &str) -> Vec<&'static str> {
    UI_MODULES
        .iter()
        .copied()
        .filter(|name| text.contains(&format!("crate::{name}::")))
        .collect()
}

#[test]
fn the_scanner_is_alive() {
    let bad = "use gpui_kit::prelude::*;\nfn f(_: &gpui::App) { let _ = ratatui::Terminal; }";
    let found = frameworks_in(&code_lines(bad));
    assert!(found.contains(&"gpui_kit"));
    assert!(found.contains(&"gpui::"));
    assert!(found.contains(&"ratatui"));

    // 反过来：纯注释里提到这些名字不该被判违规，否则护栏会逼人删光说明。
    let prose = "// 这里不许用 gpui_kit，也不许用 ratatui\nfn okay() {}";
    assert!(frameworks_in(&code_lines(prose)).is_empty());

    // 反向依赖的扫描器也要自检（含一个容易误伤的前缀：`term` vs `terminal`）。
    assert_eq!(reverse_refs_in("let x = crate::pane::Pane;"), vec!["pane"]);
    assert!(reverse_refs_in("let x = crate::terminal::spawn();").is_empty());
}

#[test]
fn core_source_never_imports_a_ui_framework() {
    let files = source_files();
    assert!(
        files.len() >= 30,
        "只找到 {} 个源文件，扫描器多半坏了",
        files.len()
    );
    let mut bad = Vec::new();
    for path in &files {
        let text = fs::read_to_string(path).expect("源码是 UTF-8");
        let code = code_lines(&text);
        for name in frameworks_in(&code) {
            bad.push(format!("{}: 界面框架 {name}", path.display()));
        }
        for name in reverse_refs_in(&code) {
            bad.push(format!("{}: 反向引用 crate::{name}", path.display()));
        }
    }
    assert!(
        bad.is_empty(),
        "core 越过了界面边界：\n  {}",
        bad.join("\n  ")
    );
}

#[test]
fn core_manifest_has_no_ui_framework_dependency() {
    let manifest = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("读得到 core 的 Cargo.toml");
    let bad: Vec<String> = manifest
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.starts_with('#') && !line.starts_with('['))
        .filter(|line| line.contains('='))
        .filter(|line| {
            UI_FRAMEWORKS
                .iter()
                .any(|name| line.starts_with(&format!("{name} =")))
                || line.starts_with("gpui-kit =")
                || line.starts_with("gpui =")
        })
        .map(String::from)
        .collect();
    assert!(bad.is_empty(), "core 的依赖表里出现了界面框架：{bad:?}");
}
