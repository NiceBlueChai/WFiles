//! 远程连接的管理：保存、连上、在后台列目录。
//!
//! 跟缩略图那一层一个思路：**问一次，没有就后台去取**。
//! 界面线程上不做网络——一个卡住的服务器不能把整个窗口冻住。
//!
//! 密码不经过这里落盘：写进系统钥匙串是 [`remote::save_password`] 的事，
//! 这里只负责在需要时把它取出来递给客户端。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui_kit::{AsyncApp, Context, EventEmitter, WeakEntity};

use crate::remote::{self, Client, Config, Entry};
use crate::settings::Settings;

/// 后台回来了什么。
#[derive(Clone, Debug)]
pub enum RemotesEvent {
    /// 一个远程目录列出来了（`error` 非空就是列失败了）。
    Listed {
        path: PathBuf,
        error: Option<String>,
    },
    /// 连上了并已保存（密码进了系统钥匙串）。
    Connected { id: String, name: String },
    /// 连接失败。
    Failed(String),
}

pub struct Remotes {
    /// 已建好的客户端，按连接 id 存。改密码或删连接时清掉对应的一条。
    clients: HashMap<String, Client>,
    /// 列过的目录内容，按远程路径存。
    listed: HashMap<PathBuf, Vec<Entry>>,
    /// 正在取的，别重复排队。
    pending: HashSet<PathBuf>,
    /// 取失败的：记下来，别每渲染一次就重试一次注定失败的服务器。
    failed: HashSet<PathBuf>,
}

impl EventEmitter<RemotesEvent> for Remotes {}

impl Default for Remotes {
    fn default() -> Self {
        Self::new()
    }
}

impl Remotes {
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
            listed: HashMap::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
        }
    }

    /// 设置里存着的连接。
    pub fn saved(&self, cx: &mut Context<Self>) -> Vec<Config> {
        Settings::peek(cx, |settings| settings.remotes.clone())
    }

    /// 存一条连接：配置进设置，密码进钥匙串。
    pub fn save(
        &mut self,
        config: Config,
        password: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        remote::save_password(&config, password)?;
        Settings::update(cx, |settings| {
            if let Some(at) = settings.remotes.iter().position(|it| it.id == config.id) {
                settings.remotes[at] = config.clone();
            } else {
                settings.remotes.push(config.clone());
            }
        });
        // 密码可能换了：旧客户端不能再用了。
        self.clients.remove(&config.id);
        Ok(())
    }

    /// 删掉一条连接：配置、密码、客户端、缓存，一样不留。
    pub fn forget(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(config) = self.saved(cx).into_iter().find(|it| it.id == id) else {
            return;
        };
        remote::forget_password(&config);
        Settings::update(cx, |settings| settings.remotes.retain(|it| it.id != id));
        self.clients.remove(id);
        self.listed
            .retain(|path, _| remote::parse_location(path).is_none_or(|(it, _)| it != id));
    }

    /// 给连接面板用的 id：名字转成能放进路径的样子，重名就加序号。
    pub fn fresh_id(&self, name: &str, cx: &mut Context<Self>) -> String {
        let slug: String = name
            .chars()
            .map(|character| {
                if character.is_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let slug = slug.trim_matches('-').to_string();
        let base = if slug.is_empty() {
            "remote".to_string()
        } else {
            slug
        };
        let taken = self
            .saved(cx)
            .into_iter()
            .filter(|it| it.id.starts_with(&base))
            .count();
        if taken == 0 {
            base
        } else {
            format!("{base}-{taken}")
        }
    }

    /// 试连一次（后台）：通了就存下来并回报密码落在哪，不通就报原因。
    pub fn connect(&mut self, config: Config, password: &str, cx: &mut Context<Self>) {
        let (tx, rx) = async_channel::bounded(1);
        let worker = config.clone();
        let secret = password.to_string();
        let for_thread = secret.clone();
        let spawn = std::thread::Builder::new()
            .name("wfiles-connect".to_string())
            .spawn(move || {
                let result = remote::probe(&worker, &for_thread);
                tx.try_send(result).ok();
            });
        if let Err(error) = spawn {
            cx.emit(RemotesEvent::Failed(format!("起不了后台线程：{error}")));
            return;
        }

        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let mut cx = cx.clone();
            let Ok(result) = rx.recv().await else {
                return;
            };
            this.update(&mut cx, |remotes, cx| {
                match result {
                    Ok(_) => match remotes.save(config.clone(), &secret, cx) {
                        Ok(()) => cx.emit(RemotesEvent::Connected {
                            id: config.id.clone(),
                            name: config.name.clone(),
                        }),
                        Err(error) => cx.emit(RemotesEvent::Failed(error)),
                    },
                    Err(error) => cx.emit(RemotesEvent::Failed(error)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 求一个远程目录的内容。
    ///
    /// 有缓存就当场给（滚动、重画不该反复问服务器）；
    /// 没有就登记一个后台任务并返回 `None`，取回来会发 [`RemotesEvent::Listed`]。
    pub fn list(&mut self, id: &str, sub: &str, cx: &mut Context<Self>) -> Option<Vec<Entry>> {
        let path = remote::location(id, sub);
        if let Some(cached) = self.listed.get(&path) {
            return Some(cached.clone());
        }
        if self.pending.contains(&path) || self.failed.contains(&path) {
            return None;
        }
        let client = self.client(id, cx)?;
        self.pending.insert(path.clone());
        Self::spawn_list(client, path, sub.to_string(), cx);
        None
    }

    /// 重新取一次（刷新按钮、文件操作之后）：先丢掉旧的缓存。
    pub fn reload(&mut self, id: &str, sub: &str, cx: &mut Context<Self>) {
        let path = remote::location(id, sub);
        self.listed.remove(&path);
        self.failed.remove(&path);
        let _ = self.list(id, sub, cx);
    }

    /// 取回来的目录内容（界面重画时用）。
    pub fn entries(&self, path: &Path) -> Option<&Vec<Entry>> {
        self.listed.get(path)
    }

    /// 取一个客户端：有现成的就用，没有就按设置里的配置建一个。
    ///
    /// 公开是为了让操作层能在主线程上把客户端取好、再带进后台线程。
    pub fn client(&mut self, id: &str, cx: &mut Context<Self>) -> Option<Client> {
        if let Some(client) = self.clients.get(id) {
            return Some(client.clone());
        }
        let config = self.saved(cx).into_iter().find(|it| it.id == id)?;
        let password = remote::load_password(&config).unwrap_or_default();
        let client = remote::open(&config, &password).ok()?;
        self.clients.insert(id.to_string(), client.clone());
        Some(client)
    }

    fn spawn_list(client: Client, path: PathBuf, sub: String, cx: &mut Context<Self>) {
        let (tx, rx) = async_channel::bounded(1);
        let spawn = std::thread::Builder::new()
            .name("wfiles-remote-list".to_string())
            .spawn(move || {
                let result = client.list(&sub);
                tx.try_send(result).ok();
            });
        if let Err(error) = spawn {
            cx.emit(RemotesEvent::Failed(format!("起不了后台线程：{error}")));
            return;
        }
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let mut cx = cx.clone();
            let Ok(result) = rx.recv().await else {
                return;
            };
            this.update(&mut cx, |remotes, cx| {
                remotes.pending.remove(&path);
                match result {
                    Ok(entries) => {
                        remotes.listed.insert(path.clone(), entries);
                        cx.emit(RemotesEvent::Listed { path, error: None });
                    }
                    Err(error) => {
                        remotes.failed.insert(path.clone());
                        cx.emit(RemotesEvent::Listed {
                            path,
                            error: Some(error),
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 双击远程文件：下载到系统临时目录，再交给系统打开。
    ///
    /// 没做"远程编辑回传"（那是 P4 的可选项）：这里给的是"先看起来"，
    /// 而改完的东西留在临时目录里，系统会自己清掉。
    pub fn open_file(&mut self, id: &str, sub: &str, cx: &mut Context<Self>) {
        let Some(client) = self.client(id, cx) else {
            cx.emit(RemotesEvent::Failed("没有这条连接".to_string()));
            return;
        };
        let name = sub.rsplit('/').next().unwrap_or("file").to_string();
        let sub = sub.to_string();
        let (tx, rx) = async_channel::bounded(1);
        let spawn = std::thread::Builder::new()
            .name("wfiles-remote-open".to_string())
            .spawn(move || {
                let result = download_to_temp(client.as_ref(), &sub, &name);
                tx.try_send(result).ok();
            });
        if let Err(error) = spawn {
            cx.emit(RemotesEvent::Failed(format!("起不了后台线程：{error}")));
            return;
        }
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let mut cx = cx.clone();
            let Ok(result) = rx.recv().await else {
                return;
            };
            this.update(&mut cx, |_, cx| {
                if let Err(error) = result {
                    cx.emit(RemotesEvent::Failed(error));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// 下载到临时目录并用系统打开（跑在后台线程上）。
fn download_to_temp(client: &dyn remote::Remote, sub: &str, name: &str) -> Result<(), String> {
    let dir = std::env::temp_dir().join("wfiles-remote");
    std::fs::create_dir_all(&dir).map_err(|error| format!("建不了临时目录：{error}"))?;
    let target = dir.join(name);
    let mut file =
        std::fs::File::create(&target).map_err(|error| format!("写不了临时文件：{error}"))?;
    client
        .download(sub, &mut file)
        .map_err(|error| format!("下载失败：{error}"))?;
    drop(file);
    open::that(&target).map_err(|error| format!("打不开：{error}"))
}
