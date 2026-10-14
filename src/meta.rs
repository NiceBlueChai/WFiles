//! 可选元数据列：Git 状态、图片尺寸、媒体时长、EXIF、ID3、PDF 页数。
//!
//! 六样东西共用一个位置：**一列**，显示什么由用户挑。
//! 六个都铺开会把文件名挤没，而一般人同时只关心其中一种。
//!
//! 三条硬规矩：
//! 1. **只读头几个字节**（PDF 例外，页数得全文扫），绝不整文件解码——
//!    一万行的目录里滚一圈不能把 CPU 打满。
//! 2. **探不出来就是这一格空着**，不报错、不重试、不弹窗。元数据是"顺带瞄一眼"
//!    的东西，它不该变成用户的负担。
//! 3. 解析一律"读到哪算哪"：格式不对就 `None`，绝不 panic——
//!    用户目录里的文件什么长相都可能有。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use gpui_kit::{AsyncApp, Context, EventEmitter, WeakEntity};
use serde::{Deserialize, Serialize};

use crate::settings::Settings;

/// 单个文件最多读多少字节去探元数据。
///
/// 拍坏的文件（比如一个 20 GB 的假 `.mp4`）不该把内存吃光，
/// 而真正要找的东西——moov、ID3、EXIF——全在头尾，读不到就是读不到。
const HEAD_BYTES: usize = 512 * 1024;

/// 缓存里最多留多少条。一条是路径加几十字节，这个上限把常驻内存压在 MB 量级。
const CAPACITY: usize = 4096;

/// 元数据列能显示哪几种。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Field {
    /// 这个文件在 Git 里的状态（已改 / 未跟踪 / …）。
    Git,
    /// 图片的像素尺寸。
    Image,
    /// 音视频的时长。
    Duration,
    /// 照片的拍摄时间与相机。
    Exif,
    /// MP3 的曲名与艺术家。
    Id3,
    /// PDF 的页数。
    Pages,
}

impl Field {
    pub const ALL: [Self; 6] = [
        Self::Git,
        Self::Image,
        Self::Duration,
        Self::Exif,
        Self::Id3,
        Self::Pages,
    ];

    /// 这一列的标题。收 `cx` 是为了按当前语言显示——
    /// 返回 `&'static str` 就只能永远是中文，而设置里存的是 `id()`
    /// （不翻），所以翻译这一层不影响落盘格式。
    pub fn label(self, cx: &gpui_kit::App) -> String {
        let text = match self {
            Self::Git => "Git 状态",
            Self::Image => "图片尺寸",
            Self::Duration => "媒体时长",
            Self::Exif => "EXIF",
            Self::Id3 => "ID3",
            Self::Pages => "PDF 页数",
        };
        crate::i18n::tr(cx, text)
    }

    /// 存进设置与命令面板 id 用的短名。手改配置的人写的是这个。
    pub fn id(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Image => "image",
            Self::Duration => "duration",
            Self::Exif => "exif",
            Self::Id3 => "id3",
            Self::Pages => "pages",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|field| field.id() == id)
    }

    /// 命令面板里搜什么词能找到它。
    pub fn keywords(self) -> &'static str {
        match self {
            Self::Git => "git 版本控制 状态 改动 仓库",
            Self::Image => "图片 尺寸 分辨率 宽 高 image size",
            Self::Duration => "时长 视频 音频 媒体 多久 duration",
            Self::Exif => "exif 照片 拍摄 相机 时间 参数",
            Self::Id3 => "id3 mp3 音乐 曲名 歌手 艺术家 专辑",
            Self::Pages => "pdf 页数 文档 page",
        }
    }

    /// 这个文件值不值得探一下。
    ///
    /// 便宜的判定（看后缀），挡掉注定失败的后台任务：
    /// 给 `notes.txt` 起一个线程去找 EXIF 只是白烧一次调度。
    pub fn applies(self, path: &Path, is_dir: bool) -> bool {
        match self {
            // 目录也有状态：里面改了多少项，汇总出来。
            Self::Git => true,
            _ if is_dir => false,
            // 图片格式不手抄名单：`image` 认得什么我们就探什么，
            // 它以后加了新格式我们自动跟上。
            Self::Image => extension_of(path)
                .is_some_and(|extension| image::ImageFormat::from_extension(&extension).is_some()),
            Self::Duration => has_extension(
                path,
                &["mp4", "m4v", "mov", "m4a", "mp4v", "mkv", "webm", "wav"],
            ),
            Self::Exif => has_extension(path, &["jpg", "jpeg", "tif", "tiff"]),
            Self::Id3 => has_extension(path, &["mp3"]),
            Self::Pages => has_extension(path, &["pdf"]),
        }
    }
}

fn extension_of(path: &Path) -> Option<String> {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
}

fn has_extension(path: &Path, wanted: &[&str]) -> bool {
    extension_of(path).is_some_and(|extension| wanted.contains(&extension.as_str()))
}

// ---------------------------------------------------------------------------
// 探测（跑在后台线程上）
// ---------------------------------------------------------------------------

/// 探一个文件的一项元数据。可能读文件、可能起子进程，**别在 UI 线程调**。
pub fn probe(field: Field, path: &Path, is_dir: bool) -> Option<String> {
    if !field.applies(path, is_dir) {
        return None;
    }
    match field {
        // Git 是仓库级的，一次调用回来一批，走 [`git_status`]，不在这里探。
        Field::Git => None,
        Field::Image => image::image_dimensions(path)
            .ok()
            .map(|(width, height)| format!("{width} × {height}")),
        Field::Duration => duration(path).map(format_duration),
        Field::Exif => exif(path),
        Field::Id3 => id3(path),
        Field::Pages => pdf_pages(path).map(|pages| format!("{pages} 页")),
    }
}

/// 读头一段；文件比上限大就只读上限那么多。
///
/// 之所以不"整个读进来"：这条路径会被一万行的目录反复走，
/// 一个拍坏的 4 GB 假视频就能把内存打爆。
fn read_head(path: &Path, cap: usize) -> Option<Vec<u8>> {
    let mut file = fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let take = size.min(cap as u64) as usize;
    let mut buffer = vec![0u8; take];
    file.read_exact(&mut buffer).ok()?;
    Some(buffer)
}

/// 秒数写成 `1:23` / `1:02:03`。
pub fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0. {
        return String::new();
    }
    let total = seconds.round() as u64;
    let (hours, minutes, rest) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{rest:02}")
    } else {
        format!("{minutes}:{rest:02}")
    }
}

/// 音视频时长（秒）。认 MP4/MOV/M4A、MKV/WebM、WAV 三族。
fn duration(path: &Path) -> Option<f64> {
    match extension_of(path)?.as_str() {
        "wav" => wav_duration(&read_head(path, HEAD_BYTES)?),
        "mkv" | "webm" => {
            let data = read_head(path, HEAD_BYTES)?;
            ebml_duration(&data)
        }
        _ => mp4_duration(&read_head(path, HEAD_BYTES)?),
    }
}

const U32: usize = std::mem::size_of::<u32>();

fn u32_be(data: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = data.get(at..at + U32)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn u32_le(data: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = data.get(at..at + U32)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn u64_be(data: &[u8], at: usize) -> Option<u64> {
    let bytes: [u8; 8] = data.get(at..at + 8)?.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

/// MP4/MOV：一路盒子套盒子找到 `moov` → `mvhd`，里面有时基与时长。
fn mp4_duration(data: &[u8]) -> Option<f64> {
    let moov = find_box(data, 0, data.len(), b"moov")?;
    let mvhd = find_box(data, moov.0, moov.1, b"mvhd")?;
    let at = mvhd.0;
    // mvhd 开头是版本号：v0 用 32 位时间戳，v1 用 64 位，字段位置跟着变。
    let (timescale, duration) = if data.get(at)? == &1 {
        (u32_be(data, at + 20)?, u64_be(data, at + 24)? as f64)
    } else {
        (u32_be(data, at + 12)?, u32_be(data, at + 16)? as f64)
    };
    if timescale == 0 {
        return None;
    }
    Some(duration / timescale as f64)
}

/// 在这一层盒子里找一个叫 `kind` 的，返回它**内容**的范围。
fn find_box(data: &[u8], start: usize, end: usize, kind: &[u8; 4]) -> Option<(usize, usize)> {
    let mut position = start;
    while position + 8 <= end {
        let mut size = u32_be(data, position)? as usize;
        let mut header = 8;
        // size == 1 表示长度用后面 8 个字节存（超过 4 GB 的文件才会有）。
        if size == 1 {
            size = u64_be(data, position + 8)? as usize;
            header = 16;
        } else if size == 0 {
            // size == 0 表示"一直到文件尾"。
            size = end - position;
        }
        if size < header || position + size > end {
            return None;
        }
        if data.get(position + 4..position + 8)? == kind {
            return Some((position + header, position + size));
        }
        position += size;
    }
    None
}

/// WAV：`fmt ` 里拿字节率，`data` 里拿长度，除一下就是秒数。
fn wav_duration(data: &[u8]) -> Option<f64> {
    if data.len() < 12 || &data[..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return None;
    }
    let (mut byte_rate, mut data_bytes) = (None, None);
    // RIFF 的块是"类型(4) + 长度(4)"，跟 MP4 那种"长度在前"的盒子正好相反。
    let mut position = 12;
    while position + 8 <= data.len() {
        let kind = data.get(position..position + 4);
        let size = u32_le(data, position + 4)? as usize;
        let payload = position + 8;
        match kind {
            // fmt 块里：格式(2) 声道(2) 采样率(4) 字节率(4) …
            Some(kind) if kind == b"fmt " => byte_rate = u32_le(data, payload + 8),
            Some(kind) if kind == b"data" => data_bytes = Some(size),
            _ => {}
        }
        // 块按偶数字节对齐，奇数长度后面垫一个字节。
        position = payload + size + (size % 2);
    }
    match (byte_rate, data_bytes) {
        (Some(rate), Some(bytes)) if rate > 0 => Some(bytes as f64 / rate as f64),
        _ => None,
    }
}

// ---- EBML（MKV / WebM）----

const EBML_SEGMENT: u64 = 0x1853_8067;
const EBML_INFO: u64 = 0x1549_A966;
const EBML_DURATION: u64 = 0x4489;
const EBML_TIMESTAMP_SCALE: u64 = 0x2A_D7_B1;

/// EBML 的时长：`Segment → Info → Duration` 是一个浮点数，
/// 单位是"时间戳刻度"，刻度大小在 `TimestampScale` 里（默认 10^9，即纳秒）。
fn ebml_duration(data: &[u8]) -> Option<f64> {
    let (start, end) = ebml_find(data, 0, data.len(), EBML_DURATION)?;
    let scaled = match end - start {
        4 => f32::from_be_bytes(data.get(start..end)?.try_into().ok()?) as f64,
        8 => f64::from_be_bytes(data.get(start..end)?.try_into().ok()?),
        _ => return None,
    };
    let scale = ebml_find(data, 0, data.len(), EBML_TIMESTAMP_SCALE)
        .and_then(|(at, to)| ebml_uint(data, at, to))
        .unwrap_or(1_000_000_000) as f64;
    if scale <= 0. {
        return None;
    }
    Some(scaled * scale / 1_000_000_000.)
}

/// 读一个 EBML 无符号整数（去掉高位长度标记后剩下的字节）。
fn ebml_uint(data: &[u8], start: usize, end: usize) -> Option<u64> {
    let mut value = 0u64;
    for byte in data.get(start..end)? {
        value = (value << 8) | *byte as u64;
    }
    Some(value)
}

/// 在 EBML 树里找 `target`，返回它内容的范围；只往 Segment 与 Info 两个容器里钻。
fn ebml_find(data: &[u8], start: usize, end: usize, target: u64) -> Option<(usize, usize)> {
    let mut position = start;
    while position < end {
        let (id, id_len) = ebml_vint(data, position, false)?;
        let (size, size_len) = ebml_vint(data, position + id_len, true)?;
        let payload = position + id_len + size_len;
        // 尺寸位全 1 是"长度未知"（流式文件常见）：这一层就到边界为止。
        let unknown = size == (1u64 << (7 * size_len)) - 1;
        let next = if unknown {
            end
        } else {
            payload + size as usize
        };
        if next > end {
            return None;
        }
        if id == target {
            return Some((payload, next));
        }
        // 只往 Segment 与 Info 两个容器里钻：别的元素当数据看，别乱解释。
        if (id == EBML_SEGMENT || id == EBML_INFO)
            && payload < next
            && let Some(hit) = ebml_find(data, payload, next, target)
        {
            return Some(hit);
        }
        position = next;
    }
    None
}

/// 读一个 EBML 变长整数：长度由第一个字节前导零的个数决定。
///
/// `is_size` 时长度标记本身不参与取值（尺寸的每一位都是数据位），
/// id 则保留那一位——否则 `0x4489` 和 `0x489` 就分不清了。
fn ebml_vint(data: &[u8], at: usize, is_size: bool) -> Option<(u64, usize)> {
    let first = *data.get(at)?;
    if first == 0 {
        return None;
    }
    // 前导零的个数就是长度标记：u8 的 leading_zeros 是 0..8，直接用。
    let length = first.leading_zeros() as usize + 1;
    let bytes = data.get(at..at + length)?;
    let mut value = if is_size {
        (bytes[0] & (0xFF >> length)) as u64
    } else {
        bytes[0] as u64
    };
    for byte in &bytes[1..] {
        value = (value << 8) | *byte as u64;
    }
    Some((value, length))
}

// ---- EXIF ----

/// JPEG/TIFF 里的 EXIF：优先报拍摄时间，没有时间就报相机型号。
fn exif(path: &Path) -> Option<String> {
    let data = read_head(path, HEAD_BYTES)?;
    // JPEG 的 EXIF 挂在 APP1 段里；TIFF 整个文件就是一块 EXIF。
    let tiff_at = if data.starts_with(&[0xFF, 0xD8]) {
        jpeg_app1(&data)?
    } else if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        0
    } else {
        return None;
    };
    let le = data.get(tiff_at)? == &b'I';
    let ifd0 = usize::try_from(tiff_u32(&data, tiff_at + 4, le)?).ok()?;
    let entries = tiff_ifd(&data, tiff_at, ifd0, le)?;

    let date = tiff_text(&data, tiff_at, &entries, 0x0132)
        .or_else(|| {
            let exif_at = usize::try_from(tiff_value(&entries, 0x8769)?).ok()?;
            let exif = tiff_ifd(&data, tiff_at, exif_at, le)?;
            tiff_text(&data, tiff_at, &exif, 0x9003)
        })
        .map(|raw| tidy_date(&raw));
    let camera = tiff_text(&data, tiff_at, &entries, 0x0110)
        .or_else(|| tiff_text(&data, tiff_at, &entries, 0x010F));

    match (date, camera) {
        (Some(date), Some(camera)) => Some(format!("{date} · {camera}")),
        (Some(date), None) => Some(date),
        (None, Some(camera)) => Some(camera),
        (None, None) => None,
    }
}

/// 走 JPEG 的段表找第一个 APP1（`FF E1`）且内容以 `Exif\0\0` 开头的，
/// 返回里面 TIFF 头在文件中的位置。
fn jpeg_app1(data: &[u8]) -> Option<usize> {
    let mut position = 2;
    while position + 4 <= data.len() {
        if data[position] != 0xFF {
            return None;
        }
        let marker = data[position + 1];
        // SOS（图像数据开始）之后就没有段了，EXIF 只可能在它前面。
        if marker == 0xDA || marker == 0xD8 {
            return None;
        }
        let length = usize::from(u16::from_be_bytes(
            data.get(position + 2..position + 4)?.try_into().ok()?,
        ));
        if length < 2 {
            return None;
        }
        let payload = position + 4;
        if marker == 0xE1 && data.get(payload..payload + 6)? == b"Exif\0\0" {
            return Some(payload + 6);
        }
        position = payload + length - 2;
    }
    None
}

/// IFD 里的一条：标签、类型、个数、值（或值的偏移）。
struct Entry {
    tag: u16,
    value: u32,
}

fn tiff_u32(data: &[u8], at: usize, le: bool) -> Option<u32> {
    if le {
        u32_le(data, at)
    } else {
        u32_be(data, at)
    }
}

fn tiff_u16(data: &[u8], at: usize, le: bool) -> Option<u16> {
    let bytes: [u8; 2] = data.get(at..at + 2)?.try_into().ok()?;
    Some(if le {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    })
}

/// 读一个 IFD：前面两个字节是条目数，后面每条 12 字节。
fn tiff_ifd(data: &[u8], base: usize, at: usize, le: bool) -> Option<Vec<Entry>> {
    let count = tiff_u16(data, base + at, le)?;
    let mut out = Vec::with_capacity(count as usize);
    for index in 0..count {
        let entry = base + at + 2 + index as usize * 12;
        out.push(Entry {
            tag: tiff_u16(data, entry, le)?,
            value: tiff_u32(data, entry + 8, le)?,
        });
    }
    Some(out)
}

fn tiff_value(entries: &[Entry], tag: u16) -> Option<u32> {
    entries
        .iter()
        .find(|entry| entry.tag == tag)
        .map(|entry| entry.value)
}

/// 取一个 ASCII 型的标签值。
///
/// 值可能就嵌在那 4 个字节里（短的），也可能在别处（长的）：
/// EXIF 规定超过 4 字节就存偏移，我们两种都认。
fn tiff_text(data: &[u8], base: usize, entries: &[Entry], tag: u16) -> Option<String> {
    let entry = entries.iter().find(|entry| entry.tag == tag)?;
    let at = if entry.value <= 4 {
        // 短值在使用处内联：这里拿不到（需要条目的偏移），退化成跳过。
        return None;
    } else {
        base + usize::try_from(entry.value).ok()?
    };
    let text: String = data
        .get(at..)?
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as char)
        .collect();
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `2024:05:01 12:34:56` → `2024-05-01 12:34`：
/// 冒号分隔的日期只有 EXIF 这么写，用户看的是普通日期。
fn tidy_date(raw: &str) -> String {
    let mut out = raw.to_string();
    if out.len() >= 10 {
        let date = out[..10].replace(':', "-");
        out.replace_range(..10, &date);
    }
    if out.len() > 16 {
        out.truncate(16);
    }
    out
}

// ---- ID3 ----

/// MP3 的 ID3：先找 v2（文件头），没有就退回 v1（文件尾 128 字节）。
fn id3(path: &Path) -> Option<String> {
    let head = read_head(path, HEAD_BYTES)?;
    if let Some(text) = id3v2(&head) {
        return Some(text);
    }
    let mut file = fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    if size < 128 {
        return None;
    }
    file.seek(SeekFrom::End(-128)).ok()?;
    let mut tail = [0u8; 128];
    file.read_exact(&mut tail).ok()?;
    id3v1(&tail)
}

fn id3v2(data: &[u8]) -> Option<String> {
    if !data.starts_with(b"ID3") {
        return None;
    }
    let version = *data.get(3)?;
    if !(2..=4).contains(&version) {
        // 只认 v2.2 / v2.3 / v2.4：别的版本号按读不出来处理。
        return None;
    }
    // 头里的长度是四个 7 位拼起来的（syncsafe），别当普通整数读。
    let mut size = 0u32;
    for byte in data.get(6..10)? {
        size = (size << 7) | u32::from(*byte & 0x7F);
    }
    let end = (10 + size as usize).min(data.len());

    let mut position = 10;
    let (mut title, mut artist, mut album) = (None, None, None);
    while position + 10 <= end {
        let frame = data.get(position..position + 4)?;
        // 帧 id 只能是大写字母与数字：读到别的就说明到填充区了。
        if !frame
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            break;
        }
        let mut length = u32::from_be_bytes(data.get(position + 4..position + 8)?.try_into().ok()?);
        // v2.4 把帧长度也改成 syncsafe 了；v2.3 及更早是普通整数。
        if version == 4 {
            let mut plain = 0u32;
            for byte in data.get(position + 4..position + 8)? {
                plain = (plain << 7) | u32::from(*byte & 0x7F);
            }
            length = plain;
        }
        let payload = position + 10;
        let next = payload + length as usize;
        if length == 0 || next > end {
            break;
        }
        let text = frame_text(data.get(payload..next)?);
        match frame {
            b"TIT2" => title = text,
            b"TPE1" => artist = text,
            b"TALB" => album = text,
            _ => {}
        }
        position = next;
    }
    join_tags(&[title, artist, album])
}

/// 一个文本帧：第一个字节是编码，后面是内容。
fn frame_text(frame: &[u8]) -> Option<String> {
    let (encoding, body) = (*frame.first()?, frame.get(1..)?);
    match encoding {
        0 => Some(body.iter().map(|byte| *byte as char).collect()),
        3 => Some(String::from_utf8(body.to_vec()).ok()?),
        _ => {
            // UTF-16：前两个字节可能是 BOM，认出来就按它定字节序。
            let (le, rest) = match (body.first()?, body.get(1)?) {
                (0xFF, 0xFE) => (true, body.get(2..)?),
                (0xFE, 0xFF) => (false, body.get(2..)?),
                _ => (encoding == 1, body),
            };
            let units: Vec<u16> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| {
                    if le {
                        u16::from_le_bytes([pair[0], pair[1]])
                    } else {
                        u16::from_be_bytes([pair[0], pair[1]])
                    }
                })
                .collect();
            Some(
                String::from_utf16(&units)
                    .ok()?
                    .trim_end_matches('\0')
                    .to_string(),
            )
        }
    }
    .map(|text| text.trim().to_string())
    .filter(|text| !text.is_empty())
}

fn id3v1(tail: &[u8]) -> Option<String> {
    if !tail.starts_with(b"TAG") {
        return None;
    }
    let field = |at: usize, to: usize| -> Option<String> {
        let text: String = tail
            .get(at..to)?
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as char)
            .collect();
        let text = text.trim().to_string();
        (!text.is_empty()).then_some(text)
    };
    join_tags(&[field(3, 33), field(33, 63), field(63, 93)])
}

/// 把拿到的几个标签拼成一格文字：曲名 · 艺术家（都没有就退回专辑）。
fn join_tags(parts: &[Option<String>]) -> Option<String> {
    let text = parts
        .iter()
        .filter_map(|part| part.as_ref())
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ");
    (!text.is_empty()).then_some(text)
}

// ---- PDF ----

/// PDF 页数：数 `/Type /Page`（不是 `/Pages`）的个数。
///
/// 现代 PDF 会把对象压缩进对象流，那时候页面对象根本不在明文字节里，
/// 数出来是 0——退回 `/Count N`（目录节点里那个数字），取最大的那个。
fn pdf_pages(path: &Path) -> Option<u32> {
    let data = read_head(path, 32 * 1024 * 1024)?;
    let mut counted = 0u32;
    let mut at = 0;
    while let Some(found) = find(&data[at..], b"/Type") {
        let start = at + found + 5;
        let after = start + spaces(&data[start..]);
        // `/Pages` 也以 `/Page` 开头：后面跟着 s 的是目录节点，不算一页。
        if data.get(after..after + 5) == Some(b"/Page".as_slice())
            && data.get(after + 5) != Some(&b's')
        {
            counted += 1;
        }
        at = after;
    }
    if counted > 0 {
        return Some(counted);
    }
    let mut largest = 0u32;
    let mut at = 0;
    while let Some(found) = find(&data[at..], b"/Count") {
        let start = at + found + 6 + spaces(&data[at + found + 6..]);
        let digits: String = data
            .get(start..)?
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .map(|byte| *byte as char)
            .collect();
        if let Ok(value) = digits.parse::<u32>() {
            largest = largest.max(value);
        }
        at = start + digits.len();
    }
    (largest > 0).then_some(largest)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 从 `at` 开始跳空白：PDF 里 `/Type /Page` 中间可以是空格、换行甚至什么都没有。
fn spaces(data: &[u8]) -> usize {
    data.iter()
        .take_while(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0'))
        .count()
}

// ---- Git ----

/// 一个文件在 Git 里的状态。只记"不干净的"，干净的进列表没意义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitState {
    /// 改过了（还没 `git add`）。
    Modified,
    /// 改过了，而且已经暂存。
    Staged,
    /// 新加进来的（已暂存）。
    Added,
    /// 删掉了。
    Deleted,
    /// 改名了（已暂存）。
    Renamed,
    /// 还没被 Git 管。
    Untracked,
    /// 合并冲突。
    Conflict,
}

impl GitState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Modified => "已改",
            Self::Staged => "已暂存",
            Self::Added => "新增",
            Self::Deleted => "已删",
            Self::Renamed => "改名",
            Self::Untracked => "未跟踪",
            Self::Conflict => "冲突",
        }
    }
}

/// 从 `git status --porcelain` 的一行里读出状态。
///
/// 前两个字符是XY：X 是暂存区相对 HEAD，Y 是工作区相对暂存区。
/// 两边都可能动过，取"更该让用户先看见"的那个。
pub fn git_state(line: &str) -> Option<(String, GitState)> {
    let bytes = line.as_bytes();
    if bytes.len() < 4 {
        return None;
    }
    let (x, y) = (bytes[0], bytes[1]);
    let state = if x == b'U' || y == b'U' || (x == y && matches!(x, b'A' | b'D')) {
        // 两边都加了 / 都删了 / 任一边标记为 unmerged：都是冲突。
        GitState::Conflict
    } else if x == b'?' && y == b'?' {
        GitState::Untracked
    } else if x == b'!' {
        // 被 .gitignore 忽略的不算"要处理的事"。
        return None;
    } else {
        match (x, y) {
            (b'A', _) | (_, b'A') => GitState::Added,
            (b'R', _) | (_, b'R') => GitState::Renamed,
            (b'D', _) | (_, b'D') => GitState::Deleted,
            (b'M', _) => GitState::Staged,
            (_, b'M') => GitState::Modified,
            _ => return None,
        }
    };
    let mut path = line.get(3..)?.to_string();
    // 改名是 `R  old -> new`：用户看到的是移动后那个文件。
    if let Some((_, moved)) = path.rsplit_once(" -> ") {
        path = moved.to_string();
    }
    let path = unquote(&path);
    if path.is_empty() {
        return None;
    }
    Some((path, state))
}

/// 去掉 Git 给特殊字符加的引号（`core.quotepath` 关掉后只剩极少数情况）。
fn unquote(text: &str) -> String {
    if !text.starts_with('"') || !text.ends_with('"') {
        return text.to_string();
    }
    let body = &text[1..text.len() - 1];
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => {}
        }
    }
    out
}

/// 从 `path` 往上找仓库根：有 `.git`（目录或文件——worktree 里是文件）。
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let mut dir = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    while let Some(current) = dir {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        dir = current.parent();
    }
    None
}

/// 一次 `git status` 换回整个仓库的状态表。
///
/// 路径是**相对仓库根**的（实测如此），回到调用方手里再拼成绝对路径。
/// 跑的是 git 命令行而不是解析 `.git`：后者得自己比 index、比 HEAD、
/// 还算内容哈希，写错一处就是"状态显示错了"，这一类错最难被发现。
pub fn git_status(root: &Path) -> Option<HashMap<PathBuf, GitState>> {
    let output = std::process::Command::new("git")
        // `-C` 直接站到仓库根上，省一层 cwd 切换；
        // `quotepath=false` 让中文路径按原样出来，不用解八进制转义。
        .args([
            "-C",
            &root.to_string_lossy(),
            "-c",
            "core.quotepath=false",
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--ignore-submodules=all",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some((path, state)) = git_state(line) else {
            continue;
        };
        out.insert(root.join(path), state);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// 缓存实体
// ---------------------------------------------------------------------------

/// 后台算好了：哪一格该更新了。
#[derive(Clone, Debug)]
pub enum MetadataEvent {
    /// 某个文件的一项元数据算出来了。
    Ready(PathBuf),
    /// 一个仓库的 Git 状态整批回来了（一次调用回来一批，不是一个文件一个）。
    GitReady(PathBuf),
}

/// 一个文件"上次看的时候"的样子：改了就得重探。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<(u64, u32)>,
    size: u64,
}

impl Stamp {
    fn of(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        let modified = metadata.modified().ok().and_then(|time| {
            let elapsed = time.duration_since(UNIX_EPOCH).ok()?;
            Some((elapsed.as_secs(), elapsed.subsec_nanos()))
        });
        Some(Self {
            modified,
            size: metadata.len(),
        })
    }
}

struct Cached {
    stamp: Stamp,
    text: Option<String>,
}

/// 一个仓库的状态。
struct Repo {
    states: HashMap<PathBuf, GitState>,
}

impl Repo {
    /// 这个目录下（含子目录）有多少项不干净。
    fn count_under(&self, dir: &Path) -> usize {
        self.states
            .iter()
            .filter(|(path, _)| path.starts_with(dir))
            .count()
    }
}

pub struct Metadata {
    /// 当前显示哪一列；跟着设置走，改了就清空缓存。
    field: Option<Field>,
    values: HashMap<PathBuf, Cached>,
    /// 正在算的，别重复排队。
    pending: HashSet<PathBuf>,
    /// 仓库根 → 状态表。
    git: HashMap<PathBuf, Repo>,
    git_pending: HashSet<PathBuf>,
    /// 这个根上 git 用不了（没装 git / 不是仓库 / 调用失败）：
    /// 记下来，别每行渲染都去试一次。
    git_failed: HashSet<PathBuf>,
    /// 目录 → 它的仓库根。往上找 `.git` 要碰文件系统，
    /// 每行都找一遍太贵，同一个目录记一次就够。
    roots: HashMap<PathBuf, Option<PathBuf>>,
}

impl EventEmitter<MetadataEvent> for Metadata {}

impl Default for Metadata {
    fn default() -> Self {
        Self::new()
    }
}

impl Metadata {
    pub fn new() -> Self {
        Self {
            field: None,
            values: HashMap::new(),
            pending: HashSet::new(),
            git: HashMap::new(),
            git_pending: HashSet::new(),
            git_failed: HashSet::new(),
            roots: HashMap::new(),
        }
    }

    /// 当前显示哪一列；没有就是这一列不出现。
    ///
    /// 顺手跟设置对一次：表头比第一行先画，不对一次的话
    /// 第一帧表头会少一格，列就歪了。
    pub fn field(&mut self, cx: &mut Context<Self>) -> Option<Field> {
        self.sync(cx);
        self.field
    }

    /// 这一格该显示什么。
    ///
    /// 返回 `None` 表示"这一列现在不显示"（界面上整列不画）；
    /// 返回 `Some("")` 表示"这一列在，但这个文件或还没算出来、或不适用"。
    /// 分得这么清是为了让列表的列宽稳定：否则每行少一个格子会左右跳。
    pub fn text(&mut self, path: &Path, is_dir: bool, cx: &mut Context<Self>) -> Option<String> {
        self.sync(cx);
        let field = self.field?;
        if field == Field::Git {
            return Some(self.git_text(path, is_dir, cx).unwrap_or_default());
        }
        if !field.applies(path, is_dir) {
            return Some(String::new());
        }
        let Some(stamp) = Stamp::of(path) else {
            return Some(String::new());
        };
        if let Some(cached) = self.values.get(path)
            && cached.stamp == stamp
        {
            return Some(cached.text.clone().unwrap_or_default());
        }
        if !self.pending.contains(path) {
            self.pending.insert(path.to_path_buf());
            self.spawn_value(field, path.to_path_buf(), is_dir, stamp, cx);
        }
        Some(String::new())
    }

    /// 跟着设置走：用户换了列，旧值全作废。
    fn sync(&mut self, cx: &mut Context<Self>) {
        let field = Settings::peek(cx, |settings| Field::from_id(&settings.meta_column));
        if field == self.field {
            return;
        }
        self.field = field;
        self.values.clear();
        self.pending.clear();
        cx.notify();
    }

    fn spawn_value(
        &mut self,
        field: Field,
        path: PathBuf,
        is_dir: bool,
        stamp: Stamp,
        cx: &mut Context<Self>,
    ) {
        let (tx, rx) = async_channel::bounded(1);
        let worker = path.clone();
        let spawn = std::thread::Builder::new()
            .name("wfiles-meta".to_string())
            .spawn(move || {
                let result = probe(field, &worker, is_dir);
                // 收的人已经关掉面板了就无所谓，丢掉即可。
                tx.try_send(result).ok();
            });
        if let Err(error) = spawn {
            // 起不了线程（进程里线程太多）就当场算一次：
            // 慢一点，但这一格不会永远空着。
            //
            // 记进日志而不是 `eprintln!`：从文件管理器启动的进程
            // 根本没有终端可写，那行字只进 devtools 的 stderr，
            // 用户那边一个字也看不到——而"这一格一直空着"
            // 正是靠这句话才能定位。
            crate::logfile::warn(&format!("元数据：起不了后台线程（{error}），就地算一次"));
            let text = probe(field, &path, is_dir);
            self.finish(path, stamp, text, cx);
            return;
        }

        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let mut cx = cx.clone();
            let Ok(text) = rx.recv().await else {
                return;
            };
            this.update(&mut cx, |meta, cx| meta.finish(path, stamp, text, cx))
                .ok();
        })
        .detach();
    }

    fn finish(
        &mut self,
        path: PathBuf,
        stamp: Stamp,
        text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.pending.remove(&path);
        // 缓存满了就整包清掉：最坏是重算一遍，好过无限涨。
        if self.values.len() >= CAPACITY {
            self.values.clear();
        }
        self.values.insert(path.clone(), Cached { stamp, text });
        cx.emit(MetadataEvent::Ready(path));
        cx.notify();
    }

    fn git_text(&mut self, path: &Path, is_dir: bool, cx: &mut Context<Self>) -> Option<String> {
        let root = self.root_of(path)?;
        if let Some(repo) = self.git.get(&root) {
            if let Some(state) = repo.states.get(path) {
                return Some(state.label().to_string());
            }
            // 目录 Git 不单独列：把它底下改了多少项汇总出来，
            // 一眼能看出"这一片有东西动了"。
            if is_dir {
                let count = repo.count_under(path);
                return (count > 0).then(|| format!("{count} 项改动"));
            }
            return None;
        }
        if self.git_pending.contains(&root)
            || self.git_failed.contains(&root)
            || !self.git_pending.insert(root.clone())
        {
            return None;
        }
        self.spawn_git(root, cx);
        None
    }

    /// 这个路径属于哪个仓库。同一个目录只找一次。
    fn root_of(&mut self, path: &Path) -> Option<PathBuf> {
        let dir = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()?.to_path_buf()
        };
        if let Some(cached) = self.roots.get(&dir) {
            return cached.clone();
        }
        let root = repo_root(&dir);
        self.roots.insert(dir, root.clone());
        root
    }

    fn spawn_git(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let (tx, rx) = async_channel::bounded(1);
        let worker = root.clone();
        let spawn = std::thread::Builder::new()
            .name("wfiles-git".to_string())
            .spawn(move || {
                tx.try_send(git_status(&worker)).ok();
            });
        if let Err(error) = spawn {
            crate::logfile::warn(&format!(
                "Git 状态（{}）：起不了后台线程（{error}），就地算一次",
                root.display()
            ));
            let states = git_status(&root);
            self.finish_git(root, states, cx);
            return;
        }
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let mut cx = cx.clone();
            let Ok(states) = rx.recv().await else {
                return;
            };
            this.update(&mut cx, |meta, cx| meta.finish_git(root, states, cx))
                .ok();
        })
        .detach();
    }

    fn finish_git(
        &mut self,
        root: PathBuf,
        states: Option<HashMap<PathBuf, GitState>>,
        cx: &mut Context<Self>,
    ) {
        self.git_pending.remove(&root);
        match states {
            Some(states) => {
                self.git.insert(root.clone(), Repo { states });
                cx.emit(MetadataEvent::GitReady(root));
            }
            None => {
                self.git_failed.insert(root);
            }
        }
        cx.notify();
    }

    /// 文件动过了，Git 状态就得重来：复制、移动、删除都会让它变。
    ///
    /// 不清仓库根的查找缓存（`.git` 不会跑），也不清各文件的探测值
    /// （它们按 mtime 自己会失效）。
    pub fn invalidate_git(&mut self) {
        self.git.clear();
        self.git_failed.clear();
    }

    /// 缓存里已经有多少条，测试用。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.values.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // ---- 字段 ----

    #[test]
    fn field_ids_round_trip() {
        for field in Field::ALL {
            assert_eq!(Field::from_id(field.id()), Some(field));
        }
        assert_eq!(Field::from_id("nope"), None);
    }

    #[test]
    fn applicability_is_decided_by_the_extension() {
        let root = temp();
        let image = root.path().join("pic.PNG");
        let song = root.path().join("song.mp3");
        let doc = root.path().join("doc.pdf");
        let text = root.path().join("notes.txt");
        let folder = root.path().join("folder");

        assert!(Field::Image.applies(&image, false), "后缀大写也要认");
        assert!(!Field::Image.applies(&text, false));
        assert!(Field::Id3.applies(&song, false));
        assert!(!Field::Id3.applies(&image, false));
        assert!(Field::Pages.applies(&doc, false));
        assert!(Field::Duration.applies(&root.path().join("a.mkv"), false));
        // 目录只有 Git 有话说。
        assert!(Field::Git.applies(&folder, true));
        assert!(!Field::Image.applies(&folder, true));
        assert!(!Field::Pages.applies(&folder, true));
    }

    // ---- 图片尺寸 ----

    #[test]
    fn a_real_png_reports_its_pixels() {
        let root = temp();
        let path = root.path().join("pic.png");
        let buffer = image::RgbImage::from_fn(320, 180, |x, y| image::Rgb([x as u8, y as u8, 128]));
        buffer.save(&path).unwrap();

        assert_eq!(
            probe(Field::Image, &path, false).as_deref(),
            Some("320 × 180")
        );
    }

    #[test]
    fn a_text_file_is_not_probed_as_an_image() {
        let root = temp();
        let path = root.path().join("a.txt");
        fs::write(&path, "hello").unwrap();
        assert_eq!(probe(Field::Image, &path, false), None);
    }

    // ---- 时长 ----

    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn mp4(timescale: u32, duration: u32) -> Vec<u8> {
        let mut mvhd = vec![0u8, 0, 0, 0];
        mvhd.extend_from_slice(&[0u8; 4]);
        mvhd.extend_from_slice(&[0u8; 4]);
        mvhd.extend_from_slice(&timescale.to_be_bytes());
        mvhd.extend_from_slice(&duration.to_be_bytes());
        let mut out = boxed(b"ftyp", &[0u8; 8]);
        out.extend_from_slice(&boxed(b"moov", &boxed(b"mvhd", &mvhd)));
        out
    }

    #[test]
    fn an_mp4_reports_its_duration() {
        let root = temp();
        let path = root.path().join("clip.mp4");
        fs::write(&path, mp4(1000, 5000)).unwrap();
        assert_eq!(duration(&path), Some(5.0));
        assert_eq!(
            probe(Field::Duration, &path, false).as_deref(),
            Some("0:05")
        );
    }

    #[test]
    fn a_mp4_whose_moov_is_missing_gives_up_quietly() {
        let root = temp();
        let path = root.path().join("broken.mp4");
        fs::write(&path, boxed(b"ftyp", &[0u8; 8])).unwrap();
        assert_eq!(duration(&path), None);
        assert_eq!(probe(Field::Duration, &path, false), None);
    }

    fn wav(data_bytes: u32, byte_rate: u32) -> Vec<u8> {
        let mut fmt = vec![1u8, 0, 2, 0];
        fmt.extend_from_slice(&44100u32.to_le_bytes());
        fmt.extend_from_slice(&byte_rate.to_le_bytes());
        fmt.extend_from_slice(&[4u8, 0, 16, 0]);
        let data = vec![0u8; data_bytes as usize];
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&36u32.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        // RIFF 的块是小端的（MP4 那套盒子是大端的，别混用）。
        out.extend_from_slice(&chunk_le(b"fmt ", &fmt));
        out.extend_from_slice(&chunk_le(b"data", &data));
        out
    }

    /// 小端的 RIFF 块：类型在前，长度在后。
    fn chunk_le(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = kind.to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn a_wav_reports_its_duration() {
        let root = temp();
        let path = root.path().join("sound.wav");
        fs::write(&path, wav(176_400, 176_400)).unwrap();
        assert_eq!(duration(&path), Some(1.0));
    }

    /// 造一个 EBML 元素：id 后面跟长度、再跟内容。
    fn ebml(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut size = (payload.len() as u32).to_be_bytes().to_vec();
        // 长度一律用 4 个字节的变长整数：合法，只是不是最短写法。
        size[0] |= 0x10;
        let mut out = id.to_vec();
        out.extend_from_slice(&size);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn a_webm_reports_its_duration() {
        let root = temp();
        let path = root.path().join("clip.webm");
        let mut info = ebml(&[0x2A, 0xD7, 0xB1], &1_000_000u32.to_be_bytes());
        // Duration 是个浮点数：5000 个刻度 × 每刻度 1 毫秒 = 5 秒。
        info.extend_from_slice(&ebml(&[0x44, 0x89], &5000.0f64.to_be_bytes()));
        let mut out = ebml(&[0x1A, 0x45, 0xDF, 0xA3], &[]);
        out.extend_from_slice(&ebml(
            &[0x18, 0x53, 0x80, 0x67],
            &ebml(&[0x15, 0x49, 0xA9, 0x66], &info),
        ));
        fs::write(&path, out).unwrap();

        assert_eq!(duration(&path), Some(5.0));
        assert_eq!(
            probe(Field::Duration, &path, false).as_deref(),
            Some("0:05")
        );
    }

    #[test]
    fn durations_are_printed_the_way_people_read_them() {
        assert_eq!(format_duration(5.0), "0:05");
        assert_eq!(format_duration(65.4), "1:05");
        assert_eq!(format_duration(3725.0), "1:02:05");
        assert_eq!(format_duration(-1.0), "", "负数不是时长");
        assert_eq!(format_duration(f64::NAN), "");
    }

    // ---- EXIF ----

    /// 造一张只有 SOI + APP1(EXIF) + EOI 的"照片"：像素一段都没有，
    /// 但 EXIF 该读得出来——我们要测的正是那一段。
    fn jpeg_with_exif(date: &str) -> Vec<u8> {
        let mut tiff = b"II".to_vec();
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0132u16.to_le_bytes()); // DateTime
        tiff.extend_from_slice(&2u16.to_le_bytes()); // ASCII
        tiff.extend_from_slice(&20u32.to_le_bytes());
        tiff.extend_from_slice(&26u32.to_le_bytes()); // 值放在 IFD 后面
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut text = date.as_bytes().to_vec();
        text.resize(20, 0);
        tiff.extend_from_slice(&text);

        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);

        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    #[test]
    fn exif_reports_the_shot_time() {
        let root = temp();
        let path = root.path().join("photo.jpg");
        fs::write(&path, jpeg_with_exif("2024:05:01 12:34:56")).unwrap();

        assert_eq!(
            probe(Field::Exif, &path, false).as_deref(),
            Some("2024-05-01 12:34"),
            "EXIF 的冒号日期要改成正常人写的日期"
        );
    }

    #[test]
    fn a_jpeg_without_exif_is_left_blank() {
        let root = temp();
        let path = root.path().join("photo.jpg");
        fs::write(&path, [0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
        assert_eq!(probe(Field::Exif, &path, false), None);
    }

    #[test]
    fn exif_dates_are_tidied_up() {
        assert_eq!(tidy_date("2024:05:01 12:34:56"), "2024-05-01 12:34");
        assert_eq!(tidy_date(""), "");
    }

    // ---- ID3 ----

    fn id3_frame(id: &[u8; 4], text: &str) -> Vec<u8> {
        let mut payload = vec![3u8]; // UTF-8
        payload.extend_from_slice(text.as_bytes());
        let mut out = id.to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&payload);
        out
    }

    fn syncsafe(value: u32) -> [u8; 4] {
        [
            (value >> 21) as u8 & 0x7F,
            (value >> 14) as u8 & 0x7F,
            (value >> 7) as u8 & 0x7F,
            value as u8 & 0x7F,
        ]
    }

    fn mp3_with_id3v2(version: u8) -> Vec<u8> {
        let mut frames = id3_frame(b"TIT2", "晴天");
        frames.extend_from_slice(&id3_frame(b"TPE1", "周杰伦"));
        let mut out = b"ID3".to_vec();
        out.extend_from_slice(&[version, 0, 0]);
        out.extend_from_slice(&syncsafe(frames.len() as u32));
        out.extend_from_slice(&frames);
        out.extend_from_slice(&[0u8; 64]); // 假装后面是音频
        out
    }

    #[test]
    fn id3v2_reports_the_title_and_artist() {
        let root = temp();
        let path = root.path().join("song.mp3");
        fs::write(&path, mp3_with_id3v2(3)).unwrap();

        assert_eq!(
            probe(Field::Id3, &path, false).as_deref(),
            Some("晴天 · 周杰伦")
        );
    }

    #[test]
    fn id3v2_4_uses_syncsafe_frame_sizes() {
        let root = temp();
        let path = root.path().join("song.mp3");
        let frames = id3_frame(b"TIT2", "晴天");
        let mut out = b"ID3".to_vec();
        out.extend_from_slice(&[4u8, 0, 0]);
        out.extend_from_slice(&syncsafe(frames.len() as u32));
        // v2.4 的帧长度也是 syncsafe 的，按普通整数读会读错。
        let mut frame = frames.clone();
        frame[4..8].copy_from_slice(&syncsafe((frames.len() - 10) as u32));
        out.extend_from_slice(&frame);
        fs::write(&path, out).unwrap();

        assert_eq!(probe(Field::Id3, &path, false).as_deref(), Some("晴天"));
    }

    fn mp3_with_id3v1() -> Vec<u8> {
        let mut tail = b"TAG".to_vec();
        let mut title = b"Blue".to_vec();
        title.resize(30, 0);
        let mut artist = b"Someone".to_vec();
        artist.resize(30, 0);
        tail.extend_from_slice(&title);
        tail.extend_from_slice(&artist);
        tail.resize(128, 0);
        let mut out = vec![0u8; 64];
        out.extend_from_slice(&tail);
        out
    }

    #[test]
    fn id3v1_at_the_end_of_the_file_is_found_too() {
        let root = temp();
        let path = root.path().join("old.mp3");
        fs::write(&path, mp3_with_id3v1()).unwrap();

        assert_eq!(
            probe(Field::Id3, &path, false).as_deref(),
            Some("Blue · Someone")
        );
    }

    #[test]
    fn a_mp3_without_tags_is_left_blank() {
        let root = temp();
        let path = root.path().join("bare.mp3");
        fs::write(&path, [0u8; 256]).unwrap();
        assert_eq!(probe(Field::Id3, &path, false), None);
    }

    // ---- PDF ----

    #[test]
    fn pdf_pages_are_counted() {
        let root = temp();
        let path = root.path().join("doc.pdf");
        fs::write(
            &path,
            b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n\
             2 0 obj<</Type/Page>>endobj\n\
             3 0 obj<</Type /Page>>endobj\n\
             4 0 obj<</Type/Pages/Count 3>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF",
        )
        .unwrap();

        assert_eq!(
            probe(Field::Pages, &path, false).as_deref(),
            Some("2 页"),
            "目录节点（/Pages）不能算成一页"
        );
    }

    #[test]
    fn a_pdf_with_compressed_objects_falls_back_to_the_count() {
        let root = temp();
        let path = root.path().join("modern.pdf");
        // 现代 PDF 把页面对象压进对象流：明文字里一个 /Type /Page 都没有，
        // 只剩 /Count 还看得见。
        fs::write(
            &path,
            b"%PDF-1.7\n%fake compressed objects\n/Count 42\n%%EOF",
        )
        .unwrap();

        assert_eq!(probe(Field::Pages, &path, false).as_deref(), Some("42 页"));
    }

    // ---- Git ----

    #[test]
    fn porcelain_lines_become_paths_and_states() {
        assert_eq!(
            git_state(" M src/app.rs"),
            Some(("src/app.rs".to_string(), GitState::Modified))
        );
        assert_eq!(
            git_state("M  src/app.rs"),
            Some(("src/app.rs".to_string(), GitState::Staged))
        );
        assert_eq!(
            git_state("?? new.txt"),
            Some(("new.txt".to_string(), GitState::Untracked))
        );
        assert_eq!(
            git_state("A  added.txt"),
            Some(("added.txt".to_string(), GitState::Added))
        );
        assert_eq!(
            git_state("R  old.txt -> new.txt"),
            Some(("new.txt".to_string(), GitState::Renamed)),
            "改名要看的是移动后那个名字"
        );
        assert_eq!(
            git_state("UU both.txt"),
            Some(("both.txt".to_string(), GitState::Conflict))
        );
        assert_eq!(git_state("!! ignored.txt"), None, "忽略的不算待办");
        assert_eq!(git_state(""), None, "空行不算");
    }

    #[test]
    fn quoted_paths_are_unwrapped() {
        assert_eq!(unquote("plain.txt"), "plain.txt");
        assert_eq!(unquote("\"a b.txt\""), "a b.txt");
        assert_eq!(unquote("\"tab\\there\""), "tab\there");
    }

    #[test]
    fn the_repo_root_is_found_by_walking_up() {
        let root = temp();
        let repo = root.path().join("repo");
        let deep = repo.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();

        assert_eq!(repo_root(&deep), Some(repo.clone()));
        let file = deep.join("c.txt");
        fs::write(&file, "x").unwrap();
        assert_eq!(
            repo_root(&file),
            Some(repo.clone()),
            "文件要从它的目录往上找"
        );
    }

    #[test]
    fn outside_a_repo_there_is_no_root() {
        // 临时目录不在任何仓库里（真在仓库里的机器上这条会跳过）。
        let root = temp();
        if repo_root(root.path()).is_some() {
            return;
        }
        assert_eq!(repo_root(root.path()), None);
    }

    #[test]
    fn git_status_reads_a_real_repo() {
        let root = temp();
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .output()
            .is_ok_and(|output| output.status.success());
        if !ok {
            // 没装 git 的地方别把测试跑红。
            return;
        }
        fs::write(repo.join("new.txt"), "hello").unwrap();

        let states = git_status(&repo).expect("该读出状态");
        assert_eq!(
            states.get(&repo.join("new.txt")),
            Some(&GitState::Untracked)
        );
    }

    // ---- 缓存实体 ----

    #[test]
    fn an_entity_starts_empty() {
        let meta = Metadata::new();
        assert_eq!(meta.len(), 0);
        assert_eq!(meta.field, None);
    }
}
