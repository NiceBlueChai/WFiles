//! 预览内容：判断一个文件该怎么预览，并把能预览的东西读出来。
//!
//! 这一层不依赖任何 UI 类型——UI 只负责把 [`Preview`] 画出来。
//! 这样"哪些文件能预览、预览出来是什么"可以在单测里钉死，
//! 而渲染方式（等宽字体、滚动、配色）可以随便改。
//!
//! 图片只返回 [`Preview::Opaque`]：真正的解码在 `thumbnails` 里异步做，
//! 这里碰都不碰——那是几毫秒到几百毫秒的活，不该出现在读目录的路径上。

use std::{fs, io::Read as _, path::Path};

/// 一次预览愿意读多少内容。
///
/// Quick Look 是"看一眼"，不是编辑器：读满这些就够了，
/// 再往后只会拖慢弹出速度。截断会在界面上明确标出来，不假装是全文。
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    /// 最多读多少字节。
    pub bytes: usize,
    /// 最多保留多少行。
    pub lines: usize,
    /// 二进制最多吐多少行十六进制。
    pub hex_rows: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            bytes: 256 * 1024,
            lines: 500,
            hex_rows: 32,
        }
    }
}

/// 十六进制每行的字节数，跟主流 hex 查看器一致。
pub const HEX_ROW_BYTES: usize = 16;

/// 按内容决定的类型。
///
/// 没有 `Directory` 变体：是不是目录由调用方用 `is_dir` 先判断掉，
/// [`load`] 见到目录直接回 [`Preview::Directory`]，用不着再看后缀。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Pdf,
    Audio,
    Video,
    Archive,
    /// 普通文本：txt、log、csv 这些。
    Text,
    /// 有语法的源码/配置，界面用等宽字体显示。
    Code,
    /// 扩展名没认出来，得看内容才知道。
    Other,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Image => "图像",
            Self::Pdf => "PDF 文档",
            Self::Audio => "音频",
            Self::Video => "视频",
            Self::Archive => "压缩包",
            Self::Text => "文本",
            Self::Code => "源码",
            Self::Other => "文件",
        }
    }

    /// 能不能在 Quick Look 里直接显示内容。
    ///
    /// 图片/PDF/音视频/压缩包要交给专门的解码器或别的模块，
    /// 这里只给个占位；其余的由 [`load`] 读出文本或十六进制。
    fn is_opaque(self) -> bool {
        matches!(
            self,
            Self::Image | Self::Pdf | Self::Audio | Self::Video | Self::Archive
        )
    }
}

/// 读出来的文本内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextPreview {
    pub text: String,
    /// 被字节数或行数截断了，界面要标出来。
    pub truncated: bool,
    /// 保留下来的行数。
    pub lines: usize,
    /// 有语法时的语言名，普通文本是 `None`。
    pub language: Option<&'static str>,
}

/// 二进制的十六进制转储。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HexPreview {
    pub rows: Vec<HexRow>,
    /// 文件总字节数，用来告诉用户"只显示了前 N 字节"。
    pub total_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HexRow {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl HexRow {
    /// 排成 `00000020  48 65 6c 6c 6f 00 ff  Hello..` 这样的一行。
    ///
    /// 不可打印字节一律写成 `.`，免得终端控制字符顺着预览跑出来。
    /// 最后一段不满时**不补空格**：行尾不留空白，复制出来也是干净的。
    pub fn format(&self) -> String {
        let mut hex = String::new();
        for (index, byte) in self.bytes.iter().enumerate() {
            // 前后各 8 字节分开，中间空一格——跟 xxd 一样，方便数偏移。
            if index == HEX_ROW_BYTES / 2 {
                hex.push(' ');
            }
            hex.push_str(&format!("{byte:02x} "));
        }
        let hex = hex.trim_end();

        let ascii: String = self
            .bytes
            .iter()
            .map(|byte| match *byte as char {
                character if character.is_ascii_graphic() || character == ' ' => character,
                _ => '.',
            })
            .collect();

        format!("{:08x}  {hex}  {ascii}", self.offset)
    }
}

/// 一个文件预览出来的样子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preview {
    Directory,
    /// 要交给专门解码器的类型；界面先显示图标和元信息。
    Opaque(Kind),
    Text(TextPreview),
    Hex(HexPreview),
    /// 读不出来：没权限、不是文件、或者磁盘出错。
    Unavailable(String),
}

/// 按扩展名分类。认不出来的返回 [`Kind::Other`]，由 [`load`] 看内容决定。
pub fn classify(path: &Path) -> Kind {
    let Some(extension) = extension_of(path) else {
        return Kind::Other;
    };

    // 复合后缀先看，"tar.gz" 不能因为后缀是 "gz" 就被当成一个普通 gz。
    if matches!(extension.as_str(), "gz" | "bz2" | "xz" | "zst") {
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if stem.ends_with(".tar") {
            return Kind::Archive;
        }
    }

    if IMAGE.contains(&extension.as_str()) {
        return Kind::Image;
    }
    if AUDIO.contains(&extension.as_str()) {
        return Kind::Audio;
    }
    if VIDEO.contains(&extension.as_str()) {
        return Kind::Video;
    }
    if ARCHIVE.contains(&extension.as_str()) {
        return Kind::Archive;
    }
    if extension == "pdf" {
        return Kind::Pdf;
    }
    if language_of_extension(&extension).is_some() {
        return Kind::Code;
    }
    if TEXT.contains(&extension.as_str()) {
        return Kind::Text;
    }

    Kind::Other
}

/// 扩展名对应的语言名；不是源码返回 `None`。
pub fn language_of(path: &Path) -> Option<&'static str> {
    extension_of(path).and_then(|extension| language_of_extension(&extension))
}

fn language_of_extension(extension: &str) -> Option<&'static str> {
    let language = match extension {
        "rs" => "Rust",
        "py" | "pyw" | "pyi" => "Python",
        "js" | "mjs" | "cjs" => "JavaScript",
        "jsx" => "JSX",
        "ts" | "mts" | "cts" => "TypeScript",
        "tsx" => "TSX",
        "json" | "jsonc" | "json5" => "JSON",
        "toml" => "TOML",
        "yaml" | "yml" => "YAML",
        "html" | "htm" => "HTML",
        "css" => "CSS",
        "scss" | "sass" => "Sass",
        "less" => "Less",
        "vue" | "svelte" => "前端组件",
        "c" | "h" => "C",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "C++",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "go" => "Go",
        "rb" => "Ruby",
        "php" => "PHP",
        "lua" => "Lua",
        "sh" | "bash" => "Shell",
        "zsh" => "Zsh",
        "fish" => "Fish",
        "ps1" | "psm1" => "PowerShell",
        "bat" | "cmd" => "批处理",
        "sql" => "SQL",
        "xml" | "xsd" | "xsl" => "XML",
        "swift" => "Swift",
        "dart" => "Dart",
        "scala" => "Scala",
        "cs" => "C#",
        "fs" | "fsx" => "F#",
        "pl" | "pm" => "Perl",
        "r" => "R",
        "m" | "mm" => "Objective-C",
        "hs" => "Haskell",
        "ex" | "exs" => "Elixir",
        "erl" => "Erlang",
        "clj" | "cljs" => "Clojure",
        "vim" => "Vim Script",
        "el" => "Emacs Lisp",
        "zig" => "Zig",
        "nim" => "Nim",
        "jl" => "Julia",
        "ml" => "OCaml",
        "elm" => "Elm",
        "groovy" => "Groovy",
        "gradle" => "Gradle",
        "md" | "markdown" => "Markdown",
        "ini" | "cfg" | "conf" | "config" | "properties" => "配置",
        _ => return None,
    };

    Some(language)
}

/// 小写扩展名；没有扩展名时是空串而不是 `None`，方便直接查表。
fn extension_of(path: &Path) -> Option<String> {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
}

/// 读出预览内容。目录和需要专门解码器的类型只回一个占位。
pub fn load(path: &Path) -> Preview {
    load_with(path, Budget::default())
}

/// 同 [`load`]，但可以指定读多少。测试用它构造截断场景，
/// 不必真的去写一个几百 KB 的文件。
pub fn load_with(path: &Path, budget: Budget) -> Preview {
    if path.is_dir() {
        return Preview::Directory;
    }

    let kind = classify(path);
    let size = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if kind.is_opaque() {
        return Preview::Opaque(kind);
    }

    // 多读一个字节：读满了就说明后面还有，截断标记据此置位，
    // 不必再 stat 一次也不必信任可能已经过期的 size。
    let mut buffer = Vec::new();
    let read = match fs::File::open(path) {
        Ok(mut file) => file
            .by_ref()
            .take(budget.bytes as u64 + 1)
            .read_to_end(&mut buffer),
        Err(error) => return Preview::Unavailable(error.to_string()),
    };
    if let Err(error) = read {
        return Preview::Unavailable(error.to_string());
    }

    let truncated_by_bytes = buffer.len() > budget.bytes;
    if truncated_by_bytes {
        buffer.truncate(budget.bytes);
    }

    if looks_binary(&buffer) {
        return Preview::Hex(HexPreview {
            rows: hex_rows(&buffer, budget.hex_rows),
            total_bytes: size,
        });
    }

    let mut text = String::from_utf8_lossy(&buffer).into_owned();
    // UTF-8 BOM 不属于内容，留在首行会显示成一个怪字符。
    if text.starts_with('\u{feff}') {
        text.remove(0);
    }

    // 按字节截断可能正好切在某个字符中间，往回退到字符边界再切。
    if truncated_by_bytes && text.len() > budget.bytes {
        let boundary = text.floor_char_boundary(budget.bytes);
        text.truncate(boundary);
    }

    let mut lines: Vec<&str> = text.lines().collect();
    let truncated_by_lines = lines.len() > budget.lines;
    if truncated_by_lines {
        lines.truncate(budget.lines);
    }
    let total = lines.len();
    let text = lines.join("\n");

    Preview::Text(TextPreview {
        text,
        truncated: truncated_by_bytes || truncated_by_lines,
        lines: total,
        language: language_of(path),
    })
}

/// 是否当二进制处理。
///
/// 两条规则，都是 git 在用的：有 NUL 字节，或者根本不是合法 UTF-8。
/// 真实文本里不会出现 NUL；而 UTF-16 之类虽然"是文本"，
/// 我们也没法当 UTF-8 渲染，转储成十六进制至少不丢信息。
///
/// 结尾被截断造成的半个字符**不算**二进制：按字节上限读文件必然会切在
/// 字符中间，那不是文件的问题，是我们读得少。`error_len()` 为 `None`
/// 正是"输入到结尾了但序列没完"这一种。
fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    if bytes.contains(&0) {
        return true;
    }

    match std::str::from_utf8(bytes) {
        Ok(_) => false,
        Err(error) => error.error_len().is_some(),
    }
}

fn hex_rows(bytes: &[u8], limit: usize) -> Vec<HexRow> {
    bytes
        .chunks(HEX_ROW_BYTES)
        .take(limit)
        .enumerate()
        .map(|(index, chunk)| HexRow {
            offset: (index * HEX_ROW_BYTES) as u64,
            bytes: chunk.to_vec(),
        })
        .collect()
}

const IMAGE: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "tif", "tiff", "webp", "avif", "heic", "heif",
    "svg", "pbm", "pgm", "ppm", "pam", "qoi", "tga", "dds", "hdr", "exr",
];
const AUDIO: &[&str] = &[
    "mp3", "wav", "flac", "aac", "m4a", "ogg", "oga", "opus", "aiff", "aif", "wma",
];
const VIDEO: &[&str] = &[
    "mp4", "m4v", "mov", "avi", "mkv", "webm", "wmv", "flv", "mpg", "mpeg", "ts", "m2ts",
];
const ARCHIVE: &[&str] = &[
    "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "iso", "dmg", "deb", "rpm", "apk", "jar",
];
const TEXT: &[&str] = &[
    "txt",
    "text",
    "log",
    "csv",
    "tsv",
    "rtf",
    "env",
    "gitignore",
    "gitattributes",
    "editorconfig",
    "nfo",
    "asc",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(name);
        fs::write(&path, contents).unwrap();
        (root, path)
    }

    fn text_of(root: &tempfile::TempDir, name: &str, contents: &str) -> TextPreview {
        let path = root.path().join(name);
        fs::write(&path, contents).unwrap();
        let Preview::Text(preview) = load(&path) else {
            panic!("{name} 应该预览成文本");
        };
        preview
    }

    #[test]
    fn extensions_map_to_the_right_kind() {
        let cases = [
            ("photo.PNG", Kind::Image),
            ("paper.pdf", Kind::Pdf),
            ("song.mp3", Kind::Audio),
            ("clip.mkv", Kind::Video),
            ("bundle.tar.gz", Kind::Archive),
            ("plain.zip", Kind::Archive),
            ("main.rs", Kind::Code),
            ("notes.txt", Kind::Text),
            ("mystery.qqq", Kind::Other),
            ("noextension", Kind::Other),
        ];

        for (name, expected) in cases {
            assert_eq!(
                classify(Path::new(name)),
                expected,
                "{name} 应该分类成 {expected:?}"
            );
        }
    }

    #[test]
    fn tar_gz_is_an_archive_not_a_plain_gzip() {
        // "gz" 本身在压缩包表里，这条测的是复合后缀先判定的分支。
        assert_eq!(classify(Path::new("a.tar.gz")), Kind::Archive);
        assert_eq!(classify(Path::new("a.gz")), Kind::Archive);
    }

    #[test]
    fn text_files_preview_with_their_contents() {
        let root = tempfile::tempdir().unwrap();
        let preview = text_of(&root, "a.txt", "第一行\n第二行\n");
        assert_eq!(preview.text, "第一行\n第二行");
        assert_eq!(preview.lines, 2);
        assert!(!preview.truncated);
        assert_eq!(preview.language, None);
    }

    #[test]
    fn source_files_carry_their_language() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            text_of(&root, "a.rs", "fn main() {}").language,
            Some("Rust")
        );
        assert_eq!(
            text_of(&root, "a.json", "{}").language,
            Some("JSON"),
            "配置文件也算代码：要等宽、要保留缩进"
        );
        assert_eq!(text_of(&root, "a.py", "x = 1").language, Some("Python"));
    }

    #[test]
    fn unknown_extensions_fall_back_to_content_sniffing() {
        let root = tempfile::tempdir().unwrap();
        // Dockerfile、LICENSE 这类没有后缀的文件，看着像文本就该当文本显示。
        let preview = text_of(&root, "Dockerfile", "FROM alpine\n");
        assert_eq!(preview.text, "FROM alpine");
        assert_eq!(preview.language, None);

        // 0xff 开头的字节在任何位置都不是合法 UTF-8；
        // 注意不能用 0x01..0x04——那些是合法的控制字符，会被当文本。
        let binary = root.path().join("blob");
        fs::write(&binary, [0xff, 0xfe, 0xfd, 0xfc]).unwrap();
        assert!(
            matches!(load(&binary), Preview::Hex(_)),
            "没有后缀的二进制不该被当文本"
        );
    }

    #[test]
    fn nul_bytes_make_a_file_binary() {
        let (_root, path) = write("weird.txt", "hello\0world");
        assert!(
            matches!(load(&path), Preview::Hex(_)),
            "文本后缀也挡不住 NUL：内容说了算"
        );
    }

    #[test]
    fn invalid_utf8_is_dumped_instead_of_mangled() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("latin.txt");
        fs::write(&path, [0x48, 0xe9, 0x6c, 0x6c, 0x6f]).unwrap();
        assert!(matches!(load(&path), Preview::Hex(_)));
    }

    #[test]
    fn a_byte_utf8_bom_is_stripped() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("bom.txt");
        fs::write(&path, "\u{feff}hello").unwrap();

        let Preview::Text(preview) = load(&path) else {
            panic!("应该是文本");
        };
        assert_eq!(preview.text, "hello");
    }

    #[test]
    fn byte_budget_truncates_without_splitting_a_character() {
        let root = tempfile::tempdir().unwrap();
        // 每个"好"是 3 字节，5 字节的预算必然切在字符中间。
        let path = root.path().join("chinese.txt");
        fs::write(&path, "好好好好好").unwrap();

        let Preview::Text(preview) = load_with(
            &path,
            Budget {
                bytes: 5,
                lines: 10,
                hex_rows: 4,
            },
        ) else {
            panic!("应该是文本");
        };

        assert_eq!(preview.text, "好", "切完必须还是合法 UTF-8");
        assert!(preview.truncated);
    }

    #[test]
    fn line_budget_truncates_and_flags_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("many.txt");
        fs::write(&path, "a\nb\nc\nd\ne").unwrap();

        let Preview::Text(preview) = load_with(
            &path,
            Budget {
                bytes: 1024,
                lines: 3,
                hex_rows: 4,
            },
        ) else {
            panic!("应该是文本");
        };

        assert_eq!(preview.text, "a\nb\nc");
        assert_eq!(preview.lines, 3);
        assert!(preview.truncated, "被截断时必须标出来，不能假装是全文");
    }

    #[test]
    fn opaque_kinds_are_not_read_at_all() {
        let root = tempfile::tempdir().unwrap();
        // 内容是文本但后缀是图片：不该去读它，交给缩略图层异步解码。
        let path = root.path().join("fake.png");
        fs::write(&path, "not really a png").unwrap();

        assert_eq!(load(&path), Preview::Opaque(Kind::Image));
    }

    #[test]
    fn directories_are_reported_as_such() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(load(root.path()), Preview::Directory);
    }

    #[test]
    fn missing_files_say_why() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("gone.txt");

        let Preview::Unavailable(reason) = load(&missing) else {
            panic!("应该是读不出来");
        };
        assert!(!reason.is_empty(), "界面要显示这句话，不能是空的");
    }

    #[test]
    fn hex_rows_are_laid_out_like_a_real_hex_viewer() {
        let row = HexRow {
            offset: 0x20,
            bytes: b"Hello\x00\xff".to_vec(),
        };
        assert_eq!(
            row.format(),
            "00000020  48 65 6c 6c 6f 00 ff  Hello..",
            "不可打印字节要写成点，不能把控制字符放出去"
        );
    }

    #[test]
    fn hex_preview_keeps_only_the_first_rows() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("blob.bin");
        let bytes: Vec<u8> = (0..128u8).collect();
        fs::write(&path, &bytes).unwrap();

        let Preview::Hex(preview) = load_with(
            &path,
            Budget {
                bytes: 4096,
                lines: 10,
                hex_rows: 2,
            },
        ) else {
            panic!("应该是十六进制");
        };

        assert_eq!(preview.rows.len(), 2);
        assert_eq!(preview.rows[1].offset, HEX_ROW_BYTES as u64);
        assert_eq!(preview.total_bytes, 128, "总长度要能告诉用户看了多少");
    }

    #[test]
    fn kind_labels_are_chinese() {
        assert_eq!(Kind::Image.label(), "图像");
        assert_eq!(Kind::Code.label(), "源码");
    }
}
