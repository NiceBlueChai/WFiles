//! 压缩包：看里面有什么、解压出来、把一批文件打成包。
//!
//! 只做 **zip** 一个格式：它是唯一在三平台上都不用额外装软件就能打开的
//! （Windows 资源管理器、macOS 访达、Linux 各家的归档管理器都认）。
//! RAR 要专有解码器，7z 的常见实现又拖一堆依赖——都不符合
//! "三平台通用 + 日常高频"这条收功能的线。
//! 压缩包**内搜索**明确不做（见 ROADMAP「明确不做」）。
//!
//! 这一层不认识 GPUI：判定与读写都能单独测，界面只负责把结论画出来、
//! 把活扔到后台线程上跑。

use std::fs::File;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use zip::ZipArchive;
use zip::write::{SimpleFileOptions, ZipWriter};
use zip::CompressionMethod;

/// 压缩包里的一个条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 包内路径，用 `/` 分隔（zip 规范就是这么写的）。
    pub name: String,
    /// 解压后的大小。目录是 0。
    pub size: u64,
    pub is_dir: bool,
}

/// 这个文件是不是我们能打开的压缩包。
pub fn is_archive(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
}

/// 读出压缩包里的条目，按包内路径排好序。
pub fn entries(path: &Path) -> Result<Vec<Entry>, String> {
    if !is_archive(path) {
        return Err("不是 zip 压缩包".to_string());
    }
    let file = File::open(path).map_err(|error| format!("打不开：{error}"))?;
    let mut archive =
        ZipArchive::new(file).map_err(|error| format!("读不了这个压缩包：{error}"))?;

    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| format!("第 {} 项读不出来：{error}", index + 1))?;
        // 名字可能是 CP437 编码的老包，读不出来就报出来而不是假装空名字。
        let name = entry
            .name()
            .map_err(|error| format!("第 {} 项的名字读不出来：{error}", index + 1))?
            .to_string();
        entries.push(Entry {
            is_dir: name.ends_with('/'),
            name,
            size: entry.size(),
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

/// 解压：把 `wanted` 里点到的条目解到 `dest` 目录下，返回解出来的**文件**个数。
///
/// `wanted` 留空就是全部。目录条目只建目录，不计进返回值——
/// 给用户报"解出来几个文件"，目录不算文件。
pub fn extract(path: &Path, dest: &Path, wanted: &[String]) -> Result<usize, String> {
    if !is_archive(path) {
        return Err("不是 zip 压缩包".to_string());
    }
    std::fs::create_dir_all(dest).map_err(|error| format!("建不了目标目录：{error}"))?;
    let file = File::open(path).map_err(|error| format!("打不开：{error}"))?;
    let mut archive =
        ZipArchive::new(file).map_err(|error| format!("读不了这个压缩包：{error}"))?;

    let mut files = 0;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("第 {} 项读不出来：{error}", index + 1))?;
        let name = entry
            .name()
            .map_err(|error| format!("第 {} 项的名字读不出来：{error}", index + 1))?
            .to_string();
        if !wanted.is_empty() && !wanted.iter().any(|it| it == &name) {
            continue;
        }
        let Some(out) = safe_destination(dest, &name) else {
            // 跳过而不是整包失败：一个坏条目不该毁掉另外几十个好文件。
            continue;
        };
        if entry.is_dir() || name.ends_with('/') {
            std::fs::create_dir_all(&out)
                .map_err(|error| format!("建不了 {name}：{error}"))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("建不了 {name} 的上级目录：{error}"))?;
        }
        let mut out_file =
            File::create(&out).map_err(|error| format!("写不了 {name}：{error}"))?;
        std::io::copy(&mut entry, &mut out_file)
            .map_err(|error| format!("解 {name} 解到一半：{error}"))?;
        files += 1;
    }
    Ok(files)
}

/// 条目 `name` 该落在 `dest` 底下的哪个路径。挡掉 Zip-Slip。
///
/// 恶意（或打包时写错）的压缩包里可能有 `../../evil.sh` 或 `/etc/evil`
/// 这种名字，照着解就会写到目标目录外面去——这是解压类漏洞里最经典的一个。
/// 凡是要往上走（`..`）、带盘符、或者干脆是绝对路径的，一律不接受。
fn safe_destination(dest: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    // 有些打包工具用 Windows 的反斜杠当分隔符，一并认。
    let name = name.replace('\\', "/");
    let mut out = dest.to_path_buf();
    for part in Path::new(&name).components() {
        match part {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    // 解完还是目标目录本身：说明名字里一个正常分量都没有。
    (out != dest).then_some(out)
}

/// 打包：把 `sources` 收进 `zip`，包内路径按它们相对 `base` 的位置算。
///
/// `base` 是这批文件的共同父目录。选中三个散落的文件打包时，
/// 用当前目录当 base：包里就不会出现一长串 `Users/xxx/Downloads/...`。
pub fn create(zip: &Path, base: &Path, sources: &[PathBuf]) -> Result<usize, String> {
    if sources.is_empty() {
        return Err("先选中要打包的东西".to_string());
    }
    let file = File::create(zip).map_err(|error| format!("建不了 {zip:?}：{error}"))?;
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let mut files = 0;
    for source in sources {
        let relative = relative_name(source, base);
        files += add_path(&mut writer, source, &relative, zip, options)?;
    }
    writer
        .finish()
        .map_err(|error| format!("收尾写坏了：{error}"))?;
    if files == 0 {
        return Err("没有可打包的内容".to_string());
    }
    Ok(files)
}

/// 一个文件或一整棵目录树在包里的相对路径。
///
/// 选中目录自己当 base 时相对路径是空的，那就用目录自己的名字——
/// "把这个文件夹打包"说的就是包里有这个文件夹。
fn relative_name(source: &Path, base: &Path) -> PathBuf {
    let stripped = source
        .strip_prefix(base)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty());
    match stripped {
        Some(relative) => relative.to_path_buf(),
        None => source
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| source.to_path_buf()),
    }
}

/// 把一个文件或一整个目录写进包里，返回写进去的文件个数。
fn add_path<W: Write + std::io::Seek>(
    writer: &mut ZipWriter<W>,
    source: &Path,
    relative: &Path,
    archive: &Path,
    options: SimpleFileOptions,
) -> Result<usize, String> {
    // 把自己装进去会一边写一边读，越打越大，最后是个废包。
    if same_path(source, archive) {
        return Ok(0);
    }
    let name = relative
        .to_str()
        .map(|name| name.replace('\\', "/"))
        .ok_or_else(|| format!("{source:?} 的名字不是合法 Unicode"))?;
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| format!("看不了 {source:?}：{error}"))?;

    if metadata.is_dir() {
        writer
            .add_directory(format!("{name}/"), options)
            .map_err(|error| format!("写不了目录 {name}：{error}"))?;
        let mut files = 0;
        let children = std::fs::read_dir(source)
            .map_err(|error| format!("列不出 {source:?}：{error}"))?;
        for child in children {
            let child = child.map_err(|error| format!("列不出 {source:?}：{error}"))?;
            let child_path = child.path();
            let child_relative = relative.join(child.file_name());
            files += add_path(writer, &child_path, &child_relative, archive, options)?;
        }
        return Ok(files);
    }

    if !metadata.is_file() {
        // 符号链接之类：跟着解出来会指向包外的东西，跳过。
        return Ok(0);
    }

    writer
        .start_file(name.clone(), options)
        .map_err(|error| format!("写不了 {name}：{error}"))?;
    let mut input =
        File::open(source).map_err(|error| format!("读不了 {source:?}：{error}"))?;
    std::io::copy(&mut input, writer)
        .map_err(|error| format!("写 {name} 写到一半：{error}"))?;
    Ok(1)
}

/// 两条路径是不是同一个文件（名字不同但指向同一处也算）。
fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// 目标名字撞车时换个名字：`报告.zip` 已存在就试 `报告 2.zip`。
///
/// 绝不覆盖已有文件：把人家一份好好的归档顶掉，比多一个带序号的名字糟得多。
pub fn unused_name(directory: &Path, stem: &str, extension: &str) -> PathBuf {
    let mut candidate = directory.join(format!("{stem}.{extension}"));
    let mut counter = 2;
    while candidate.exists() {
        candidate = directory.join(format!("{stem} {counter}.{extension}"));
        counter += 1;
    }
    candidate
}

/// 解压目录撞车时换个名字：`报告` 已存在就试 `报告 2`。
///
/// 和 [`unused_name`] 是同一套规矩，只是一个不带扩展名。
pub fn unused_dir(parent: &Path, stem: &str) -> PathBuf {
    let mut candidate = parent.join(stem);
    let mut counter = 2;
    while candidate.exists() {
        candidate = parent.join(format!("{stem} {counter}"));
        counter += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::{
        Entry, create, entries, extract, is_archive, safe_destination, unused_dir, unused_name,
    };
    use std::path::{Path, PathBuf};

    fn dir(name: &str) -> tempfile::TempDir {
        tempfile::tempdir().expect(name)
    }

    #[test]
    fn only_zip_counts_as_an_archive() {
        assert!(is_archive(Path::new("/a/b.zip")));
        assert!(is_archive(Path::new("/a/b.ZIP")), "扩展名大小写不敏感");
        assert!(!is_archive(Path::new("/a/b.rar")));
        assert!(!is_archive(Path::new("/a/b.7z")));
        assert!(!is_archive(Path::new("/a/b.txt")));
        assert!(!is_archive(Path::new("/a/b")));
    }

    #[test]
    fn a_round_trip_keeps_names_sizes_and_contents() {
        let work = dir("工作目录");
        let inside = work.path().join("inside");
        std::fs::create_dir_all(inside.join("nested")).unwrap();
        std::fs::write(inside.join("a.txt"), "hello").unwrap();
        std::fs::write(inside.join("nested").join("b.txt"), "world!").unwrap();

        let zip = work.path().join("bundle.zip");
        let count = create(&zip, work.path(), std::slice::from_ref(&inside)).expect("打包");
        assert_eq!(count, 2, "两个文件（目录不算）");

        let listed = entries(&zip).expect("列条目");
        assert_eq!(
            listed,
            vec![
                Entry {
                    name: "inside/".to_string(),
                    size: 0,
                    is_dir: true
                },
                Entry {
                    name: "inside/a.txt".to_string(),
                    size: 5,
                    is_dir: false
                },
                Entry {
                    name: "inside/nested/".to_string(),
                    size: 0,
                    is_dir: true
                },
                Entry {
                    name: "inside/nested/b.txt".to_string(),
                    size: 6,
                    is_dir: false
                },
            ]
        );

        let out = work.path().join("out");
        assert_eq!(extract(&zip, &out, &[]).expect("解压"), 2);
        assert_eq!(
            std::fs::read_to_string(out.join("inside").join("a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("inside").join("nested").join("b.txt")).unwrap(),
            "world!"
        );
    }

    #[test]
    fn extracting_only_what_was_picked() {
        let work = dir("工作目录");
        std::fs::write(work.path().join("one.txt"), "1").unwrap();
        std::fs::write(work.path().join("two.txt"), "2").unwrap();
        let zip = work.path().join("two.zip");
        create(&zip, work.path(), &[work.path().join("one.txt"), work.path().join("two.txt")])
            .expect("打包");

        let out = work.path().join("out");
        let files = extract(&zip, &out, &["two.txt".to_string()]).expect("解压");
        assert_eq!(files, 1);
        assert!(out.join("two.txt").exists());
        assert!(!out.join("one.txt").exists(), "没点到的不该解出来");
    }

    #[test]
    fn a_malicious_entry_cannot_escape_the_destination() {
        assert_eq!(safe_destination(Path::new("/out"), "ok.txt").as_deref(), Some(Path::new("/out/ok.txt")));
        assert_eq!(safe_destination(Path::new("/out"), "a/b.txt").as_deref(), Some(Path::new("/out/a/b.txt")));
        assert_eq!(safe_destination(Path::new("/out"), "../escape.txt"), None, "Zip-Slip");
        assert_eq!(safe_destination(Path::new("/out"), "a/../../escape.txt"), None);
        assert_eq!(safe_destination(Path::new("/out"), "/etc/passwd"), None, "绝对路径");
        assert_eq!(safe_destination(Path::new("/out"), ""), None);
        assert_eq!(safe_destination(Path::new("/out"), "..\\escape.txt"), None, "反斜杠也要认");
    }

    #[test]
    fn a_name_that_is_already_taken_gets_a_number() {
        let work = dir("工作目录");
        std::fs::write(work.path().join("报告.zip"), "x").unwrap();
        let next = unused_name(work.path(), "报告", "zip");
        assert_eq!(next, work.path().join("报告 2.zip"));
        std::fs::write(&next, "x").unwrap();
        assert_eq!(
            unused_name(work.path(), "报告", "zip"),
            work.path().join("报告 3.zip")
        );
    }

    #[test]
    fn packing_a_single_file_outside_the_base_uses_just_its_name() {
        let work = dir("工作目录");
        let other = dir("别处");
        std::fs::write(other.path().join("note.txt"), "hi").unwrap();
        let zip = work.path().join("one.zip");
        create(&zip, work.path(), &[other.path().join("note.txt")]).expect("打包");

        let listed = entries(&zip).expect("列条目");
        assert_eq!(
            listed,
            vec![Entry {
                name: "note.txt".to_string(),
                size: 2,
                is_dir: false
            }],
            "base 之外的文件只记名字，不该把整条路径塞进去"
        );
    }

    #[test]
    fn an_empty_selection_is_refused() {
        let work = dir("工作目录");
        assert!(create(&work.path().join("x.zip"), work.path(), &[]).is_err());
    }

    #[test]
    fn a_non_archive_is_refused() {
        let work = dir("工作目录");
        let plain = work.path().join("plain.txt");
        std::fs::write(&plain, "x").unwrap();
        assert!(entries(&plain).is_err());
        assert!(extract(&plain, &work.path().join("out"), &[]).is_err());
    }

    #[test]
    fn the_zip_never_packs_itself() {
        let work = dir("工作目录");
        std::fs::write(work.path().join("a.txt"), "x").unwrap();
        let zip = work.path().join("all.zip");
        // 把整个目录（连带即将生成的 zip）都选上，也不能把自己装进去。
        let parent = work.path().parent().expect("临时目录有上级").to_path_buf();
        let count = create(&zip, &parent, &[work.path().to_path_buf()]).expect("打包");
        let listed = entries(&zip).expect("列条目");
        assert_eq!(count, 1, "包里只有一个文件：{listed:?}");
        assert!(
            listed.iter().all(|entry| !entry.name.ends_with("all.zip")),
            "包里不该有 all.zip 自己：{listed:?}"
        );
    }

    #[test]
    fn packing_a_folder_puts_the_folder_itself_inside() {
        let work = dir("工作目录");
        let folder = work.path().join(" stuff".replace(' ', ""));
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("a.txt"), "x").unwrap();
        let zip = work.path().join("bundle.zip");
        create(&zip, &folder, std::slice::from_ref(&folder)).expect("打包");

        let listed = entries(&zip).expect("列条目");
        assert_eq!(
            listed
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["stuff/", "stuff/a.txt"],
            "选中文件夹打包时，包里要有这个文件夹"
        );
    }

    #[test]
    fn a_directory_that_is_already_taken_gets_a_number() {
        let work = dir("工作目录");
        std::fs::create_dir(work.path().join("报告")).unwrap();
        assert_eq!(unused_dir(work.path(), "报告"), work.path().join("报告 2"));
        assert_eq!(
            unused_dir(work.path(), "别的"),
            work.path().join("别的"),
            "没撞车就用原名"
        );
    }

    #[test]
    fn unused_name_does_not_touch_existing_files() {
        let work = dir("工作目录");
        let first: PathBuf = unused_name(work.path(), "照片", "zip");
        assert_eq!(first, work.path().join("照片.zip"));
        assert!(!first.exists(), "只是算出个名字，不该顺手建文件");
    }
}
