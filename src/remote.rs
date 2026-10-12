//! 远程位置（P4）：连接配置、凭据存放，以及各协议的客户端。
//!
//! 现在实现了 WebDAV——它同时也是坚果云、Nextcloud、ownCloud 等一众服务
//! 的接入方式，所以排在最前面。SFTP / FTP / S3 / SMB 按路线图排在其后。
//!
//! 三条纪律：
//! 1. **密码不进配置文件**。配置是明文 JSON，会被同步、会被贴到工单里；
//!    密码只进系统钥匙串，配置里只留"去哪儿拿"的那把钥匙。
//! 2. **客户端一律同步**。调用方本来就在后台线程里，同步代码比"再拖一个
//!    异步运行时进来"好懂得多，出错也少一层。
//! 3. 路径一律是**相对连接根**的字符串，不是 `PathBuf`：远程路径的分隔符
//!    永远是 `/`，而 Windows 的 `PathBuf` 会把 `/` 也认成分隔符又会把 `\`
//!    也认成分隔符——混在一起迟早出事。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 远程位置有哪几种。加协议就在这里加一支，并配一个客户端。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// WebDAV：坚果云、Nextcloud、ownCloud、ownCloud 一类的网盘与自建 NAS。
    WebDav,
}

impl Kind {
    pub const ALL: [Self; 1] = [Self::WebDav];

    pub fn label(self) -> &'static str {
        match self {
            Self::WebDav => "WebDAV",
        }
    }

    /// 地址栏里该填成什么样，给用户一个例子比说一堆规则有用。
    pub fn url_hint(self) -> &'static str {
        match self {
            Self::WebDav => "https://dav.jianguoyun.com/dav/",
        }
    }
}

/// 一条保存下来的远程连接。
///
/// `id` 是稳定的：改显示名、改地址都不会丢掉钥匙串里的密码，
/// 因为密码是按 `id` 存的。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub id: String,
    /// 显示名（侧边栏、命令面板里看到的）。
    pub name: String,
    pub kind: Kind,
    /// 服务器地址，含协议与端口，结尾带不带 `/` 都行。
    pub url: String,
    /// 登录用户名。
    pub user: String,
    /// 进去之后先落在哪个子目录；空就是根。
    #[serde(default)]
    pub root: String,
}

impl Config {
    pub fn new(id: String, name: String, kind: Kind, url: String, user: String) -> Self {
        Self {
            id,
            name,
            kind,
            url,
            user,
            root: String::new(),
        }
    }

    /// 钥匙串里这个密码挂在哪个"账号"下。
    ///
    /// 用 `id` 而不是用户名：同一个服务器开两个账号也分得清。
    pub fn secret_key(&self) -> String {
        format!("remote:{}", self.id)
    }
}

/// 密码只进系统钥匙串，不进配置文件。
///
/// 配置是明文 JSON，会被同步、会被贴到工单里；密码一旦写进去就等于泄露。
/// 三个平台的钥匙串（钥匙串访问 / 凭据管理器 / Secret Service）都覆盖到了，
/// 所以**没有"退而求其次写进设置"这一条**：钥匙串用不了就连不上，
/// 并且把原因原样说给用户听，而不是偷偷降级。
/// 钥匙串里这一条的"服务名"。
const SERVICE: &str = "wfiles";

/// 存密码。存不进去就报错，调用方负责把原因显示出来。
pub fn save_password(config: &Config, password: &str) -> Result<(), String> {
    match keyring::Entry::new(SERVICE, &config.secret_key()) {
        Ok(entry) => match entry.set_password(password) {
            Ok(()) => Ok(()),
            Err(error) => Err(format!("钥匙串写不进去：{error}")),
        },
        Err(error) => Err(format!("打不开钥匙串：{error}")),
    }
}

/// 取密码。没存过、被拒绝访问、这台机器压根没有钥匙串：都是 `None`。
pub fn load_password(config: &Config) -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, &config.secret_key()).ok()?;
    entry.get_password().ok()
}

/// 忘掉这个密码（删连接时调）。钥匙串里没有也不算错。
pub fn forget_password(config: &Config) {
    if let Ok(entry) = keyring::Entry::new(SERVICE, &config.secret_key()) {
        let _ = entry.delete_credential();
    }
}

// ---------------------------------------------------------------------------
// 目录里的一项（远程通用）
// ---------------------------------------------------------------------------

/// 远程目录里的一项。
///
/// 字段跟本地的 `FileEntry` 对齐：`is_dir` / `size` / `modified`，
/// 这样列表视图不必知道它在跟谁说话。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// 相对连接根的路径，分隔符是 `/`，开头没有 `/`。
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// 修改时间（Unix 秒）。服务器不给就没有。
    pub modified: Option<u64>,
}

// ---------------------------------------------------------------------------
// WebDAV
// ---------------------------------------------------------------------------

/// 一个 WebDAV 连接。建好之后可以反复用（HTTP 连接由 `ureq` 复用）。
pub struct Dav {
    /// 服务器根地址，结尾一定有 `/`。
    base: String,
    /// 根地址里的路径部分（如 `/dav/`），用来把服务器回的 href 变回相对路径。
    base_path: String,
    user: String,
    agent: ureq::Agent,
    /// `Authorization` 头的值，建连接时算一次。
    auth: String,
}

impl Dav {
    /// 连一次并顺便验货：能列根目录就算通。
    ///
    /// 不验的话，用户填错一个字母要等到第一次刷新列表才发现，
    /// 那时候的报错（"列不出来"）远不如"连不上"好懂。
    pub fn connect(config: &Config, password: &str) -> Result<Self, String> {
        let dav = Self::new(config, password)?;
        dav.list("")?;
        Ok(dav)
    }

    /// 建一个客户端（不联网，成功不代表地址对）。
    pub fn new(config: &Config, password: &str) -> Result<Self, String> {
        let base = with_slash(&config.url);
        let base_path = path_of(&base);
        // 远程可能很慢，但一直转圈比报错更难排查：
        // 连不上 10 秒、读不到 30 秒就报，不让人干等。
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout_read(std::time::Duration::from_secs(30))
            .build();
        Ok(Self {
            base,
            base_path,
            user: config.user.clone(),
            agent,
            auth: format!("Basic {}", basic(&config.user, password)),
        })
    }

    /// 登录用户名。界面上要提示"当前以谁的身份连着"。
    #[allow(dead_code)]
    pub fn user(&self) -> &str {
        &self.user
    }

    /// 把相对路径拼成完整 URL。
    fn url_of(&self, path: &str) -> String {
        let mut url = self.base.clone();
        url.push_str(&encode(path));
        url
    }

    /// 服务器的 href → 相对连接根的路径。
    ///
    /// 服务器回什么都有：完整的 `https://host/dav/a.txt`、
    /// 只有路径的 `/dav/a.txt`、甚至带着别的主机（代理后面）。
    /// 认得出的前缀就剥掉，认不出就当它是相对路径。
    fn relative(&self, href: &str) -> String {
        let decoded = decode(href);
        let stripped = decoded
            .strip_prefix(&self.base)
            .or_else(|| decoded.strip_prefix(&self.base_path))
            .unwrap_or(decoded.as_str());
        // 目录的 href 常常带个尾巴斜杠；路径开头不该有斜杠。
        stripped.trim_matches('/').to_string()
    }

    /// 列一个目录（`""` 是根）。
    pub fn list(&self, path: &str) -> Result<Vec<Entry>, String> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop>
<d:displayname/><d:getcontentlength/><d:getlastmodified/><d:resourcetype/>
</d:prop></d:propfind>"#;

        let url = self.url_of(path);
        let response = self
            .agent
            .request("PROPFIND", &url)
            .set("Authorization", &self.auth)
            .set("Depth", "1")
            .set("Content-Type", "application/xml; charset=utf-8")
            .send_string(BODY)
            .map_err(|error| explain("列目录", error))?;
        let status = response.status();
        if !(200..300).contains(&status) {
            return Err(format!("列目录：服务器回 {status}"));
        }
        let text = response
            .into_string()
            .map_err(|error| format!("列目录：读不回响应：{error}"))?;
        let entries =
            parse_propfind(text.as_bytes()).map_err(|error| format!("列目录：{error}"))?;

        // 解析出来的是服务器回的原始 href，这里折成"相对连接根"的路径；
        // 第一项是目录自己，列表里不该出现它。
        let myself = path.trim_matches('/');
        Ok(entries
            .into_iter()
            .filter_map(|mut entry| {
                entry.path = self.relative(&entry.path);
                (entry.path != myself).then_some(entry)
            })
            .collect())
    }

    /// 下载一个文件，写进 `sink`，返回写了多少字节。
    pub fn download(&self, path: &str, sink: &mut dyn Write) -> Result<u64, String> {
        let url = self.url_of(path);
        let response = self
            .agent
            .request("GET", &url)
            .set("Authorization", &self.auth)
            .call()
            .map_err(|error| explain("下载", error))?;
        let status = response.status();
        if status != 200 {
            return Err(format!("下载：服务器回 {status}"));
        }
        let mut reader = response.into_reader();
        // 一边收一边写：一个 2 GB 的文件不该先在内存里整个摊开。
        std::io::copy(&mut reader, sink).map_err(|error| format!("下载：写盘失败：{error}"))
    }
}

/// 写操作：上传、建目录、改名、复制、删除。
///
/// 现在界面还没接上（P4 的"远程与本地之间的双栏传输"会用），但客户端
/// 该有的动作先备齐：它们跑的是同一条同步链路和同一套错误翻译，
/// 留到那时候再写等于把 `explain` 重写一遍。
#[allow(dead_code)]
impl Dav {
    /// 上传：整个内容一次性 PUT。远程目录里的文件通常不大，
    /// 真要传大文件也走的是同一条路（调用方已经在后台线程上）。
    pub fn upload(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let url = self.url_of(path);
        self.agent
            .request("PUT", &url)
            .set("Authorization", &self.auth)
            .send_bytes(bytes)
            .map(|_| ())
            .map_err(|error| explain("上传", error))
    }

    /// 建目录。父目录必须已经存在——WebDAV 不会像 `mkdir -p` 那样一路建下去。
    pub fn mkdir(&self, path: &str) -> Result<(), String> {
        self.empty_of("MKCOL", "建目录", path)
    }

    /// 删文件或目录（目录必须是空的：WebDAV 的 DELETE 是"删这一项"）。
    pub fn remove(&self, path: &str) -> Result<(), String> {
        self.empty_of("DELETE", "删除", path)
    }

    /// 改名/移动。`Destination` 必须是完整 URL（RFC 4918 的规定）。
    pub fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.move_of("MOVE", "改名", from, to)
    }

    /// 复制一份（服务器自己拷，不用下载再上传）。
    pub fn copy(&self, from: &str, to: &str) -> Result<(), String> {
        self.move_of("COPY", "复制", from, to)
    }

    /// 发一个没有请求体的动作（MKCOL / DELETE）。
    fn empty_of(&self, method: &str, doing: &str, path: &str) -> Result<(), String> {
        let url = self.url_of(path);
        self.agent
            .request(method, &url)
            .set("Authorization", &self.auth)
            .call()
            .map(|_| ())
            .map_err(|error| explain(doing, error))
    }

    /// MOVE / COPY：目标必须是完整 URL，且默认不覆盖（覆盖与否交给冲突处理）。
    fn move_of(&self, method: &str, doing: &str, from: &str, to: &str) -> Result<(), String> {
        let url = self.url_of(from);
        let destination = self.url_of(to);
        self.agent
            .request(method, &url)
            .set("Authorization", &self.auth)
            .set("Destination", &destination)
            .set("Overwrite", "F")
            .call()
            .map(|_| ())
            .map_err(|error| explain(doing, error))
    }
}

/// 把 `ureq` 的错误翻成人话。
///
/// 它自带的 `Display` 会把状态码、IO 错误、TLS 错误全糊在一起，
/// 用户看到的是一长串看不懂的东西。这里只留最关键的那一条。
fn explain(doing: &str, error: ureq::Error) -> String {
    let detail = match &error {
        ureq::Error::Status(code, _) => format!("服务器回 {code}"),
        ureq::Error::Transport(transport) => transport.to_string(),
    };
    format!("{doing}失败：{detail}")
}

/// `path` 那一段 URL 编码：只动该动的字符。
///
/// 斜杠要留着（那是路径分隔），中文与空格必须编码。
fn encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 反过来的活：把 `%E4%B8%AD` 变回"中"。解不出来就原样留着。
fn decode(text: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(text.len());
    let mut chars = text.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let pair = chars
                .next()
                .and_then(|high| chars.next().map(|low| [high, low]));
            if let Some(pair) = pair
                && let (Some(high), Some(low)) = (hex_of(pair[0]), hex_of(pair[1]))
            {
                bytes.push(high * 16 + low);
                continue;
            }
            // 解不出来：把这个 % 原样放回去，总比丢字符好。
            bytes.push(byte);
            if let Some(pair) = pair {
                bytes.extend_from_slice(&pair);
            }
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex_of(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// HTTP Basic 的 `user:password` 编码。
///
/// 手写而不再拖一个 base64 依赖：就 20 行，而且我们只编码不解码。
fn basic(user: &str, password: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{user}:{password}");
    let mut out = String::new();
    for chunk in raw.as_bytes().chunks(3) {
        let (a, b, c) = (
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        );
        let group = (u32::from(a) << 16) | (u32::from(b) << 8) | u32::from(c);
        out.push(TABLE[(group >> 18) as usize & 63] as char);
        out.push(TABLE[(group >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(group >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[group as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 保证结尾有一个 `/`：后面拼路径时不用再想这件事。
fn with_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

/// 取 URL 里的路径部分（`https://host/dav/` → `/dav/`）。
///
/// 拿它跟服务器回的 href 对前缀——服务器常常只回路径不带主机。
fn path_of(url: &str) -> String {
    let rest = url.find("://").map(|at| &url[at + 3..]).unwrap_or(url);
    let path = rest.find('/').map(|at| &rest[at..]).unwrap_or("/");
    with_slash(path)
}

/// 解析 PROPFIND 的 207 响应。
///
/// 只用正经 XML 解析器：手写正则式扫描会在命名空间前缀（`d:`、`D:`、没有前缀）
/// 上翻车，而各家服务器恰恰在这些地方花样最多。
fn parse_propfind(body: &[u8]) -> Result<Vec<Entry>, String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);

    let mut out: Vec<Entry> = Vec::new();
    // 当前这一项收集到的字段。
    let mut href = String::new();
    let mut is_dir = false;
    let mut size = 0u64;
    let mut modified = None;
    // 现在读到的字符属于哪个标签（只看我们关心的那几个）。
    let mut tag = Tag::Other;
    let mut in_response = false;

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(element)) => {
                tag = Tag::of(element.name());
                if tag == Tag::Response {
                    in_response = true;
                    href.clear();
                    is_dir = false;
                    size = 0;
                    modified = None;
                }
                // `<d:collection/>` 只能出现在 `<d:resourcetype>` 里，
                // 见到它就是目录。
                if in_response && tag == Tag::Collection {
                    is_dir = true;
                }
            }
            Ok(Event::Empty(element)) => {
                if in_response && Tag::of(element.name()) == Tag::Collection {
                    is_dir = true;
                }
            }
            Ok(Event::Text(text)) => {
                if !in_response {
                    continue;
                }
                let value = text.decode().unwrap_or_default().to_string();
                match tag {
                    Tag::Href => href = value,
                    Tag::Length => size = value.parse().unwrap_or(0),
                    Tag::Modified => modified = parse_http_date(&value),
                    _ => {}
                }
            }
            Ok(Event::End(element)) => {
                let ended = Tag::of(element.name());
                if ended == Tag::Response {
                    if let Some(name) = name_of(&href) {
                        out.push(Entry {
                            name,
                            // 原始的 href 先留着：怎么折成相对路径得看连接根，
                            // 那是 `Dav` 才知道的事。
                            path: href.clone(),
                            is_dir,
                            size,
                            modified,
                        });
                    }
                    in_response = false;
                }
                tag = Tag::Other;
            }
            Err(error) => return Err(format!("XML 读不懂：{error}")),
            _ => {}
        }
    }
    if out.is_empty() {
        return Err("服务器回的东西里没有文件".to_string());
    }
    Ok(out)
}

/// 我们关心的那几个标签，其余的都归到 `Other`。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Response,
    Href,
    Length,
    Modified,
    Collection,
    Other,
}

impl Tag {
    /// 按**本地名**认标签：`d:href`、`D:href`、`href` 都是同一个东西。
    /// 前缀怎么写取决于服务器心情，命名空间前缀不该决定我们的行为。
    fn of(name: quick_xml::name::QName<'_>) -> Self {
        let local = name.local_name();
        match local.as_ref() {
            b"response" => Self::Response,
            b"href" => Self::Href,
            b"getcontentlength" => Self::Length,
            b"getlastmodified" => Self::Modified,
            b"collection" => Self::Collection,
            _ => Self::Other,
        }
    }
}

/// 从 href 里取最后一段当显示名；目录的 href 带尾巴斜杠，要跳过。
fn name_of(href: &str) -> Option<String> {
    let trimmed = href.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let name = decode(name);
    (!name.is_empty()).then_some(name)
}

/// `Wed, 21 Oct 2015 07:28:00 GMT` → Unix 秒。
///
/// 手写是为了不拖一个时间解析库：HTTP 日期只有这一种写法（RFC 7231），
/// 而且我们只要秒——解析不出来就当没有时间，那一列空着也不影响浏览。
fn parse_http_date(text: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // "Wed, 21 Oct 2015 07:28:00 GMT"
    let mut parts = text.split_whitespace();
    let _weekday = parts.next()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = MONTHS.iter().position(|name| *name == month_name)? as u32 + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let clock = parts.next()?;
    let mut clock = clock.split(':');
    let hour: u32 = clock.next()?.parse().ok()?;
    let minute: u32 = clock.next()?.parse().ok()?;
    let second: u32 = clock.next()?.parse().ok()?;

    days_from_civil(year, month, day).map(|days| {
        days * 86_400 + u64::from(hour) * 3600 + u64::from(minute) * 60 + u64::from(second)
    })
}

/// 公历年月日 → 距 1970-01-01 的天数。
///
/// 老老实实累加年与月，不用那套位运算技巧：一个文件的时间只算一次，
/// 一年 365 天的循环慢不到哪儿去，而看不懂的整数技巧错了最难查。
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<u64> {
    if !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let mut days: i64 = 0;
    let mut cursor = 1970i64;
    while cursor < year {
        days += if leap(cursor) { 366 } else { 365 };
        cursor += 1;
    }
    while cursor > year {
        cursor -= 1;
        days -= if leap(cursor) { 366 } else { 365 };
    }
    let lengths = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for month in 1..month {
        days += i64::from(lengths[month as usize - 1]);
    }
    days += i64::from(day) - 1;
    u64::try_from(days).ok()
}

fn leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    // ---- 纯字符串活 ----

    #[test]
    fn base64_matches_the_known_answer() {
        // "Aladdin:open sesame" 的 Basic 编码是 RFC 2617 里的例子。
        // 三个例子都是现算的标准答案：RFC 2617 的例子 + 短到一组的两种情况。
        assert_eq!(
            basic("Aladdin", "open sesame"),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        assert_eq!(basic("a", "b"), "YTpi");
        assert_eq!(basic("ab", "c"), "YWI6Yw==");
    }

    #[test]
    fn urls_get_a_trailing_slash() {
        assert_eq!(with_slash("https://host/dav"), "https://host/dav/");
        assert_eq!(with_slash("https://host/dav/"), "https://host/dav/");
    }

    #[test]
    fn the_path_part_is_pulled_out_of_the_url() {
        assert_eq!(path_of("https://host/dav/"), "/dav/");
        assert_eq!(path_of("https://host/"), "/");
        assert_eq!(path_of("https://host"), "/");
    }

    #[test]
    fn encoding_keeps_slashes_and_hides_the_rest() {
        assert_eq!(encode("a/b.txt"), "a/b.txt");
        assert_eq!(
            encode("我的 文件.txt"),
            "%E6%88%91%E7%9A%84%20%E6%96%87%E4%BB%B6.txt"
        );
    }

    #[test]
    fn decoding_round_trips() {
        assert_eq!(decode("%E6%88%91.txt"), "我.txt");
        assert_eq!(decode("plain.txt"), "plain.txt");
        assert_eq!(decode("%zz"), "%zz", "解不出来的原样留着");
    }

    #[test]
    fn http_dates_become_unix_seconds() {
        assert_eq!(
            parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
        assert_eq!(parse_http_date(" nonsense"), None);
    }

    // ---- PROPFIND 解析 ----

    const MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/folder/</d:href>
    <d:propstat><d:prop>
      <d:displayname>folder</d:displayname>
      <d:resourcetype><d:collection/></d:resourcetype>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/folder/notes.txt</d:href>
    <d:propstat><d:prop>
      <d:displayname>notes.txt</d:displayname>
      <d:getcontentlength>1234</d:getcontentlength>
      <d:getlastmodified>Wed, 21 Oct 2015 07:28:00 GMT</d:getlastmodified>
      <d:resourcetype/>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/folder/sub/</d:href>
    <d:propstat><d:prop>
      <d:resourcetype><d:collection/></d:resourcetype>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn a_multistatus_becomes_entries() {
        let entries = parse_propfind(MULTISTATUS.as_bytes()).expect("该解析出来");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "folder");
        assert!(entries[0].is_dir, "有 <d:collection/> 的就是目录");
        assert_eq!(entries[1].name, "notes.txt");
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].size, 1234);
        assert_eq!(entries[1].modified, Some(1_445_412_480));
        assert_eq!(entries[2].name, "sub");
        assert!(entries[2].is_dir);
    }

    #[test]
    fn a_response_without_a_prefix_parses_the_same() {
        // 有的服务器不写命名空间前缀：认本地名，不该因此解析失败。
        let bare = MULTISTATUS.replace("d:", "");
        let entries = parse_propfind(bare.as_bytes()).expect("该解析出来");
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is_dir);
    }

    #[test]
    fn junk_is_reported_not_swallowed() {
        assert!(parse_propfind(b"not xml at all").is_err());
    }

    #[test]
    fn a_folder_href_without_a_trailing_slash_still_has_a_name() {
        assert_eq!(name_of("/dav/folder/"), Some("folder".to_string()));
        assert_eq!(name_of("/dav/a.txt"), Some("a.txt".to_string()));
        // "/dav/" 是连接根的 href：它的名字是 dav（目录自己是靠路径对比滤掉的，
        // 不是靠这里）；真正的 "/" 才没有名字。
        assert_eq!(name_of("/dav/"), Some("dav".to_string()));
        assert_eq!(name_of("/"), None);
    }

    // ---- 真连一次（本地起一个假服务器）----

    /// 一个只认几个方法的假 WebDAV 服务器：真握手、真 HTTP、真 XML。
    ///
    /// 用真的 TCP 而不是 mock，是因为 WebDAV 最容易错的地方恰恰在
    /// "URL 拼成什么样、href 回来是什么样"——这些只有真发一次才试得出来。
    struct Fake {
        /// 客户端连的就是这个地址。
        url: String,
        /// 记下最后收到的请求，断言用。
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    fn fake_server() -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").expect("该能监听");
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    break;
                };
                let recorded = recorded.clone();
                let _ = thread::spawn(move || handle(stream, recorded)).join();
            }
        });
        Fake {
            url: format!("http://127.0.0.1:{port}/dav/"),
            seen,
        }
    }

    fn handle(mut stream: TcpStream, recorded: std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::BufRead as _;
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let mut length = 0usize;
        loop {
            line.clear();
            if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; length];
        let _ = reader.read_exact(&mut body);
        recorded.lock().unwrap().push(format!("{method} {target}"));

        let (status, content_type, payload): (&str, &str, Vec<u8>) = match method.as_str() {
            "PROPFIND" => (
                "207 Multi-Status",
                "application/xml",
                MULTISTATUS.as_bytes().to_vec(),
            ),
            "GET" => ("200 OK", "text/plain", b"hello dav".to_vec()),
            _ => ("201 Created", "text/plain", Vec::new()),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&payload);
        let _ = stream.flush();
    }

    fn config(url: &str) -> Config {
        Config::new(
            "t1".to_string(),
            "测试".to_string(),
            Kind::WebDav,
            url.to_string(),
            "me".to_string(),
        )
    }

    #[test]
    fn listing_a_real_folder_goes_over_http() {
        let fake = fake_server();
        let dav = Dav::new(&config(&fake.url), "secret").unwrap();

        let entries = dav.list("folder").expect("该列得出来");
        // 三项目录里去掉"folder"自己，剩 notes.txt 与 sub。
        assert_eq!(entries.len(), 2, "目录自己不该出现在列表里：{entries:?}");
        assert_eq!(entries[0].name, "notes.txt");
        assert!(entries[1].is_dir);

        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(seen, vec!["PROPFIND /dav/folder".to_string()]);
    }

    #[test]
    fn connecting_checks_that_the_root_is_reachable() {
        let fake = fake_server();
        // 用 "" 列根：连不上会在这一步就报错，而不是等到用户点进去才发现。
        assert!(Dav::connect(&config(&fake.url), "secret").is_ok());
    }

    #[test]
    fn a_dead_server_says_so_instead_of_hanging() {
        // 端口 1 上什么都没有：该立刻报"连不上"，不是卡住。
        let dav = Dav::new(&config("http://127.0.0.1:1/dav/"), "secret").unwrap();
        let error = dav.list("").expect_err("该报错");
        assert!(error.contains("列目录失败"), "报错要带上在干什么：{error}");
    }

    #[test]
    fn downloading_writes_the_bytes_out() {
        let fake = fake_server();
        let dav = Dav::new(&config(&fake.url), "secret").unwrap();

        let mut sink = Vec::new();
        let written = dav
            .download("folder/notes.txt", &mut sink)
            .expect("该下得来");
        assert_eq!(written, 9);
        assert_eq!(String::from_utf8(sink).unwrap(), "hello dav");
    }

    #[test]
    fn writing_methods_reach_the_server() {
        let fake = fake_server();
        let dav = Dav::new(&config(&fake.url), "secret").unwrap();

        dav.upload("folder/new.txt", b"hi").unwrap();
        dav.mkdir("folder/sub2").unwrap();
        dav.remove("folder/old.txt").unwrap();
        dav.rename("folder/a.txt", "folder/b.txt").unwrap();

        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                "PUT /dav/folder/new.txt".to_string(),
                "MKCOL /dav/folder/sub2".to_string(),
                "DELETE /dav/folder/old.txt".to_string(),
                "MOVE /dav/folder/a.txt".to_string(),
            ]
        );
    }

    #[test]
    fn the_secret_key_follows_the_id_not_the_name() {
        let mut config = config("https://host/dav/");
        assert_eq!(config.secret_key(), "remote:t1");
        // 改名字不该把密码弄丢。
        config.name = "改名了".to_string();
        assert_eq!(config.secret_key(), "remote:t1");
    }
}

// ---------------------------------------------------------------------------
// 远程路径
// ---------------------------------------------------------------------------

/// 远程路径的协议头：`dav://<连接 id>/<相对路径>`。
pub const SCHEME: &str = "dav";

/// 拼一个远程路径。
///
/// **用 `PathBuf` 装它是有意的**：导航历史、双栏同步、标签恢复、命令面板全都按
/// "一个路径"在工作，让它也走同一条路，这些地方一行都不用改。
pub fn location(id: &str, sub: &str) -> PathBuf {
    PathBuf::from(format!("{SCHEME}://{id}/{}", sub.trim_matches('/')))
}

/// 拆一个远程路径。不是远程路径就是 `None`（本地路径照旧走本地那一套）。
pub fn parse_location(path: &Path) -> Option<(String, String)> {
    let text = path.to_string_lossy();
    let rest = text.strip_prefix(&format!("{SCHEME}://"))?;
    let (id, sub) = match rest.find('/') {
        Some(at) => (&rest[..at], rest[at + 1..].trim_matches('/').to_string()),
        None => (rest, String::new()),
    };
    if id.is_empty() {
        return None;
    }
    Some((id.to_string(), sub))
}

#[cfg(test)]
mod location_tests {
    use super::*;

    #[test]
    fn locations_round_trip() {
        let path = location("jg", "照片/2024");
        assert_eq!(
            parse_location(&path),
            Some(("jg".to_string(), "照片/2024".to_string()))
        );
        assert_eq!(
            parse_location(&location("jg", "")),
            Some(("jg".to_string(), String::new()))
        );
        assert_eq!(
            parse_location(&location("jg", "x")),
            Some(("jg".to_string(), "x".to_string()))
        );
    }

    #[test]
    fn local_paths_are_not_remote() {
        assert_eq!(parse_location(Path::new("/Users/me/notes.txt")), None);
        assert_eq!(parse_location(Path::new("dav")), None);
    }
}
