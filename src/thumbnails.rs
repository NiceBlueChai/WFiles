//! 缩略图：后台解码 + 有上限的缓存。
//!
//! 列表要的是"滚到哪儿都能立刻画出来"，所以解码一律不在 UI 线程做：
//! 每个新请求丢给一个普通线程，做完了把 [`Thumbnail`] 送回实体，
//! 实体发一个 [`ThumbnailsEvent`]，订阅它的窗格重画一次就能看到。
//!
//! 缓存键带上了修改时间和大小：文件一改，旧图自动失效，
//! 不会出现"我明明换过图，缩略图还是旧的"这种最难解释的 bug。
//!
//! 缓存本身（[`Cache`]）是不认识 GPUI 的纯数据结构，淘汰策略可以单独测；
//! 只有外面那层 [`Thumbnails`] 实体才碰 `cx.spawn`。

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use gpui_kit::{AsyncApp, Context, EventEmitter, RenderImage, WeakEntity};

/// 列表里的小图边长（像素，逻辑尺寸）。
pub const LIST_EDGE: u32 = 40;
/// 网格视图里的图边长：格子里的缩略图是 64 逻辑像素，按 2 倍屏取。
pub const GRID_EDGE: u32 = 128;
/// Quick Look 里的大图边长。
pub const PREVIEW_EDGE: u32 = 512;

/// 缓存里最多留多少张。40px 的小图一张约 6 KB（RGBA），
/// 这个上限把常驻内存压在 10 MB 量级。
const CAPACITY: usize = 2048;

/// 缓存键。除了路径，还带上了修改时间和大小——
/// 同一个路径换了内容必须重新生成。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    path: PathBuf,
    /// 修改时间（秒, 纳秒）；取不到就记 `None`，那种文件系统上只能靠大小兜底。
    modified: Option<(u64, u32)>,
    size: u64,
    edge: u32,
}

/// 一张生成好的缩略图。
#[derive(Clone)]
pub struct Thumbnail {
    /// 可以直接喂给 `img()` 的 GPUI 图像。
    pub image: Arc<RenderImage>,
    /// 原图的宽高——界面要显示"1920 × 1080"；
    /// 缩略图自己的尺寸问 `image.size(0)` 就行，不用另存一份。
    pub source_width: u32,
    pub source_height: u32,
}

/// 广播：某张图生成好了，重画一下就能看到。
#[derive(Clone, Debug)]
pub enum ThumbnailsEvent {
    Ready(PathBuf),
}

/// 纯数据结构：只管存和淘汰，不认识 GPUI。
struct Cache {
    ready: HashMap<Key, Thumbnail>,
    /// 生成顺序，超容量时从队头淘汰。
    order: VecDeque<Key>,
    capacity: usize,
}

impl Cache {
    fn new(capacity: usize) -> Self {
        Self {
            ready: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn get(&self, key: &Key) -> Option<Thumbnail> {
        self.ready.get(key).cloned()
    }

    fn insert(&mut self, key: Key, thumbnail: Thumbnail) {
        // 同一个 key 重复进来（理论上不会，pending 挡住了）就先去掉旧的位置。
        if self.ready.contains_key(&key) {
            return;
        }
        while self.ready.len() >= self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.ready.remove(&oldest);
        }
        self.order.push_back(key.clone());
        self.ready.insert(key, thumbnail);
    }
}

pub struct Thumbnails {
    cache: Cache,
    /// 正在生成的，别重复排队。
    pending: HashSet<Key>,
    /// 解不出来的（文件坏了、格式不支持）。记下来，
    /// 否则每渲染一行都要重新启一个注定失败的线程。
    failed: HashSet<Key>,
}

impl EventEmitter<ThumbnailsEvent> for Thumbnails {}

impl Default for Thumbnails {
    fn default() -> Self {
        Self::new()
    }
}

impl Thumbnails {
    pub fn new() -> Self {
        Self {
            cache: Cache::new(CAPACITY),
            pending: HashSet::new(),
            failed: HashSet::new(),
        }
    }

    /// 要一张缩略图。
    ///
    /// 有现成的直接给；没有就登记一个后台任务并返回 `None`——
    /// 调用方先画占位图标，图好了会收到 [`ThumbnailsEvent::Ready`]。
    /// 整个调用是同步且便宜的，可以放心在渲染每一行的时候问一次。
    pub fn get(&mut self, path: &Path, edge: u32, cx: &mut Context<Self>) -> Option<Thumbnail> {
        if !decodable(path) {
            return None;
        }

        // 取不到元数据（文件没了、没权限）就没有键，也就无从缓存。
        let key = key_of(path, edge)?;
        if let Some(hit) = self.cache.get(&key) {
            return Some(hit);
        }
        if self.pending.contains(&key) || self.failed.contains(&key) {
            return None;
        }

        self.pending.insert(key.clone());
        Self::spawn(key, cx);
        None
    }

    fn spawn(key: Key, cx: &mut Context<Self>) {
        let (tx, rx) = async_channel::bounded(1);
        let path = key.path.clone();
        let edge = key.edge;
        // 解码是纯 CPU 活，放进普通线程：UI 线程全程不碰它。
        std::thread::spawn(move || {
            let result = decode(&path, edge);
            // 接收端已经没了就说明这次请求作废了（比如文件被删），无所谓。
            tx.try_send(result).ok();
        });

        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let mut cx = cx.clone();
            if let Ok(result) = rx.recv().await {
                this.update(&mut cx, |thumbs, cx| thumbs.finish(key, result, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn finish(&mut self, key: Key, result: Option<Thumbnail>, cx: &mut Context<Self>) {
        self.pending.remove(&key);
        match result {
            Some(thumbnail) => {
                self.cache.insert(key.clone(), thumbnail);
                cx.emit(ThumbnailsEvent::Ready(key.path));
            }
            None => self.remember_failure(key),
        }
        cx.notify();
    }

    fn remember_failure(&mut self, key: Key) {
        // 失败的 key 也别无限攒：攒多了直接清掉，最坏是重试一次。
        if self.failed.len() >= CAPACITY {
            self.failed.clear();
        }
        self.failed.insert(key);
    }

    /// 缓存里已经有多少张，测试用。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.cache.ready.len()
    }
}

/// 这个扩展名我们解得了么。
///
/// 交给 `image` 自己判断，别手抄一份格式名单——它加新格式我们自动跟上，
/// 它不支持的（比如 SVG，那是矢量格式，走另一条渲染路径）我们直接说解不了，
/// 界面退回通用图标，也不会去启一个注定失败的线程。
fn decodable(path: &Path) -> bool {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .is_some_and(|extension| image::ImageFormat::from_extension(&extension).is_some())
}

fn key_of(path: &Path, edge: u32) -> Option<Key> {
    let metadata = fs::metadata(path).ok()?;
    Some(Key {
        path: path.to_path_buf(),
        modified: moment(metadata.modified().ok()),
        size: metadata.len(),
        edge,
    })
}

/// `SystemTime` 没有实现 `Hash`，转成秒+纳秒进键里。
fn moment(time: Option<SystemTime>) -> Option<(u64, u32)> {
    let elapsed = time?.duration_since(UNIX_EPOCH).ok()?;
    Some((elapsed.as_secs(), elapsed.subsec_nanos()))
}

/// 真正的解码，跑在普通线程上。
///
/// 解码后立刻缩小：先解出 4000×3000 再交给 GPU 缩到 40px 是纯粹的浪费，
/// 那 1200 万像素我们只用到 1600 个。
///
/// 反过来**不放大**：一张 200×100 的图要 512 的大图预览时，
/// 我们给原尺寸，让界面去缩放——放大只会得到一张糊图，还多占内存。
fn decode(path: &Path, edge: u32) -> Option<Thumbnail> {
    let image = image::ImageReader::open(path).ok()?.decode().ok()?;
    let (source_width, source_height) = (image.width(), image.height());
    let scaled = if source_width > edge || source_height > edge {
        image.thumbnail(edge, edge)
    } else {
        image
    };
    let buffer = scaled.to_rgba8();

    Some(Thumbnail {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
        source_width,
        source_height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// 造一张真实的 PNG：宽高不同，好验证缩放保持比例。
    fn write_png(path: &Path, width: u32, height: u32) {
        let buffer =
            image::RgbImage::from_fn(width, height, |x, y| image::Rgb([x as u8, y as u8, 128]));
        buffer.save(path).expect("写测试 PNG");
    }

    fn cache(capacity: usize) -> Cache {
        Cache::new(capacity)
    }

    fn key(name: &str, edge: u32) -> Key {
        Key {
            path: PathBuf::from(name),
            modified: Some((1, 2)),
            size: 3,
            edge,
        }
    }

    fn thumbnail() -> Thumbnail {
        Thumbnail {
            image: Arc::new(RenderImage::new(Vec::<image::Frame>::new())),
            source_width: 1,
            source_height: 1,
        }
    }

    #[test]
    fn a_real_png_decodes_into_a_thumbnail() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("pic.png");
        write_png(&path, 400, 200);

        let result = decode(&path, LIST_EDGE).expect("PNG 应该解得出来");
        assert_eq!(
            (result.source_width, result.source_height),
            (400, 200),
            "要报告原图尺寸，界面显示的是这个"
        );
        let size = result.image.size(0);
        assert_eq!(
            (size.width.0 as u32, size.height.0 as u32),
            (LIST_EDGE, LIST_EDGE / 2),
            "缩略图要保持比例，不能压扁"
        );
    }

    #[test]
    fn a_bigger_edge_gives_a_bigger_thumbnail() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("pic.png");
        write_png(&path, 400, 200);

        let small = decode(&path, LIST_EDGE).unwrap();
        let large = decode(&path, PREVIEW_EDGE).unwrap();
        assert_eq!(
            large.image.size(0).width.0 as u32,
            400,
            "图比请求的小，就不该放大"
        );
        assert!(large.image.size(0).width.0 > small.image.size(0).width.0);
    }

    #[test]
    fn decoding_a_non_image_fails_quietly() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        fs::write(&path, "not an image").unwrap();

        assert!(decode(&path, LIST_EDGE).is_none());
    }

    #[test]
    fn vector_formats_are_reported_as_undecodable() {
        assert!(
            !decodable(Path::new("logo.svg")),
            "SVG 是矢量图，image crate 解不了，别去启线程"
        );
        assert!(!decodable(Path::new("notes.txt")));
        assert!(!decodable(Path::new("noextension")));
        assert!(decodable(Path::new("photo.PNG")), "后缀大写也要认");
        assert!(decodable(Path::new("photo.jpg")));
        assert!(decodable(Path::new("anim.gif")));
        assert!(decodable(Path::new("modern.webp")));
    }

    #[test]
    fn the_cache_hands_back_what_was_inserted() {
        let mut cache = cache(4);
        let key = key("a", LIST_EDGE);
        assert!(cache.get(&key).is_none());

        cache.insert(key.clone(), thumbnail());
        assert!(cache.get(&key).is_some());
    }

    #[test]
    fn the_same_file_at_two_sizes_is_cached_twice() {
        let mut cache = cache(4);
        let small = key("a", LIST_EDGE);
        let large = key("a", PREVIEW_EDGE);
        cache.insert(small.clone(), thumbnail());

        assert!(cache.get(&small).is_some());
        assert!(
            cache.get(&large).is_none(),
            "列表小图和预览大图是两张不同的图"
        );
    }

    #[test]
    fn the_cache_never_grows_past_its_capacity() {
        let mut cache = cache(2);
        cache.insert(key("a", LIST_EDGE), thumbnail());
        cache.insert(key("b", LIST_EDGE), thumbnail());
        cache.insert(key("c", LIST_EDGE), thumbnail());

        assert_eq!(cache.ready.len(), 2, "超容量就得淘汰，不能无限涨");
        assert!(cache.get(&key("a", LIST_EDGE)).is_none(), "最早进来的先走");
        assert!(cache.get(&key("c", LIST_EDGE)).is_some());
        assert_eq!(cache.order.len(), 2, "顺序表也要跟着缩，不然会漏淘汰");
    }

    #[test]
    fn reinserting_the_same_key_does_not_duplicate_it() {
        let mut cache = cache(4);
        let key = key("a", LIST_EDGE);
        cache.insert(key.clone(), thumbnail());
        cache.insert(key.clone(), thumbnail());

        assert_eq!(cache.ready.len(), 1);
        assert_eq!(cache.order.len(), 1);
    }

    #[test]
    fn a_changed_file_gets_a_fresh_key() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("pic.png");
        write_png(&path, 40, 40);
        let before = key_of(&path, LIST_EDGE).expect("键");

        // 改写内容：大小变了，键必须跟着变，否则会一直看到旧图。
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&[0u8; 4096]).unwrap();
        drop(file);

        let after = key_of(&path, LIST_EDGE).expect("键");
        assert_ne!(before, after, "文件改过之后键必须变，不然缩略图不更新");
    }

    #[test]
    fn a_missing_file_has_no_key() {
        let root = tempfile::tempdir().unwrap();
        assert!(key_of(&root.path().join("gone.png"), LIST_EDGE).is_none());
    }

    #[test]
    fn an_entity_starts_empty() {
        let thumbs = Thumbnails::new();
        assert_eq!(thumbs.len(), 0);
        assert!(thumbs.pending.is_empty());
    }

    #[test]
    fn undecodable_files_are_never_queued() {
        let root = tempfile::tempdir().unwrap();
        let thumbs = Thumbnails::new();
        // 没有 `Context` 的情况下走不到 spawn，这里只验证"解不了的在排队之前就被挡住"。
        assert!(!decodable(root.path().join("x.svg").as_path()));
        assert_eq!(thumbs.len(), 0);
    }
}
