//! FTP（P4）：明文 FTP，不带 TLS（FTPS 在路线图里明确不做）。
//!
//! 三个跟别处不一样的地方：
//! 1. **控制连接是有状态的**：`CWD` 改了当前目录，后面所有相对路径都跟着变。
//!    所以这里一律用绝对路径发命令，并且把这条连接锁在 `Mutex` 里——
//!    `Remote` 的每个方法只拿到 `&self`，而 suppaftp 要 `&mut`。
//! 2. **没有"复制"这条指令**：FTP 的指令集里压根没有服务器端复制，
//!    只能在本地临时文件里绕一圈。这是慢，但用户点了复制就该复制；
//!    报一句"不支持"反而更像坏了。
//! 3. 列目录靠 `LIST` 的一行行文本：格式各家不同（POSIX/DOS），
//!    解析交给 suppaftp 的解析器，它两种都认。

use std::io::{Read, Write};
use std::sync::Mutex;

use suppaftp::FtpStream;
use suppaftp::list::File as ListedFile;

use crate::remote::{Config, Entry, Remote};

/// 一个 FTP 连接。
pub struct Ftp {
    id: String,
    /// 登录后所在的目录：所有相对路径都挂在它下面。
    base: String,
    stream: Mutex<FtpStream>,
}

impl Ftp {
    pub fn new(config: &Config, password: &str) -> Result<Self, crate::i18n::Msg> {
        let address = address_of(&config.url)?;
        let mut stream = FtpStream::connect(&address).map_err(|error| {
            crate::i18n::Message::caused_with(
                "连不上 {address}：{error}",
                &[("address", address.to_string())],
                error.to_string(),
            )
        })?;
        stream
            .login(config.user.as_str(), password)
            .map_err(|error| {
                crate::i18n::Message::caused("登录失败：{error}", error.to_string())
            })?;
        // 二进制模式：ASCII 模式会顺手把换行改掉，传压缩包就废了。
        stream
            .transfer_type(suppaftp::types::FileType::Binary)
            .map_err(|error| {
                crate::i18n::Message::caused("切二进制模式失败：{error}", error.to_string())
            })?;
        let base = stream.pwd().map_err(|error| {
            crate::i18n::Message::caused("问不到当前目录：{error}", error.to_string())
        })?;
        // 地址里写了子目录（`ftp://host/pub/`）或配置里填了起始目录，
        // 就先 `CWD` 进去：用户填的那个目录才是他想的根。
        let start = if config.root.trim_matches('/').is_empty() {
            start_of(&config.url)
        } else {
            config.root.trim_matches('/').to_string()
        };
        let base = if start.is_empty() {
            base
        } else {
            let target = absolute(&base, &start);
            stream.cwd(&target).map_err(|error| {
                crate::i18n::Message::caused_with(
                    "进不去起始目录 {target}：{error}",
                    &[("target", target.to_string())],
                    error.to_string(),
                )
            })?;
            stream.pwd().map_err(|error| {
                crate::i18n::Message::caused("问不到当前目录：{error}", error.to_string())
            })?
        };
        Ok(Self {
            id: config.id.clone(),
            base,
            stream: Mutex::new(stream),
        })
    }

    /// 相对连接根的路径 → 服务器上的绝对路径。
    fn absolute(&self, path: &str) -> String {
        absolute(&self.base, path)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, FtpStream>, crate::i18n::Msg> {
        self.stream
            .lock()
            .map_err(|_| crate::i18n::Message::msg("这条连接正在忙"))
    }

    /// 这一项是不是目录：只有真的 `CWD` 进去才知道，
    /// `LIST` 一个文件也会回一行，分不出来。
    fn is_dir(&self, absolute: &str) -> bool {
        let Ok(mut stream) = self.stream.lock() else {
            return false;
        };
        let entered = stream.cwd(absolute).is_ok();
        if entered {
            // 一定要切回来：控制连接是有状态的，留在别处会连累后面的命令。
            let _ = stream.cwd(&self.base);
        }
        entered
    }

    /// 复制没有 FTP 指令可用：先下到临时文件，再传上去。
    fn copy_through_a_temp_file(&self, from: &str, to: &str) -> Result<(), crate::i18n::Msg> {
        let mut temporary = std::env::temp_dir();
        temporary.push(format!("wfiles-ftp-{}-{}.tmp", self.id, std::process::id()));
        let result = (|| -> Result<(), crate::i18n::Msg> {
            {
                let mut file = std::fs::File::create(&temporary).map_err(|error| {
                    crate::i18n::Message::caused("建不了临时文件：{error}", error)
                })?;
                self.download(from, &mut file)?;
            }
            let mut file = std::fs::File::open(&temporary)
                .map_err(|error| crate::i18n::Message::caused("读不了临时文件：{error}", error))?;
            let length = file
                .metadata()
                .map_err(|error| crate::i18n::Message::caused("看不了临时文件：{error}", error))?
                .len();
            self.upload(to, &mut file, length)
        })();
        // 临时文件用完就删：留着只会占地方。
        std::fs::remove_file(&temporary).ok();
        result
    }
}

impl Remote for Ftp {
    fn id(&self) -> &str {
        &self.id
    }

    /// 两个方向都能续：`REST`+`RETR` 读半截，`APPE` 写半截，
    /// 都是 FTP 里活了三十多年的指令。
    fn resumes_download(&self) -> bool {
        true
    }

    fn resumes_upload(&self) -> bool {
        true
    }

    fn list(&self, path: &str) -> Result<Vec<Entry>, crate::i18n::Msg> {
        let absolute = self.absolute(path);
        let lines = self.lock()?.list(Some(&absolute)).map_err(|error| {
            crate::i18n::Message::caused("列目录失败：{error}", error.to_string())
        })?;
        let mut out = Vec::new();
        for line in lines {
            let Ok(file) = ListedFile::try_from(line.as_str()) else {
                continue;
            };
            let name = file.name().to_string();
            if name == "." || name == ".." {
                continue;
            }
            out.push(Entry {
                path: join(path, &name),
                name,
                is_dir: file.is_directory(),
                size: file.size() as u64,
                modified: seconds_of(file.modified()),
            });
        }
        Ok(out)
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>, crate::i18n::Msg> {
        if path.is_empty() {
            return Ok(Some(Entry {
                name: String::new(),
                path: String::new(),
                is_dir: true,
                size: 0,
                modified: None,
            }));
        }
        let absolute = self.absolute(path);
        let mut stream = self.lock()?;
        // 先试 CWD：进得去就是目录。
        if stream.cwd(&absolute).is_ok() {
            let _ = stream.cwd(&self.base);
            drop(stream);
            return Ok(Some(Entry {
                name: name_of(path),
                path: path.to_string(),
                is_dir: true,
                size: 0,
                modified: None,
            }));
        }
        let size = match stream.size(&absolute) {
            Ok(size) => size as u64,
            Err(_) => return Ok(None),
        };
        let modified = stream
            .mdtm(&absolute)
            .ok()
            .and_then(|at| at.and_utc().timestamp().try_into().ok());
        drop(stream);
        Ok(Some(Entry {
            name: name_of(path),
            path: path.to_string(),
            is_dir: false,
            size,
            modified,
        }))
    }

    fn download(&self, path: &str, sink: &mut dyn Write) -> Result<u64, crate::i18n::Msg> {
        let absolute = self.absolute(path);
        self.lock()?
            .retr(&absolute, |reader| pump(reader, sink))
            .map_err(|error| crate::i18n::Message::caused("下载失败：{error}", error.to_string()))
    }

    /// 从 `offset` 接着下载。
    ///
    /// FTP 的续传是老规矩：先 `REST <字节数>` 告诉服务器"跳过前面这些"，
    /// 再照常 `RETR`。`REST` 自己不会开始传输，也不该留着——
    /// 下一次不带 `REST` 的 `RETR` 用的是同一条控制连接，
    /// 服务器还记得上一个断点，所以用完要把偏移归零。
    fn download_from(
        &self,
        path: &str,
        offset: u64,
        sink: &mut dyn Write,
    ) -> Result<u64, crate::i18n::Msg> {
        let absolute = self.absolute(path);
        let mut session = self.lock()?;
        session.resume_transfer(offset as usize).map_err(|error| {
            crate::i18n::Message::caused("续传失败：{error}", error.to_string())
        })?;
        let result = session.retr(&absolute, |reader| pump(reader, sink));
        // 不管成没成，把断点还回去：下一次普通下载该从 0 开始。
        session.resume_transfer(0).ok();
        result.map_err(|error| crate::i18n::Message::caused("下载失败：{error}", error.to_string()))
    }

    fn upload(
        &self,
        path: &str,
        reader: &mut dyn Read,
        _length: u64,
    ) -> Result<(), crate::i18n::Msg> {
        let absolute = self.absolute(path);
        // `put_file` 要一个 Sized 的 `R: Read`，而这里我们只有 `&mut dyn Read`：
        // 套一层就行，它只是把 read 转发过去。
        let mut wrapped = ReadRef(reader);
        self.lock()?
            .put_file(&absolute, &mut wrapped)
            .map(|_| ())
            .map_err(|error| crate::i18n::Message::caused("上传失败：{error}", error.to_string()))
    }

    /// 从 `offset` 接着上传：`APPE` 的意思是"接着文件末尾写"，
    /// 而调用方已经确认过服务器上正好有 `offset` 个字节，两者对得上，
    /// 所以这里不用再问一遍长度。
    ///
    /// 为什么不用 `REST` + `STOR`：`STOR` 会把文件截断，`REST` 对它的解释
    /// 各服务器不一致（有的从断点写，有的从 0 写），而 `APPE` 的意思没有歧义。
    fn upload_from(
        &self,
        path: &str,
        reader: &mut dyn Read,
        _length: u64,
        _offset: u64,
    ) -> Result<(), crate::i18n::Msg> {
        let absolute = self.absolute(path);
        let mut wrapped = ReadRef(reader);
        self.lock()?
            .append_file(&absolute, &mut wrapped)
            .map(|_| ())
            .map_err(|error| crate::i18n::Message::caused("续传失败：{error}", error.to_string()))
    }

    fn mkdir(&self, path: &str) -> Result<(), crate::i18n::Msg> {
        let absolute = self.absolute(path);
        self.lock()?
            .mkdir(&absolute)
            .map_err(|error| crate::i18n::Message::caused("建目录失败：{error}", error.to_string()))
    }

    fn remove(&self, path: &str) -> Result<(), crate::i18n::Msg> {
        let absolute = self.absolute(path);
        // 目录得先把里面清空：FTP 的 RMD 只删空目录，
        // 而用户按删除时想的是"这个文件夹没了"。
        if self.is_dir(&absolute) {
            for child in self.list(path)? {
                self.remove(&child.path)?;
            }
            return self.lock()?.rmdir(&absolute).map_err(|error| {
                crate::i18n::Message::caused("删目录失败：{error}", error.to_string())
            });
        }
        self.lock()?
            .rm(&absolute)
            .map_err(|error| crate::i18n::Message::caused("删除失败：{error}", error.to_string()))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), crate::i18n::Msg> {
        let from = self.absolute(from);
        let to = self.absolute(to);
        self.lock()?
            .rename(&from, &to)
            .map_err(|error| crate::i18n::Message::caused("改名失败：{error}", error.to_string()))
    }

    fn copy(&self, from: &str, to: &str) -> Result<(), crate::i18n::Msg> {
        // FTP 的指令集里没有复制：只能下到临时文件再传上去。
        // 同一台服务器绕这一圈确实慢，但"复制"点下去就该真的复制一份。
        match self.stat(from)? {
            Some(info) if info.is_dir => {
                self.mkdir(to)?;
                for child in self.list(from)? {
                    let target = join(to, &child.name);
                    if child.is_dir {
                        self.copy(&child.path, &target)?;
                    } else {
                        self.copy_through_a_temp_file(&child.path, &target)?;
                    }
                }
                Ok(())
            }
            _ => self.copy_through_a_temp_file(from, to),
        }
    }
}

/// `retr` 的回调只能报 `FtpError`，而它里面没有装 IO 错误的那一支合适位置，
/// 借 `ConnectionError` 把 IO 错误带出去。
fn broken_pipe(error: std::io::Error) -> suppaftp::FtpError {
    suppaftp::FtpError::ConnectionError(error)
}

/// 把 `&mut dyn Read` 变成一个 Sized 的 `Read`。
struct ReadRef<'a>(&'a mut dyn Read);

impl Read for ReadRef<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer)
    }
}

/// 把数据连接上的字节搬进 `sink`，返回搬了多少。
///
/// 单独拎出来是因为下载和续传下载都要用同一套循环——
/// 两处各写一遍，改的时候准会漏掉一处。
/// 错误类型跟着 `suppaftp` 的闭包要求走（它要 `FtpError`）。
fn pump(reader: &mut dyn Read, sink: &mut dyn Write) -> Result<u64, suppaftp::FtpError> {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer).map_err(broken_pipe)?;
        if read == 0 {
            return Ok(total);
        }
        sink.write_all(&buffer[..read]).map_err(broken_pipe)?;
        total += read as u64;
    }
}

/// 相对路径挂到登录目录下面：FTP 一律用绝对路径发命令，
/// 因为控制连接上 `CWD` 改过的当前目录会连累后面每一条。
fn absolute(base: &str, path: &str) -> String {
    let path = path.trim_matches('/');
    if path.is_empty() {
        base.to_string()
    } else if base.ends_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

/// `ftp://host:21/起始目录` → `host:21`。没写端口就按 21。
fn address_of(url: &str) -> Result<String, crate::i18n::Msg> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err(crate::i18n::Message::msg("地址里没有主机名"));
    }
    if host.contains(':') {
        Ok(host.to_string())
    } else {
        Ok(format!("{host}:21"))
    }
}

/// 地址里带的起始目录：`ftp://host/pub/` → `pub`。没写就是空。
fn start_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    match rest.find('/') {
        Some(at) => rest[at..].trim_matches('/').to_string(),
        None => String::new(),
    }
}

fn join(parent: &str, name: &str) -> String {
    let parent = parent.trim_matches('/');
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn name_of(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

/// `SystemTime` → Unix 秒。服务器没给时间（或给的是 1970 年之前）就没有。
fn seconds_of(at: std::time::SystemTime) -> Option<u64> {
    at.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|it| it.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_gets_a_default_port() {
        assert_eq!(address_of("ftp://127.0.0.1/").unwrap(), "127.0.0.1:21");
        assert_eq!(
            address_of("ftp://127.0.0.1:2121/").unwrap(),
            "127.0.0.1:2121"
        );
        assert_eq!(address_of("ftp://host/pub/").unwrap(), "host:21");
        assert!(address_of("ftp:///").is_err(), "没有主机名该报错");
    }

    #[test]
    fn paths_are_joined_with_one_slash() {
        assert_eq!(join("", "a.txt"), "a.txt");
        assert_eq!(join("folder", "a.txt"), "folder/a.txt");
        assert_eq!(join("/folder/", "a.txt"), "folder/a.txt");
        assert_eq!(name_of("folder/sub"), "sub");
        assert_eq!(name_of("folder/sub/"), "sub");
        assert_eq!(start_of("ftp://host/"), "");
        assert_eq!(start_of("ftp://host:2121/pub/"), "pub");
        assert_eq!(start_of("ftp://host/pub/incoming/"), "pub/incoming");
    }

    #[test]
    fn everything_hangs_off_the_login_directory() {
        assert_eq!(absolute("/home/me", ""), "/home/me");
        assert_eq!(absolute("/home/me", "a.txt"), "/home/me/a.txt");
        assert_eq!(absolute("/home/me/", "/folder/sub/"), "/home/me/folder/sub");
        assert_eq!(absolute("/", "a.txt"), "/a.txt");
    }

    // ---- 真连一次（本地起一个假的 FTP 服务器）----

    /// 一个够用的假 FTP 服务器：真的控制连接、真的被动模式数据连接。
    ///
    /// FTP 最容易错的地方是"命令顺序与被动模式那一来一回"，
    /// 而这些只有真连一次才试得出来。
    struct Fake {
        /// `host:port`，客户端连的就是它。
        address: String,
        /// 控制连接上收到的命令（去掉参数里的路径，只留命令名）。
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        /// STOR 上传上来的内容，按路径存。
        stored: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    /// 服务器上的"文件系统"：路径 → （是不是目录, 内容）。
    type Tree = std::collections::HashMap<String, (bool, Vec<u8>)>;

    fn seed() -> Tree {
        let mut tree = Tree::new();
        tree.insert("/home/me".to_string(), (true, Vec::new()));
        tree.insert(
            "/home/me/notes.txt".to_string(),
            (false, b"hello ftp".to_vec()),
        );
        tree.insert("/home/me/folder".to_string(), (true, Vec::new()));
        tree.insert("/home/me/folder/b.txt".to_string(), (false, b"bb".to_vec()));
        tree.insert("/home/me/pub".to_string(), (true, Vec::new()));
        tree
    }

    /// `LIST` 的一行：POSIX 格式，跟真服务器一个样。
    fn list_line(size: usize, is_dir: bool, name: &str) -> String {
        let perms = if is_dir { "drwxr-xr-x" } else { "-rw-r--r--" };
        format!("{perms}   1 0  0    {size:>8} Nov 21 07:28 {name}")
    }

    /// 真服务器就这么回：`LIST 一个文件` 回那一行；
    /// `LIST 一个目录` 回它**里面**的东西，目录自己不在列表里。
    /// 把目录自己也列出来的话，`remove` 会去删一个不存在的同名子项。
    fn listing_for(tree: &Tree, path: &str) -> String {
        let mut out = String::new();
        let Some((is_dir, content)) = tree.get(path) else {
            return out;
        };
        if !is_dir {
            let name = path.rsplit('/').next().unwrap_or(path);
            out.push_str(&list_line(content.len(), false, name));
            out.push_str("\r\n");
            return out;
        }
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let mut children: Vec<&String> = tree
            .keys()
            .filter(|key| key.starts_with(&prefix) && !key[prefix.len()..].contains('/'))
            .collect();
        children.sort();
        for child in children {
            let (is_dir, content) = &tree[child];
            let name = child.rsplit('/').next().unwrap_or(child);
            out.push_str(&list_line(content.len(), *is_dir, name));
            out.push_str("\r\n");
        }
        out
    }

    fn fake_ftp() -> Fake {
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("该能监听");
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stored = Arc::new(Mutex::new(Vec::new()));
        let tree: Arc<Mutex<Tree>> = Arc::new(Mutex::new(seed()));

        let seen_for_thread = seen.clone();
        let stored_for_thread = stored.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let seen = seen_for_thread.clone();
                let stored = stored_for_thread.clone();
                let tree = tree.clone();
                let _ = thread::spawn(move || {
                    serve(stream, seen, stored, tree);
                });
            }
        });
        Fake {
            address: format!("127.0.0.1:{port}"),
            seen,
            stored,
        }
    }

    fn say(control: &mut std::net::TcpStream, text: &str) -> Result<(), ()> {
        use std::io::Write as _;
        control
            .write_all(format!("{text}\r\n").as_bytes())
            .map_err(|_| ())
    }

    /// 一条控制连接上要记住的三样东西——都是"上一条命令影响下一条"的那类状态。
    struct Session {
        /// `PASV` 开出来的被动端口，一次性的。
        passive: Option<std::net::TcpListener>,
        /// `RNFR` 记下的待改名路径，等 `RNTO`。
        renaming: Option<String>,
        /// 当前目录。控制连接是有状态的：`CWD` 改过的当前目录要跟着变，
        /// 不然测不出"所有路径都挂在登录目录下面"这件事。
        at: String,
    }

    /// 跑完一条控制连接：先打招呼，然后一问一答到对方挂断。
    fn serve(
        mut control: std::net::TcpStream,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        stored: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
        tree: std::sync::Arc<std::sync::Mutex<Tree>>,
    ) {
        use std::io::Write as _;
        let _ = control.write_all(b"220 ready\r\n");
        let mut reader = std::io::BufReader::new(control.try_clone().expect("该能复制句柄"));
        let mut state = Session {
            passive: None,
            renaming: None,
            at: "/home/me".to_string(),
        };
        let _ = session(&mut control, &mut reader, seen, stored, tree, &mut state);
    }

    fn session(
        control: &mut std::net::TcpStream,
        reader: &mut std::io::BufReader<std::net::TcpStream>,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        stored: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
        tree: std::sync::Arc<std::sync::Mutex<Tree>>,
        state: &mut Session,
    ) -> Result<(), ()> {
        use std::io::BufRead as _;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                return Err(());
            }
            let command = line.trim_end().to_string();
            let mut parts = command.splitn(2, ' ');
            let verb = parts.next().unwrap_or_default().to_uppercase();
            let argument = parts.next().unwrap_or_default().trim_matches('"');
            seen.lock().unwrap().push(verb.clone());
            match verb.as_str() {
                "USER" => say(control, "331 需要密码")?,
                "PASS" => say(control, "230 登录成功")?,
                "TYPE" => say(control, "200 好")?,
                "LIST" => {
                    let body = listing_for(&tree.lock().unwrap(), argument);
                    // 数据连接的规矩：先在控制连接上回一句"我开了"（150），
                    // 再走数据连接，最后回"传完了"（226）。
                    // 直接回 226 的话客户端根本不会去连那个被动端口。
                    say(control, "150 打开数据连接")?;
                    let reply = if send_on_data(&mut state.passive, body.as_bytes()) {
                        "226 传完了"
                    } else {
                        "425 数据连接开不了"
                    };
                    say(control, reply)?;
                }
                "RETR" => {
                    let content = tree
                        .lock()
                        .unwrap()
                        .get(argument)
                        .filter(|(is_dir, _)| !*is_dir)
                        .map(|(_, content)| content.clone());
                    let Some(content) = content else {
                        say(control, "550 没有这个文件")?;
                        continue;
                    };
                    say(control, "150 打开数据连接")?;
                    send_on_data(&mut state.passive, &content);
                    say(control, "226 传完了")?;
                }
                "STOR" => {
                    say(control, "150 打开数据连接")?;
                    let received = receive_on_data(&mut state.passive);
                    tree.lock()
                        .unwrap()
                        .insert(argument.to_string(), (false, received.clone()));
                    stored.lock().unwrap().push((
                        argument.to_string(),
                        String::from_utf8_lossy(&received).to_string(),
                    ));
                    say(control, "226 传完了")?;
                }
                "QUIT" => {
                    let _ = control.write_all(b"221 bye\r\n");
                    return Err(());
                }
                "RNFR" => {
                    state.renaming = Some(argument.to_string());
                    say(control, "350 继续")?;
                }
                "RNTO" => {
                    if let Some(from) = state.renaming.take() {
                        let entry = tree.lock().unwrap().remove(&from);
                        if let Some(entry) = entry {
                            tree.lock().unwrap().insert(argument.to_string(), entry);
                        }
                    }
                    say(control, "250 改好了")?;
                }
                "PASV" => {
                    let data = std::net::TcpListener::bind("127.0.0.1:0").map_err(|_| ())?;
                    let port = data.local_addr().map_err(|_| ())?.port();
                    state.passive = Some(data);
                    say(
                        control,
                        &format!("227 被动模式 (127,0,0,1,{},{})", port >> 8, port & 255),
                    )?;
                }
                "EPSV" => {
                    let data = std::net::TcpListener::bind("127.0.0.1:0").map_err(|_| ())?;
                    let port = data.local_addr().map_err(|_| ())?.port();
                    state.passive = Some(data);
                    say(control, &format!("229 扩展被动模式 (|||{port}|)"))?;
                }
                "PWD" => say(control, &format!("257 \"{}\"", state.at))?,
                "CWD" => {
                    let ok = tree
                        .lock()
                        .unwrap()
                        .get(argument)
                        .is_some_and(|(is_dir, _)| *is_dir);
                    if ok {
                        // 有状态的连接：`CWD` 之后 `PWD` 得报新地方。
                        state.at = argument.to_string();
                    }
                    say(
                        control,
                        if ok {
                            "250 好"
                        } else {
                            "550 没有这个目录"
                        },
                    )?;
                }
                "SIZE" => {
                    let size = tree
                        .lock()
                        .unwrap()
                        .get(argument)
                        .filter(|(is_dir, _)| !*is_dir)
                        .map(|(_, content)| content.len());
                    match size {
                        Some(size) => say(control, &format!("213 {size}"))?,
                        None => say(control, "550 不是文件")?,
                    }
                }
                "MDTM" => say(control, "213 20151021072800")?,
                "MKD" => {
                    tree.lock()
                        .unwrap()
                        .insert(argument.to_string(), (true, Vec::new()));
                    say(control, "257 建好了")?;
                }
                "RMD" | "DELE" => {
                    let keys: Vec<String> = tree
                        .lock()
                        .unwrap()
                        .keys()
                        .filter(|key| *key == argument || key.starts_with(&format!("{argument}/")))
                        .cloned()
                        .collect();
                    for key in keys {
                        tree.lock().unwrap().remove(&key);
                    }
                    say(control, "250 删掉了")?;
                }
                _ => say(control, "200 好")?,
            }
        }
    }

    /// 被动端口只配用一次：用完就拿掉，免得下一条命令连到上一个端口上。
    fn take_data(passive: &mut Option<std::net::TcpListener>) -> Option<std::net::TcpStream> {
        let listener = passive.take()?;
        let (data, _) = listener.accept().ok()?;
        Some(data)
    }

    fn send_on_data(passive: &mut Option<std::net::TcpListener>, body: &[u8]) -> bool {
        use std::io::Write as _;
        let Some(mut data) = take_data(passive) else {
            return false;
        };
        data.write_all(body).is_ok()
    }

    fn receive_on_data(passive: &mut Option<std::net::TcpListener>) -> Vec<u8> {
        use std::io::Read as _;
        let Some(mut data) = take_data(passive) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let _ = data.read_to_end(&mut out);
        out
    }

    fn ftp_client(fake: &Fake) -> Ftp {
        config_at(fake, "")
    }

    fn config_at(fake: &Fake, start: &str) -> Ftp {
        let config = Config::new(
            "f1".to_string(),
            "测试".to_string(),
            crate::remote::Kind::Ftp,
            format!("ftp://{}/{start}", fake.address),
            "me".to_string(),
        );
        Ftp::new(&config, "secret").expect("该连得上")
    }

    #[test]
    fn logging_in_lands_on_the_home_directory() {
        let fake = fake_ftp();
        let ftp = ftp_client(&fake);
        // 所有相对路径都挂在登录目录下面：这是后面每一条命令的前提。
        assert_eq!(ftp.base, "/home/me");
        assert!(fake.seen.lock().unwrap().contains(&"USER".to_string()));
    }

    #[test]
    fn the_starting_directory_comes_from_the_address() {
        let fake = fake_ftp();
        // 地址里写了子目录就该进到那里面：用户填的目录才是他想的根，
        // 只取主机名的话 `ftp://host/pub/` 会莫名其妙落到登录目录。
        let ftp = config_at(&fake, "pub");
        assert_eq!(ftp.base, "/home/me/pub");
    }

    #[test]
    fn listing_reads_the_posix_lines() {
        let fake = fake_ftp();
        let ftp = ftp_client(&fake);

        let entries = ftp.list("").expect("该列得出来");
        let names: Vec<&str> = entries.iter().map(|it| it.name.as_str()).collect();
        assert!(names.contains(&"notes.txt"), "{names:?}");
        assert!(names.contains(&"folder"), "{names:?}");
        let folder = entries.iter().find(|it| it.name == "folder").unwrap();
        assert!(folder.is_dir, "d 开头的是目录：{:?}", folder);
        let notes = entries.iter().find(|it| it.name == "notes.txt").unwrap();
        assert!(!notes.is_dir);
        assert_eq!(notes.size, 9);
    }

    #[test]
    fn stat_tells_a_file_from_a_folder() {
        let fake = fake_ftp();
        let ftp = ftp_client(&fake);

        let file = ftp
            .stat("notes.txt")
            .expect("该问得出来")
            .expect("该文件在");
        assert!(!file.is_dir);
        assert_eq!(file.size, 9);
        assert_eq!(file.modified, Some(1_445_412_480));

        // 目录是靠真的 CWD 进去认出来的：LIST 一个文件也会回一行，分不出来。
        let folder = ftp.stat("folder").expect("该问得出来").expect("该目录在");
        assert!(folder.is_dir);

        assert_eq!(ftp.stat("不在的东西.txt").expect("该问得出来"), None);
    }

    #[test]
    fn downloading_writes_the_bytes_out() {
        let fake = fake_ftp();
        let ftp = ftp_client(&fake);

        let mut sink = Vec::new();
        let written = ftp.download("notes.txt", &mut sink).expect("该下得来");
        assert_eq!(written, 9);
        assert_eq!(String::from_utf8(sink).unwrap(), "hello ftp");
    }

    #[test]
    fn uploading_reaches_the_server() {
        let fake = fake_ftp();
        let ftp = ftp_client(&fake);

        let mut body = std::io::Cursor::new(b"hello up".to_vec());
        ftp.upload("folder/new.txt", &mut body, 8)
            .expect("该传得上去");

        let stored = fake.stored.lock().unwrap().clone();
        assert_eq!(
            stored,
            vec![(
                "/home/me/folder/new.txt".to_string(),
                "hello up".to_string()
            )]
        );
    }

    #[test]
    fn renaming_goes_through_rnfr_and_rnto() {
        let fake = fake_ftp();
        let ftp = ftp_client(&fake);

        ftp.rename("notes.txt", "renamed.txt").expect("该改得动");

        let seen = fake.seen.lock().unwrap().clone();
        assert!(
            seen.contains(&"RNFR".to_string()) && seen.contains(&"RNTO".to_string()),
            "{seen:?}"
        );
    }

    #[test]
    fn removing_a_folder_clears_it_first() {
        let fake = fake_ftp();
        let ftp = ftp_client(&fake);

        ftp.remove("folder").expect("该删得掉");

        // 目录底下还有东西：FTP 的 RMD 只删空目录，
        // 所以得先把里面一个个删掉——不然用户看到的"删了"其实还在。
        let seen = fake.seen.lock().unwrap().clone();
        let deletes = seen.iter().filter(|it| *it == "DELE").count();
        assert_eq!(deletes, 1, "folder/b.txt 该先被删掉：{seen:?}");
        assert!(seen.contains(&"RMD".to_string()), "{seen:?}");
    }

    #[test]
    fn copying_goes_through_a_local_temp_file() {
        let fake = fake_ftp();
        let ftp = ftp_client(&fake);

        // FTP 的指令集里没有复制：只能在本地临时文件里绕一圈。
        ftp.copy("notes.txt", "copy.txt").expect("该复制得动");

        let stored = fake.stored.lock().unwrap().clone();
        assert_eq!(
            stored,
            vec![("/home/me/copy.txt".to_string(), "hello ftp".to_string())]
        );
    }

    #[test]
    fn making_a_folder_uses_the_absolute_path() {
        let fake = fake_ftp();
        let ftp = ftp_client(&fake);
        ftp.mkdir("新建的").expect("该建得了");
        let seen = fake.seen.lock().unwrap().clone();
        assert!(seen.contains(&"MKD".to_string()), "{seen:?}");
    }
}
