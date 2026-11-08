//! 新建文件时可选的模板。
//!
//! # 为什么模板内容写死在代码里
//!
//! 不读磁盘上的"模板目录"。理由是**新建文件不能有前置条件**：用户目录里没有
//! 那个目录、权限不对、路径里有空格，都会让"新建"这个动作失败在一个跟它无关的
//! 地方。将来要做自定义模板，再单独设计一套（存哪儿、怎么校验、跨平台），
//! 别现在留一个半成品的目录扫描。
//!
//! # 为什么可执行位是模板的属性
//!
//! 新建 `run.sh` 却不能直接跑，用户还得去终端 `chmod +x`——"新建脚本"这件事
//! 就只做了一半。unix 上按模板把 `0o755` 补上；Windows 没有这个位，忽略。

/// 一个可选的新建文件模板。纯数据，界面照它排菜单。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileTemplate {
    /// 稳定 id：界面、配置、命令面板都用它，别用显示名。
    pub id: &'static str,
    /// 显示名（中文原文即 i18n key）。
    pub label: &'static str,
    /// 默认后缀，带点。用来预填「未命名.md」。
    pub extension: &'static str,
    /// 初始内容。
    pub contents: &'static str,
    /// 写完要不要补可执行位（只在 unix 上生效）。
    pub executable: bool,
}

/// 全部模板。顺序就是界面上的顺序：最常用的空白排第一。
pub const TEMPLATES: &[FileTemplate] = &[
    FileTemplate {
        id: "plain",
        label: "空白文件",
        extension: ".txt",
        contents: "",
        executable: false,
    },
    FileTemplate {
        id: "markdown",
        label: "Markdown 文档",
        extension: ".md",
        contents: "# 标题\n\n",
        executable: false,
    },
    FileTemplate {
        id: "rust",
        label: "Rust 源文件",
        extension: ".rs",
        contents: "fn main() {\n    println!(\"Hello, world!\");\n}\n",
        executable: false,
    },
    FileTemplate {
        id: "python",
        label: "Python 脚本",
        extension: ".py",
        contents: "#!/usr/bin/env python3\n\n\ndef main() -> None:\n    pass\n\n\nif __name__ == \"__main__\":\n    main()\n",
        executable: true,
    },
    FileTemplate {
        id: "shell",
        label: "Shell 脚本",
        extension: ".sh",
        contents: "#!/bin/sh\n\nset -eu\n\n",
        executable: true,
    },
    FileTemplate {
        id: "json",
        label: "JSON 文件",
        extension: ".json",
        contents: "{\n  \n}\n",
        executable: false,
    },
    FileTemplate {
        id: "yaml",
        label: "YAML 文件",
        extension: ".yml",
        contents: "---\n",
        executable: false,
    },
    FileTemplate {
        id: "html",
        label: "HTML 文件",
        extension: ".html",
        contents: "<!DOCTYPE html>\n<html lang=\"zh-CN\">\n<head>\n  <meta charset=\"utf-8\">\n  <title>标题</title>\n</head>\n<body>\n\n</body>\n</html>\n",
        executable: false,
    },
];

/// 按 id 找模板。认不出来的 id 返回 `None`（配置文件是手改的，不能崩）。
pub fn template(id: &str) -> Option<&'static FileTemplate> {
    TEMPLATES.iter().find(|template| template.id == id)
}

/// 空白模板：`Cmd+Alt+N` 之类"不问模板"的入口用它。
pub fn plain() -> &'static FileTemplate {
    // TEMPLATES 的第一条就是它；`?` 在这里不可用（没有调用方愿意处理 None）。
    template("plain").expect("plain 模板永远存在")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn every_template_has_a_unique_id_and_a_dotted_extension() {
        let mut ids = HashSet::new();
        let mut extensions = HashSet::new();
        for template in TEMPLATES {
            assert!(ids.insert(template.id), "id 重复：{}", template.id);
            assert!(
                template.extension.starts_with('.') && template.extension.len() > 1,
                "后缀要以点开头且不为空：{}",
                template.extension
            );
            assert!(!template.label.is_empty(), "{} 没有显示名", template.id);
            assert!(
                extensions.insert(template.extension),
                "两个模板抢同一个后缀：{}",
                template.extension
            );
        }
    }

    #[test]
    fn an_unknown_id_is_not_a_panic() {
        assert!(template("no-such-template").is_none());
        assert!(plain().id == "plain");
    }

    /// 脚本模板必须带 shebang，否则"可执行"只是个好听的说法。
    #[test]
    fn executable_templates_start_with_a_shebang() {
        for template in TEMPLATES.iter().filter(|t| t.executable) {
            assert!(
                template.contents.starts_with("#!"),
                "{} 标了可执行却没有 shebang",
                template.id
            );
        }
    }
}
