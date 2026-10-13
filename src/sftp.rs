//! SFTP（P4）：SSH 上的文件传输。
//!
//! 三个跟别处不一样的地方：
//! 1. **服务器的公钥要认**：不认就等于把密码交给随便一台自称是它的机器。
//!    第一次连记进 known_hosts（先信一次），之后再连换了钥匙就明确拒绝——
//!    那要么是被冒充，要么是服务器重装过，两种都该让人看一眼而不是连上去。
//! 2. **russh 是异步的**：`Remote` 那套接口是同步的（调用方本来就在后台线程里），
//!    所以每条连接自带一个 tokio 运行时，外面照旧同步调用，
//!    异步不往上冒。
//! 3. **标准里没有服务器端的复制**：跟 FTP 一样绕本地临时文件。
//!    顺带一提，删目录得先把里面清空——SFTP 的 RMDIR 也只删空目录。
//!
//! 另外，判断"是不是目录"一律用 `symlink_metadata`：
//! 跟着符号链接走进去删，删掉的是别人的东西。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::Handle;
use russh::keys::known_hosts;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::remote::{Config, Entry, Remote};

/// 一个 SFTP 连接。
pub struct Sftp {
    id: String,
    /// 登录后所在的目录：所有相对路径都挂在它下面。
    base: String,
    /// russh 与 russh-sftp 都是异步的：这条连接自己带一个运行时
    /// （一个工作线程足够），外面调进来还是同步的。
    runtime: tokio::runtime::Runtime,
    session: SftpSession,
    /// 握着 russh 的连接句柄。最后一句柄一丢，
    /// 收信的那头就断了，SSH 会话跟着没——所以必须留在结构体里。
    #[allow(dead_code, reason = "握着就是它的用处，读它反而是多余的")]
    connection: Box<Handle<Trust>>,
}

impl Sftp {
    fn pull(&self, absolute: &str, offset: u64, sink: &mut dyn Write) -> Result<u64, String> {
        self.runtime.block_on(async {
            let mut file = self
                .session
                .open(absolute)
                .await
                .map_err(|error| format!("打不开 {absolute}：{error}"))?;
            if offset > 0 {
                use tokio::io::AsyncSeekExt as _;
                file.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(|error| format!("续传：定位不到 {offset}：{error}"))?;
            }
            let mut buffer = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                let read = file
                    .read(&mut buffer)
                    .await
                    .map_err(|error| format!("下载失败：{error}"))?;
                if read == 0 {
                    break;
                }
                sink.write_all(&buffer[..read])
                    .map_err(|error| format!("写不进本地文件：{error}"))?;
                total += read as u64;
            }
            file.close()
                .await
                .map_err(|error| format!("关不掉远程文件：{error}"))?;
            Ok(total)
        })
    }

    fn push(
        &self,
        absolute: &str,
        reader: &mut dyn Read,
        offset: Option<u64>,
    ) -> Result<(), String> {
        self.runtime.block_on(async {
            // 续传时不截断：`CREATE | WRITE` 会新建，也会留着旧内容。
            let flags = if offset.is_some() {
                OpenFlags::CREATE | OpenFlags::WRITE
            } else {
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE
            };
            let mut file = self
                .session
                .open_with_flags(absolute, flags)
                .await
                .map_err(|error| format!("打不开 {absolute}：{error}"))?;
            if let Some(offset) = offset {
                use tokio::io::AsyncSeekExt as _;
                file.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(|error| format!("续传：定位不到 {offset}：{error}"))?;
            }
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let read = reader
                    .read(&mut buffer)
                    .map_err(|error| format!("读不了本地文件：{error}"))?;
                if read == 0 {
                    break;
                }
                file.write_all(&buffer[..read])
                    .await
                    .map_err(|error| format!("上传失败：{error}"))?;
            }
            file.close()
                .await
                .map_err(|error| format!("关不掉远程文件：{error}"))?;
            Ok(())
        })
    }

    /// 建一个连接（会联网：握手、登录、开 SFTP 子系统）。
    pub fn new(config: &Config, password: &str) -> Result<Self, String> {
        Self::connect(config, password, known_hosts_file())
    }

    /// known_hosts 的位置可以指定：默认值是用户 `~/.ssh/known_hosts`，
    /// 跟 OpenSSH 是同一份、同一个格式，测试里则换到临时目录去。
    pub(crate) fn connect(config: &Config, password: &str, known: PathBuf) -> Result<Self, String> {
        let (host, port) = address_of(&config.url)?;
        let start = if config.root.trim_matches('/').is_empty() {
            start_of(&config.url)
        } else {
            config.root.trim_matches('/').to_string()
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| format!("起不了网络运行时：{error}"))?;

        let refused: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let trust = Trust {
            host: host.clone(),
            port,
            file: known,
            refused: refused.clone(),
        };
        // 远程可能很慢，但一直转圈比报错更难排查：断线 60 秒、15 秒探一次。
        let options = russh::client::Config {
            inactivity_timeout: Some(Duration::from_secs(60)),
            keepalive_interval: Some(Duration::from_secs(15)),
            ..Default::default()
        };

        let connected = runtime.block_on(async {
            let mut handle = russh::client::connect(Arc::new(options), (&host[..], port), trust)
                .await
                .map_err(|error| format!("连不上 {host}:{port}：{error}"))?;
            let accepted = if config.key.trim().is_empty() {
                handle
                    .authenticate_password(config.user.as_str(), password)
                    .await
            } else {
                let secret = russh::keys::load_secret_key(
                    &config.key,
                    (!password.is_empty()).then_some(password),
                )
                .map_err(|error| format!("读不了私钥 {}：{error}", config.key))?;
                let key = PrivateKeyWithHashAlg::new(Arc::new(secret), None);
                handle
                    .authenticate_publickey(config.user.as_str(), key)
                    .await
            }
            .map_err(|error| format!("登录失败：{error}"))?;
            if !accepted.success() {
                return Err(format!("{} 没通过：用户名或凭据不对", config.user));
            }
            let channel = handle
                .channel_open_session()
                .await
                .map_err(|error| format!("开不了会话通道：{error}"))?;
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(|error| format!("服务器不给开 SFTP：{error}"))?;
            let session = SftpSession::new(channel.into_stream())
                .await
                .map_err(|error| format!("SFTP 握手失败：{error}"))?;
            Ok::<_, String>((handle, session))
        });

        let (handle, session) = match connected {
            Ok(it) => it,
            Err(error) => {
                // known_hosts 那句"钥匙换了"比 russh 的报错有用得多。
                if let Some(reason) = refused.lock().ok().and_then(|mut it| it.take()) {
                    return Err(reason);
                }
                return Err(error);
            }
        };

        let base = runtime.block_on(async {
            let home = session
                .canonicalize(".")
                .await
                .map_err(|error| format!("问不到登录目录：{error}"))?;
            if start.is_empty() {
                return Ok::<_, String>(home);
            }
            session
                .canonicalize(absolute(&home, &start))
                .await
                .map_err(|error| format!("进不去起始目录 {start}：{error}"))
        })?;

        Ok(Self {
            id: config.id.clone(),
            base,
            runtime,
            session,
            connection: Box::new(handle),
        })
    }

    /// 相对连接根的路径 → 服务器上的绝对路径。
    fn absolute(&self, path: &str) -> String {
        absolute(&self.base, path)
    }

    /// 一项的属性；没有这一项就是 `None`。
    ///
    /// 用 `symlink_metadata` 而不是 `metadata`：符号链接本身是什么就是什么，
    /// 不跟进去——删东西时跟着链接走进去会删错人。
    fn attributes(
        &self,
        absolute: &str,
    ) -> Result<Option<russh_sftp::protocol::FileAttributes>, String> {
        self.runtime.block_on(async {
            match self.session.symlink_metadata(absolute).await {
                Ok(info) => Ok(Some(info)),
                Err(_) => Ok(None),
            }
        })
    }

    /// 复制没有标准指令可用：先下到临时文件，再传上去。
    fn copy_through_a_temp_file(&self, from: &str, to: &str) -> Result<(), String> {
        let mut temporary = std::env::temp_dir();
        temporary.push(format!(
            "wfiles-sftp-{}-{}.tmp",
            self.id,
            std::process::id()
        ));
        let result = (|| -> Result<(), String> {
            {
                let mut file = std::fs::File::create(&temporary)
                    .map_err(|error| format!("建不了临时文件：{error}"))?;
                self.download(from, &mut file)?;
            }
            let mut file = std::fs::File::open(&temporary)
                .map_err(|error| format!("读不了临时文件：{error}"))?;
            let length = file
                .metadata()
                .map_err(|error| format!("看不了临时文件：{error}"))?
                .len();
            self.upload(to, &mut file, length)
        })();
        std::fs::remove_file(&temporary).ok();
        result
    }
}

impl Remote for Sftp {
    fn id(&self) -> &str {
        &self.id
    }

    /// 两个方向都能续：SFTP 的读写都带偏移，`seek` 到断点就行。
    /// 四种协议里只有它和 FTP 传下载都接得上。
    fn resumes_download(&self) -> bool {
        true
    }

    fn resumes_upload(&self) -> bool {
        true
    }

    fn list(&self, path: &str) -> Result<Vec<Entry>, String> {
        let absolute = self.absolute(path);
        let entries = self.runtime.block_on(async {
            self.session
                .read_dir(&absolute)
                .await
                .map_err(|error| format!("列目录失败：{error}"))
        })?;
        let mut out = Vec::new();
        for entry in entries {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let info = entry.metadata();
            out.push(Entry {
                path: join(path, &name),
                name,
                is_dir: info.is_dir(),
                size: info.len(),
                modified: info.mtime.map(u64::from),
            });
        }
        Ok(out)
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>, String> {
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
        let Some(info) = self.attributes(&absolute)? else {
            return Ok(None);
        };
        Ok(Some(Entry {
            name: name_of(path),
            path: path.to_string(),
            is_dir: info.is_dir(),
            size: info.len(),
            modified: info.mtime.map(u64::from),
        }))
    }

    fn download(&self, path: &str, sink: &mut dyn Write) -> Result<u64, String> {
        let absolute = self.absolute(path);
        self.pull(&absolute, 0, sink)
    }

    /// 从 `offset` 接着下载：SFTP 的读是带偏移的，把文件光标
    /// `seek` 到断点再照常读就行——russh 那边只是每次读多带一个 offset，
    /// 服务器该回哪一段回哪一段。
    fn download_from(&self, path: &str, offset: u64, sink: &mut dyn Write) -> Result<u64, String> {
        let absolute = self.absolute(path);
        self.pull(&absolute, offset, sink)
    }

    fn upload(&self, path: &str, reader: &mut dyn Read, _length: u64) -> Result<(), String> {
        let absolute = self.absolute(path);
        self.push(&absolute, reader, None)
    }

    /// 从 `offset` 接着上传：照样 `seek` 过去再写。
    ///
    /// 注意 `TRUNCATE` 不能带——带上就等于把服务器上已有的半截先削掉，
    /// 那还续什么传。调用方保证过服务器上正好有 `offset` 个字节，
    /// 所以定位过去写，拼出来才是一个完整的文件。
    fn upload_from(
        &self,
        path: &str,
        reader: &mut dyn Read,
        _length: u64,
        offset: u64,
    ) -> Result<(), String> {
        let absolute = self.absolute(path);
        self.push(&absolute, reader, Some(offset))
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        let absolute = self.absolute(path);
        self.runtime.block_on(async {
            self.session
                .create_dir(&absolute)
                .await
                .map_err(|error| format!("建目录失败：{error}"))
        })
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        let absolute = self.absolute(path);
        // 目录得先把里面清空：RMDIR 也只删空目录，
        // 而用户按删除时想的是"这个文件夹没了"。
        if self
            .attributes(&absolute)?
            .is_some_and(|info| info.is_dir())
        {
            for child in self.list(path)? {
                self.remove(&child.path)?;
            }
            return self.runtime.block_on(async {
                self.session
                    .remove_dir(&absolute)
                    .await
                    .map_err(|error| format!("删目录失败：{error}"))
            });
        }
        self.runtime.block_on(async {
            self.session
                .remove_file(&absolute)
                .await
                .map_err(|error| format!("删除失败：{error}"))
        })
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let (from, to) = (self.absolute(from), self.absolute(to));
        self.runtime.block_on(async {
            self.session
                .rename(&from, &to)
                .await
                .map_err(|error| format!("改名失败：{error}"))
        })
    }

    fn copy(&self, from: &str, to: &str) -> Result<(), String> {
        // SFTP 标准里没有复制指令：只能下到临时文件再传上去。
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

// ---------------------------------------------------------------------------
// 认服务器的公钥
// ---------------------------------------------------------------------------

/// 连接期间的"这台机器我认不认"。
struct Trust {
    host: String,
    port: u16,
    /// known_hosts 的位置。
    file: PathBuf,
    /// `check_server_key` 只能回一个 bool，说不了原因；
    /// 所以把"为什么拒了"存在这里，连完之后再读出来讲给用户听。
    refused: Arc<Mutex<Option<String>>>,
}

impl Trust {
    fn known(&mut self, key: &russh::keys::PublicKey) -> bool {
        let recorded = known_hosts::known_host_keys_path(&self.host, self.port, &self.file)
            .unwrap_or_default();
        // 第一次连这台：记下来（先信一次）。不记的话每次都得问，
        // 而同步的连接过程里问不了——问不了又不敢连，就只剩不能用。
        if recorded.is_empty() {
            if let Err(error) =
                known_hosts::learn_known_hosts_path(&self.host, self.port, key, &self.file)
            {
                return self.refuse(format!("记不下这台服务器的公钥：{error}"));
            }
            return true;
        }
        if recorded.iter().any(|(_, it)| it == key) {
            return true;
        }
        self.refuse(format!(
            "{} 的服务器公钥跟上次不一样了。可能是有人在中间冒充，\
             也可能是服务器重装过——确认没问题就把 {} 里这一行删掉再连",
            self.host,
            self.file.display()
        ))
    }

    fn refuse(&self, reason: String) -> bool {
        if let Ok(mut slot) = self.refused.lock() {
            *slot = Some(reason);
        }
        false
    }
}

impl russh::client::Handler for Trust {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let owned;
        let key: &russh::keys::PublicKey = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            // 证书里装的是裸的 KeyData，不是 PublicKey——补个注释位再比。
            PublicKeyOrCertificate::Certificate(certificate) => {
                owned = russh::keys::PublicKey::new(certificate.public_key().clone(), "");
                &owned
            }
        };
        Ok(self.known(key))
    }
}

/// known_hosts 放在哪儿：跟 OpenSSH 同一份（`~/.ssh/known_hosts`）。
/// 用户已经信过的机器不用再信一次。
fn known_hosts_file() -> PathBuf {
    let mut path = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
    if cfg!(target_os = "windows") {
        path.push("ssh");
    } else {
        path.push(".ssh");
    }
    path.push("known_hosts");
    path
}

// ---------------------------------------------------------------------------
// 路径与地址
// ---------------------------------------------------------------------------

/// 相对路径挂到登录目录下面。
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

/// `sftp://host:22/起始目录` → （主机, 端口）。没写端口就按 22。
fn address_of(url: &str) -> Result<(String, u16), String> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err("地址里没有主机名".to_string());
    }
    match host.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .map_err(|_| format!("端口号不对：{port}"))?;
            Ok((host.trim_matches(['[', ']']).to_string(), port))
        }
        None => Ok((host.to_string(), 22)),
    }
}

/// 地址里带的起始目录：`sftp://host/home/me/` → `home/me`。没写就是空。
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

#[cfg(test)]
mod tests {
    use super::*;

    use russh::server::{Auth, ChannelOpenHandle, Msg, Server as _, Session};
    use russh::{Channel, ChannelId};
    use russh_sftp::protocol::{
        Attrs, Data, File, FileAttributes, Handle as SftpHandle, Name, Status, StatusCode, Version,
    };

    #[test]
    fn the_address_gets_a_default_port() {
        assert_eq!(address_of("sftp://host").unwrap(), ("host".to_string(), 22));
        assert_eq!(
            address_of("sftp://host:2222/home/me/").unwrap(),
            ("host".to_string(), 2222)
        );
        assert_eq!(
            address_of("sftp://[::1]:2222/").unwrap(),
            ("::1".to_string(), 2222)
        );
        assert!(address_of("sftp://").is_err(), "没有主机名该报错");
        assert!(address_of("sftp://host:x/").is_err(), "端口不是数字该报错");
    }

    #[test]
    fn paths_are_joined_with_one_slash() {
        assert_eq!(join("", "a.txt"), "a.txt");
        assert_eq!(join("folder", "a.txt"), "folder/a.txt");
        assert_eq!(name_of("folder/sub/"), "sub");
        assert_eq!(absolute("/home/me", ""), "/home/me");
        assert_eq!(absolute("/home/me", "/folder/sub/"), "/home/me/folder/sub");
        assert_eq!(start_of("sftp://host/"), "");
        assert_eq!(start_of("sftp://host:2222/home/me/"), "home/me");
    }

    // ---- 真连一次（本地起一个假的 SSH + SFTP 服务器）----

    /// 服务器上的"文件系统"：路径 → （是不是目录, 内容）。
    type Tree = std::collections::HashMap<String, (bool, Vec<u8>)>;

    fn seed() -> Tree {
        let mut tree = Tree::new();
        tree.insert("/home/me".to_string(), (true, Vec::new()));
        tree.insert(
            "/home/me/notes.txt".to_string(),
            (false, b"hello sftp".to_vec()),
        );
        tree.insert("/home/me/folder".to_string(), (true, Vec::new()));
        tree.insert("/home/me/folder/b.txt".to_string(), (false, b"bb".to_vec()));
        tree.insert("/home/me/pub".to_string(), (true, Vec::new()));
        tree
    }

    const HOME: &str = "/home/me";
    /// 修改时间随便定一个：1_700_000_000 是 2023 年，看得出是"有值"就行。
    const MTIME: u32 = 1_700_000_000;

    fn attrs_of(is_dir: bool, size: u64) -> FileAttributes {
        let mut attrs = FileAttributes {
            size: Some(size),
            permissions: Some(0o644),
            mtime: Some(MTIME),
            ..Default::default()
        };
        if is_dir {
            attrs.set_dir(true);
        } else {
            attrs.set_regular(true);
        }
        attrs
    }

    fn children_of(tree: &Tree, path: &str) -> Vec<String> {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let mut out: Vec<String> = tree
            .keys()
            .filter(|it| it.starts_with(&prefix) && !it[prefix.len()..].contains('/'))
            .cloned()
            .collect();
        out.sort();
        out
    }

    fn note(seen: &std::sync::Arc<std::sync::Mutex<Vec<String>>>, what: &str) {
        seen.lock().unwrap().push(what.to_string());
    }

    // ---- 假的 SSH 服务器 ----

    struct FakeServer {
        tree: std::sync::Arc<std::sync::Mutex<Tree>>,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl russh::server::Server for FakeServer {
        type Handler = Session1;

        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
            Session1 {
                tree: self.tree.clone(),
                seen: self.seen.clone(),
                channels: std::collections::HashMap::new(),
            }
        }
    }

    struct Session1 {
        tree: std::sync::Arc<std::sync::Mutex<Tree>>,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        channels: std::collections::HashMap<ChannelId, Channel<Msg>>,
    }

    impl russh::server::Handler for Session1 {
        type Error = russh::Error;

        async fn auth_password(
            &mut self,
            _user: &str,
            password: &str,
        ) -> Result<Auth, Self::Error> {
            note(&self.seen, "password");
            Ok(if password == "secret" {
                Auth::Accept
            } else {
                Auth::reject()
            })
        }

        async fn auth_publickey(
            &mut self,
            _user: &str,
            _key: &russh::keys::PublicKey,
        ) -> Result<Auth, Self::Error> {
            note(&self.seen, "publickey");
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            channel: Channel<Msg>,
            reply: ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.channels.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        async fn channel_eof(
            &mut self,
            channel: ChannelId,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            session.close(channel)?;
            Ok(())
        }

        async fn subsystem_request(
            &mut self,
            channel_id: ChannelId,
            name: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if name != "sftp" {
                session.channel_failure(channel_id)?;
                return Ok(());
            }
            let channel = self.channels.remove(&channel_id).expect("通道刚开过");
            session.channel_success(channel_id)?;
            russh_sftp::server::run(
                channel.into_stream(),
                SftpServer {
                    tree: self.tree.clone(),
                    seen: self.seen.clone(),
                    listed: None,
                },
            )
            .await;
            Ok(())
        }
    }

    // ---- 假的 SFTP 子系统 ----

    struct SftpServer {
        tree: std::sync::Arc<std::sync::Mutex<Tree>>,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        /// 已经列完的目录句柄：下一次 readdir 要回 Eof。
        listed: Option<String>,
    }

    impl russh_sftp::server::Handler for SftpServer {
        type Error = StatusCode;

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }

        async fn init(
            &mut self,
            _version: u32,
            _extensions: std::collections::HashMap<String, String>,
        ) -> Result<Version, Self::Error> {
            Ok(Version::new())
        }

        async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
            let path = if path == "." { HOME.to_string() } else { path };
            if !self.tree.lock().unwrap().contains_key(&path) {
                return Err(StatusCode::NoSuchFile);
            }
            Ok(Name {
                id,
                files: vec![File::dummy(path)],
            })
        }

        async fn opendir(&mut self, id: u32, path: String) -> Result<SftpHandle, Self::Error> {
            let tree = self.tree.lock().unwrap();
            if !tree.get(&path).is_some_and(|(is_dir, _)| *is_dir) {
                return Err(StatusCode::NoSuchFile);
            }
            Ok(SftpHandle { id, handle: path })
        }

        async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
            // 目录一次给完，下一次回 Eof——真服务器也是这么收尾的。
            if self.listed.as_deref() == Some(handle.as_str()) {
                return Err(StatusCode::Eof);
            }
            self.listed = Some(handle.clone());
            let tree = self.tree.lock().unwrap();
            let files = children_of(&tree, &handle)
                .into_iter()
                .map(|path| {
                    let (is_dir, content) = &tree[&path];
                    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                    File::new(name, attrs_of(*is_dir, content.len() as u64))
                })
                .collect();
            Ok(Name { id, files })
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            self.lstat(id, path).await
        }

        async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            let tree = self.tree.lock().unwrap();
            let Some((is_dir, content)) = tree.get(&path) else {
                return Err(StatusCode::NoSuchFile);
            };
            Ok(Attrs {
                id,
                attrs: attrs_of(*is_dir, content.len() as u64),
            })
        }

        async fn open(
            &mut self,
            id: u32,
            filename: String,
            flags: OpenFlags,
            _attrs: FileAttributes,
        ) -> Result<SftpHandle, Self::Error> {
            let mut tree = self.tree.lock().unwrap();
            if flags.contains(OpenFlags::CREATE) {
                tree.entry(filename.clone()).or_insert((false, Vec::new()));
            }
            if !tree.contains_key(&filename) {
                return Err(StatusCode::NoSuchFile);
            }
            Ok(SftpHandle {
                id,
                handle: filename,
            })
        }

        async fn read(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            len: u32,
        ) -> Result<Data, Self::Error> {
            let tree = self.tree.lock().unwrap();
            let Some((_, content)) = tree.get(&handle) else {
                return Err(StatusCode::NoSuchFile);
            };
            let at = offset as usize;
            if at >= content.len() {
                return Err(StatusCode::Eof);
            }
            let end = (at + len as usize).min(content.len());
            Ok(Data {
                id,
                data: content[at..end].to_vec(),
            })
        }

        async fn write(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            data: Vec<u8>,
        ) -> Result<Status, Self::Error> {
            let mut tree = self.tree.lock().unwrap();
            let Some((_, content)) = tree.get_mut(&handle) else {
                return Err(StatusCode::NoSuchFile);
            };
            let at = offset as usize;
            if at + data.len() > content.len() {
                content.resize(at + data.len(), 0);
            }
            content[at..at + data.len()].copy_from_slice(&data);
            note(&self.seen, &format!("write {handle}"));
            Ok(Status {
                id,
                status_code: StatusCode::Ok,
                error_message: "Ok".to_string(),
                language_tag: "en-US".to_string(),
            })
        }

        async fn close(&mut self, id: u32, _handle: String) -> Result<Status, Self::Error> {
            Ok(Status {
                id,
                status_code: StatusCode::Ok,
                error_message: "Ok".to_string(),
                language_tag: "en-US".to_string(),
            })
        }

        async fn mkdir(
            &mut self,
            id: u32,
            path: String,
            _attrs: FileAttributes,
        ) -> Result<Status, Self::Error> {
            self.tree.lock().unwrap().insert(path, (true, Vec::new()));
            note(&self.seen, "mkdir");
            Ok(ok(id))
        }

        async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
            self.tree.lock().unwrap().remove(&path);
            note(&self.seen, "rmdir");
            Ok(ok(id))
        }

        async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
            self.tree.lock().unwrap().remove(&filename);
            note(&self.seen, "remove");
            Ok(ok(id))
        }

        async fn rename(
            &mut self,
            id: u32,
            oldpath: String,
            newpath: String,
        ) -> Result<Status, Self::Error> {
            let mut tree = self.tree.lock().unwrap();
            if let Some(entry) = tree.remove(&oldpath) {
                tree.insert(newpath, entry);
            }
            note(&self.seen, "rename");
            Ok(ok(id))
        }
    }

    fn ok(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        }
    }

    // ---- 起服务器、连客户端 ----

    struct Fake {
        address: String,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        tree: std::sync::Arc<std::sync::Mutex<Tree>>,
    }

    fn fake_sftp() -> Fake {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("该能监听");
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let tree = std::sync::Arc::new(std::sync::Mutex::new(seed()));
        let seen_for_thread = seen.clone();
        let tree_for_thread = tree.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("该能起运行时");
            runtime.block_on(async move {
                listener.set_nonblocking(true).expect("该能切成非阻塞");
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("该能转成 tokio 的");
                let config = russh::server::Config {
                    keys: vec![host_key()],
                    auth_rejection_time: Duration::from_millis(0),
                    ..Default::default()
                };
                let mut server = FakeServer {
                    tree: tree_for_thread,
                    seen: seen_for_thread,
                };
                // 一直开到测试结束：这个线程随进程走，不需要收。
                let running = server.run_on_socket(std::sync::Arc::new(config), &listener);
                let _ = running.await;
            });
        });
        Fake {
            address: format!("127.0.0.1:{port}"),
            seen,
            tree,
        }
    }

    /// 服务器与客户端共用的那把钥匙（测试专用的固定 Ed25519 私钥）。
    ///
    /// 不现场生成是因为 ssh-key 要的 `CryptoRng` 跟这边能拿到的随机数对不上版本，
    /// 而测试里也用不着真随机——连得上就行。
    const TEST_KEY: &str = concat!(
        "-----BEGIN OPENSSH PRIVATE KEY-----\n",
        "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n",
        "QyNTUxOQAAACAs9Y3kgOiGBbcS+MVGno+BVc8E7SjFzwjs2vykVLeAOAAAAJBLJSCVSyUg\n",
        "lQAAAAtzc2gtZWQyNTUxOQAAACAs9Y3kgOiGBbcS+MVGno+BVc8E7SjFzwjs2vykVLeAOA\n",
        "AAAEBEJZ47nKZZ+pSEq/oFM5M04OEuEzBeOxrrm4WYl7EW3Cz1jeSA6IYFtxL4xUaej4FV\n",
        "zwTtKMXPCOza/KRUt4A4AAAAC3dmaWxlcyB0ZXN0AQI=\n",
        "-----END OPENSSH PRIVATE KEY-----\n",
    );

    /// 另一把钥匙：用来造"上次连的不是这台"这个局面。
    const OTHER_KEY: &str = concat!(
        "-----BEGIN OPENSSH PRIVATE KEY-----\n",
        "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n",
        "QyNTUxOQAAACAcktIH/22BSzDtL/xYO5h3gY/HR5b0eKHj8Boy8JPY6QAAAJBKMzZ8SjM2\n",
        "fAAAAAtzc2gtZWQyNTUxOQAAACAcktIH/22BSzDtL/xYO5h3gY/HR5b0eKHj8Boy8JPY6Q\n",
        "AAAEA23qRAyD8k9bTw0jg+XY04Ndvd/jKqUdsS6OWxyx/xDRyS0gf/bYFLMO0v/Fg7mHeB\n",
        "j8dHlvR4oePwGjLwk9jpAAAADHdmaWxlcyBvdGhlcgE=\n",
        "-----END OPENSSH PRIVATE KEY-----\n",
    );

    fn host_key() -> russh::keys::PrivateKey {
        russh::keys::decode_secret_key(TEST_KEY, None).expect("这把测试钥匙该读得出来")
    }

    /// 连一次。`key` 留空就是密码登录，填了就走密钥登录。
    fn client_at(fake: &Fake, known: &std::path::Path, key: &str) -> Sftp {
        let mut config = Config::new(
            "s1".to_string(),
            "测试".to_string(),
            crate::remote::Kind::Sftp,
            format!("sftp://{}/", fake.address),
            "me".to_string(),
        );
        if !key.is_empty() {
            config = config.with_key(key.to_string());
        }
        Sftp::connect(
            &config,
            if key.is_empty() { "secret" } else { "" },
            known.to_path_buf(),
        )
        .expect("该连得上")
    }

    #[test]
    fn logging_in_lands_on_the_home_directory() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let ftp = client_at(&fake, &dir.path().join("known_hosts"), "");
        // 所有相对路径都挂在登录目录下面。
        assert_eq!(ftp.base, HOME);
        assert!(fake.seen.lock().unwrap().contains(&"password".to_string()));
    }

    #[test]
    fn a_first_connection_records_the_host_key_and_the_second_one_reuses_it() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let known = dir.path().join("known_hosts");

        client_at(&fake, &known, "");
        let written = std::fs::read_to_string(&known).expect("该记下来了");
        assert!(
            written.contains(&format!(
                "[127.0.0.1]:{} ",
                fake.address.rsplit_once(':').unwrap().1
            )),
            "known_hosts 里该有这一台：{written}"
        );
        // 第二次连：钥匙没换，不该被拒。
        client_at(&fake, &known, "");
    }

    #[test]
    fn a_host_key_that_changed_is_refused() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let known = dir.path().join("known_hosts");
        let port = fake.address.rsplit_once(':').unwrap().1;
        // 先记一把别的钥匙：等于"上次连的不是这台"。
        let other = russh::keys::decode_secret_key(OTHER_KEY, None).unwrap();
        std::fs::write(
            &known,
            format!(
                "[127.0.0.1]:{port} {}",
                other.public_key().to_openssh().unwrap()
            ),
        )
        .unwrap();

        let config = Config::new(
            "s1".to_string(),
            "测试".to_string(),
            crate::remote::Kind::Sftp,
            format!("sftp://{}/", fake.address),
            "me".to_string(),
        );
        let error = Sftp::connect(&config, "secret", known).err().unwrap();
        assert!(error.contains("公钥"), "该说清楚是钥匙换了：{error}");
    }

    #[test]
    fn logging_in_with_a_key_instead_of_a_password() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id_ed25519");
        std::fs::write(
            &path,
            host_key()
                .to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();

        let ftp = client_at(
            &fake,
            &dir.path().join("known_hosts"),
            &path.display().to_string(),
        );
        assert_eq!(ftp.base, HOME);
        assert!(fake.seen.lock().unwrap().contains(&"publickey".to_string()));
    }

    #[test]
    fn listing_reads_the_remote_directory() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let ftp = client_at(&fake, &dir.path().join("known_hosts"), "");

        let entries = ftp.list("").expect("该列得出来");
        let names: Vec<&str> = entries.iter().map(|it| it.name.as_str()).collect();
        assert!(names.contains(&"notes.txt"), "{names:?}");
        assert!(
            entries.iter().any(|it| it.name == "folder" && it.is_dir),
            "{entries:?}"
        );
    }

    #[test]
    fn stat_tells_a_file_from_a_folder() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let ftp = client_at(&fake, &dir.path().join("known_hosts"), "");

        let file = ftp
            .stat("notes.txt")
            .expect("该问得出来")
            .expect("该文件在");
        assert!(!file.is_dir);
        assert_eq!(file.size, 10);
        assert_eq!(file.modified, Some(u64::from(MTIME)));

        assert!(ftp.stat("folder").unwrap().unwrap().is_dir);
        assert_eq!(ftp.stat("不在的东西.txt").unwrap(), None);
    }

    #[test]
    fn downloading_writes_the_bytes_out() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let ftp = client_at(&fake, &dir.path().join("known_hosts"), "");

        let mut sink = Vec::new();
        let written = ftp.download("notes.txt", &mut sink).expect("该下得来");
        assert_eq!(written, 10);
        assert_eq!(String::from_utf8(sink).unwrap(), "hello sftp");
    }

    #[test]
    fn uploading_reaches_the_server() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let ftp = client_at(&fake, &dir.path().join("known_hosts"), "");

        let mut body = std::io::Cursor::new(b"hello up".to_vec());
        ftp.upload("folder/new.txt", &mut body, 8)
            .expect("该传得上去");

        let tree = fake.tree.lock().unwrap();
        assert_eq!(
            tree.get("/home/me/folder/new.txt")
                .map(|(_, it)| it.clone()),
            Some(b"hello up".to_vec())
        );
    }

    #[test]
    fn renaming_moves_the_file() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let ftp = client_at(&fake, &dir.path().join("known_hosts"), "");

        ftp.rename("notes.txt", "renamed.txt").expect("该改得动");
        let tree = fake.tree.lock().unwrap();
        assert!(!tree.contains_key("/home/me/notes.txt"));
        assert!(tree.contains_key("/home/me/renamed.txt"));
    }

    #[test]
    fn removing_a_folder_clears_it_first() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let ftp = client_at(&fake, &dir.path().join("known_hosts"), "");

        ftp.remove("folder").expect("该删得掉");
        let seen = fake.seen.lock().unwrap().clone();
        // RMDIR 只删空目录：里面的 b.txt 得先被删掉。
        assert_eq!(
            seen.iter().filter(|it| *it == "remove").count(),
            1,
            "{seen:?}"
        );
        assert!(seen.contains(&"rmdir".to_string()), "{seen:?}");
    }

    #[test]
    fn making_a_folder_creates_it() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let ftp = client_at(&fake, &dir.path().join("known_hosts"), "");

        ftp.mkdir("新建的").expect("该建得了");
        assert!(fake.tree.lock().unwrap().contains_key("/home/me/新建的"));
    }

    #[test]
    fn copying_goes_through_a_local_temp_file() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let ftp = client_at(&fake, &dir.path().join("known_hosts"), "");

        // SFTP 标准里没有复制：只能下到临时文件再传上去。
        ftp.copy("notes.txt", "copy.txt").expect("该复制得动");
        let tree = fake.tree.lock().unwrap();
        assert_eq!(
            tree.get("/home/me/copy.txt").map(|(_, it)| it.clone()),
            Some(b"hello sftp".to_vec())
        );
    }

    #[test]
    fn the_starting_directory_comes_from_the_address() {
        let fake = fake_sftp();
        let dir = tempfile::tempdir().unwrap();
        let config = Config::new(
            "s1".to_string(),
            "测试".to_string(),
            crate::remote::Kind::Sftp,
            format!("sftp://{}/pub", fake.address),
            "me".to_string(),
        );
        let ftp =
            Sftp::connect(&config, "secret", dir.path().join("known_hosts")).expect("该连得上");
        assert_eq!(ftp.base, "/home/me/pub");
    }
}
