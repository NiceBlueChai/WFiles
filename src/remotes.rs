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
        error: Option<crate::i18n::Msg>,
    },
    /// 连上了并已保存（密码进了系统钥匙串）。
    Connected { id: String, name: String },
    /// 连接失败。
    Failed(crate::i18n::Msg),
    /// 编辑中的远程文件：改了就会自动回传，这里只是让界面知道有这一回事。
    Editing { name: String },
    /// 一次自动回传的结果。
    Uploaded {
        name: String,
        error: Option<crate::i18n::Msg>,
    },
}

pub struct Remotes {
    /// 已建好的客户端，按连接 id 存。改密码或删连接时清掉对应的一条。
    clients: HashMap<String, Client>,
    /// 列过的目录内容，按远程路径存。
    listed: HashMap<PathBuf, Vec<Entry>>,
    /// 正在显示的这份是磁盘缓存里的旧数据（值是列于何时，Unix 秒）。
    ///
    /// 跟 `listed` 分开而不是塞进 `Entry`：同一个 `listed` 里既有刚列来的
    /// 又有磁盘上翻出来的，得能一眼分清哪些要标"列于 N 前"。
    stale: HashMap<PathBuf, u64>,
    /// 正在取的，别重复排队。
    pending: HashSet<PathBuf>,
    /// 取失败的：记下来，别每渲染一次就重试一次注定失败的服务器。
    failed: HashSet<PathBuf>,
    /// 正在编辑的远程文件：本地临时文件 → 服务器上那一份。
    editing: Vec<Edit>,
    /// 临时目录的监听任务。只要有一个文件在编辑就有它。
    edit_task: Option<gpui_kit::Task<()>>,
}

/// 一个正在编辑的远程文件。
struct Edit {
    /// 哪条连接。
    id: String,
    /// 服务器上的相对路径。
    sub: String,
    /// 本地临时文件（系统临时目录底下，系统自己会清）。
    local: PathBuf,
    /// 上一次回传时本地文件长什么样：(字节数, 修改时间)。一样就不必再传。
    ///
    /// 用指纹而不是"监听事件一来就传"：编辑器保存一次会连着写好几个文件
    /// （临时文件、备份、原子替换），只认指纹才能既不多传也不漏传。
    sent: Option<Fingerprint>,
    /// 正在回传：连着两个事件不该传两遍。
    busy: bool,
}

/// 一个文件此刻的样子。改了就跟上一次不一样。
type Fingerprint = (u64, Option<u64>);

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
            stale: HashMap::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            editing: Vec::new(),
            edit_task: None,
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
    ) -> Result<(), crate::i18n::Msg> {
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
        self.stale
            .retain(|path, _| remote::parse_location(path).is_none_or(|(it, _)| it != id));
        // 磁盘上那份也一起删：连接都没了，留着别人的目录列表没有意义，
        // 而且下次新建一个同 id 的连接会读到上一世的数据。
        crate::offline::Store::new().forget(id);
        // 连接都没了，编辑中的那份自然回传不回去：清掉，免得报错弹个不停。
        self.editing.retain(|edit| edit.id != id);
        if self.editing.is_empty() {
            self.edit_task = None;
        }
        std::fs::remove_dir_all(edit_dir(id)).ok();
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
            cx.emit(RemotesEvent::Failed(crate::i18n::Message::args(
                "起不了后台线程：{error}",
                &[("error", error.to_string())],
            )));
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
    /// 三个来源，按优先级：
    ///
    /// 1. 内存里有（刚列过）——直接给，滚动重画不该反复问服务器。
    /// 2. 磁盘上有——**垫上并继续后台问真的**。这一条是关键：
    ///    断网时用户看到的是上次列的那份，而不是一片空白；
    ///    而网络通着的时候，后台那一趟回来会把它换成新的。
    ///    只垫不问是错的：那等于把"离线可用"做成了"永远显示旧的"。
    /// 3. 两处都没有——只登记后台任务，界面等着。
    ///
    /// 垫上的时候记下它有多旧，界面据此写明"列于 3 小时前"。
    pub fn list(&mut self, id: &str, sub: &str, cx: &mut Context<Self>) -> Option<Vec<Entry>> {
        let path = remote::location(id, sub);
        if let Some(cached) = self.listed.get(&path) {
            return Some(cached.clone());
        }
        let offline = crate::offline::Store::new().load(id, sub);
        if let Some((entries, at)) = &offline {
            self.listed.insert(path.clone(), entries.clone());
            self.stale.insert(path.clone(), *at);
        }
        // 不管有没有垫上，都去问一次真的服务器（下面那几行）。
        if self.pending.contains(&path) || self.failed.contains(&path) {
            return offline.map(|(entries, _)| entries);
        }
        let Some(client) = self.client(id, cx) else {
            return offline.map(|(entries, _)| entries);
        };
        self.pending.insert(path.clone());
        Self::spawn_list(client, path, sub.to_string(), cx);
        offline.map(|(entries, _)| entries)
    }

    /// 这个目录现在显示的是不是缓存里那份旧的（以及列于何时）。
    ///
    /// 界面拿它写"列于 3 小时前"那一行；`None` 就是新鲜的。
    pub fn staleness(&self, path: &Path) -> Option<u64> {
        self.stale.get(path).copied()
    }

    /// 重新取一次（刷新按钮、文件操作之后）：先丢掉旧的缓存。
    ///
    /// 磁盘上那份也得跳过——用户点了刷新就是要最新的，
    /// 拿一份三小时前的列表顶上等于没点。
    pub fn reload(&mut self, id: &str, sub: &str, cx: &mut Context<Self>) {
        let path = remote::location(id, sub);
        self.listed.remove(&path);
        self.stale.remove(&path);
        self.failed.remove(&path);
        if self.pending.contains(&path) {
            return;
        }
        let Some(client) = self.client(id, cx) else {
            return;
        };
        self.pending.insert(path.clone());
        Self::spawn_list(client, path, sub.to_string(), cx);
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
        // 落盘要连连接 id，所以在进线程前就问出来。
        let id = client.id().to_string();
        // 列目录那一趟会把 `sub` move 走，落盘那份自己留一份。
        let for_disk_sub = sub.clone();
        let spawn = std::thread::Builder::new()
            .name("wfiles-remote-list".to_string())
            .spawn(move || {
                let result = client.list(&sub);
                tx.try_send(result).ok();
            });
        if let Err(error) = spawn {
            cx.emit(RemotesEvent::Failed(crate::i18n::Message::args(
                "起不了后台线程：{error}",
                &[("error", error.to_string())],
            )));
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
                        // 落盘在后台线程上做：写缓存是一次磁盘 IO，
                        // 不该让界面线程等它。线程 detach 不等——但存的是
                        // 可再生的东西，写了一半也只是没有这一份离线缓存，
                        // 不会让界面上的数据出错。
                        let keep = entries.clone();
                        let for_disk = (id.clone(), for_disk_sub.clone());
                        std::thread::Builder::new()
                            .name("wfiles-offline-list".to_string())
                            .spawn(move || {
                                crate::offline::Store::new().save(&for_disk.0, &for_disk.1, &keep);
                            })
                            .ok();
                        remotes.listed.insert(path.clone(), entries);
                        // 拿到真的了，"这是旧的"那一行就该撤掉。
                        remotes.stale.remove(&path);
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
    /// 只是"先看起来"：这一份不回传，改完的东西留在临时目录里，系统会自己清掉。
    /// 要改了传回去用 [`Self::edit_file`]。
    pub fn open_file(&mut self, id: &str, sub: &str, cx: &mut Context<Self>) {
        self.download_and_open(id, sub, false, cx)
    }

    /// 编辑远程文件：下载到本地临时目录、交给系统打开，之后一保存就自动回传。
    ///
    /// 为什么另开一条命令而不是双击就回传：**静默覆盖服务器上的文件是很危险的**。
    /// 双击多半只是"看一眼"，要是看图软件顺手改了一下元数据就把服务器上的原件
    /// 盖掉，那是灾难。所以回传只发生在用户明确点了"编辑"的那些文件上。
    pub fn edit_file(&mut self, id: &str, sub: &str, cx: &mut Context<Self>) {
        self.download_and_open(id, sub, true, cx)
    }

    fn download_and_open(
        &mut self,
        id: &str,
        sub: &str,
        for_editing: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client(id, cx) else {
            cx.emit(RemotesEvent::Failed(crate::i18n::Message::msg(
                "没有这条连接",
            )));
            return;
        };
        let name = sub.rsplit('/').next().unwrap_or("file").to_string();
        let local = temp_file(id, &name);
        if let Err(error) = std::fs::create_dir_all(local.parent().unwrap_or(Path::new("."))) {
            cx.emit(RemotesEvent::Failed(crate::i18n::Message::args(
                "建不了临时目录：{error}",
                &[("error", error.to_string())],
            )));
            return;
        }
        let sub = sub.to_string();
        let owner = id.to_string();
        let back_to = sub.clone();
        // 下载线程拿 `local`，返回之后主线程还要用它开编辑器和算指纹。
        let opened = local.clone();
        let (tx, rx) = async_channel::bounded(1);
        let spawn = std::thread::Builder::new()
            .name("wfiles-remote-open".to_string())
            .spawn(move || {
                let result = download_to(client.as_ref(), &back_to, &local);
                tx.try_send(result).ok();
            });
        if let Err(error) = spawn {
            cx.emit(RemotesEvent::Failed(crate::i18n::Message::args(
                "起不了后台线程：{error}",
                &[("error", error.to_string())],
            )));
            return;
        }
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let mut cx = cx.clone();
            let Ok(result) = rx.recv().await else {
                return;
            };
            this.update(&mut cx, |remotes, cx| {
                if let Err(error) = result {
                    cx.emit(RemotesEvent::Failed(error));
                    cx.notify();
                    return;
                }
                if !for_editing {
                    // 只看：打开就完事，不登记、不监听。
                    if let Err(error) = open::that(&opened) {
                        cx.emit(RemotesEvent::Failed(crate::i18n::Message::args(
                            "打不开：{error}",
                            &[("error", error.to_string())],
                        )));
                    }
                    cx.notify();
                    return;
                }
                let name = remotes.note_edit(owner, sub, &opened, cx);
                if let Err(error) = open::that(&opened) {
                    cx.emit(RemotesEvent::Failed(crate::i18n::Message::args(
                        "打不开：{error}",
                        &[("error", error.to_string())],
                    )));
                } else {
                    cx.emit(RemotesEvent::Editing { name });
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 登记一个"正在本地编辑"的文件：从这一刻起它一被保存就回传。
    /// 返回文件名，界面拿它写提示。
    ///
    /// 记下**当下**的指纹：刚下载下来那一份本来就和服务器一致，
    /// 不该在编辑器第一次碰它之前就白传一趟。
    fn note_edit(
        &mut self,
        id: String,
        sub: String,
        local: &Path,
        cx: &mut Context<Self>,
    ) -> String {
        let name = local
            .file_name()
            .map(|it| it.to_string_lossy().to_string())
            .unwrap_or_default();
        self.editing.push(Edit {
            id,
            sub,
            sent: fingerprint(local),
            local: local.to_path_buf(),
            busy: false,
        });
        self.watch_edits(cx);
        name
    }

    /// 开始监听临时目录：编辑器一保存，事件就到，回传跟着走。
    ///
    /// 只起一次：多个文件共用一个监听任务，任务自己会去抖。
    fn watch_edits(&mut self, cx: &mut Context<Self>) {
        if self.edit_task.is_some() {
            return;
        }
        let root = edit_root();
        let Ok((watcher, rx)) = crate::watch::watch_directory(&root) else {
            return;
        };
        self.edit_task = Some(
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let mut cx = cx.clone();
                // watcher 活在任务里：任务被丢弃时监听停止。
                let _watcher = watcher;
                while rx.recv().await.is_ok() {
                    // 去抖：保存一次会有好几个事件（写临时文件、改名替换），
                    // 等安静下来再看指纹——指纹一样就一个字节都不传。
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(200))
                        .await;
                    while rx.try_recv().is_ok() {}
                    this.update(&mut cx, |remotes, cx| remotes.upload_edits(cx))
                        .ok();
                }
            }),
        );
    }

    /// 把改过的文件传回服务器。指纹没变的不传。
    fn upload_edits(&mut self, cx: &mut Context<Self>) {
        let due: Vec<(usize, String, String, PathBuf, Fingerprint)> = self
            .editing
            .iter()
            .enumerate()
            .filter(|(_, edit)| !edit.busy)
            .filter_map(|(at, edit)| {
                let now = fingerprint(&edit.local)?;
                if edit.sent == Some(now) {
                    return None;
                }
                Some((
                    at,
                    edit.id.clone(),
                    edit.sub.clone(),
                    edit.local.clone(),
                    now,
                ))
            })
            .collect();

        for (at, id, sub, local, sent) in due {
            // 连接没了（改过密码、删过连接）就别再报错，安静地跳过。
            let Some(client) = self.client(&id, cx) else {
                continue;
            };
            self.editing[at].busy = true;
            let name = Path::new(&sub)
                .file_name()
                .map(|it| it.to_string_lossy().to_string())
                .unwrap_or_default();
            let (tx, rx) = async_channel::bounded(1);
            let spawn = std::thread::Builder::new()
                .name("wfiles-remote-upload".to_string())
                .spawn(move || {
                    let result = upload_to(client.as_ref(), &sub, &local);
                    tx.try_send(result).ok();
                });
            if let Err(error) = spawn {
                self.editing[at].busy = false;
                cx.emit(RemotesEvent::Failed(crate::i18n::Message::args(
                    "起不了后台线程：{error}",
                    &[("error", error.to_string())],
                )));
                continue;
            }
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let mut cx = cx.clone();
                let Ok(result) = rx.recv().await else {
                    return;
                };
                this.update(&mut cx, |remotes, cx| {
                    let error = result.err();
                    if let Some(edit) = remotes.editing.get_mut(at) {
                        edit.busy = false;
                        // 只在真的传成了才记账：失败那趟不记，
                        // 下一次保存还会再试，而不是"反正记过了"就此不传。
                        if error.is_none() {
                            edit.sent = Some(sent);
                        }
                    }
                    cx.emit(RemotesEvent::Uploaded { name, error });
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    /// 有几个文件正在编辑（界面上写一句"保存即回传"用得上）。
    pub fn editing_count(&self) -> usize {
        self.editing.len()
    }
}

/// 一个文件此刻的样子：(字节数, 修改时间)。
///
/// 少了（文件被编辑器删了再写回来）就当"没这回事"，不传。
fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let info = std::fs::metadata(path).ok()?;
    let modified = info
        .modified()
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|it| it.as_secs());
    Some((info.len(), modified))
}

/// 远程文件落地的地方：系统临时目录底下按连接分开，重名不会互相盖。
fn edit_root() -> PathBuf {
    std::env::temp_dir().join("wfiles-remote")
}

fn edit_dir(id: &str) -> PathBuf {
    edit_root().join(id)
}

fn temp_file(id: &str, name: &str) -> PathBuf {
    edit_dir(id).join(name)
}

/// 下载到临时文件（跑在后台线程上）。打开是主线程的事——
/// 系统调用不能在 worker 线程里做，那是平台层的规矩。
fn download_to(
    client: &dyn remote::Remote,
    sub: &str,
    local: &Path,
) -> Result<(), crate::i18n::Msg> {
    let mut file = std::fs::File::create(local)
        .map_err(|error| crate::i18n::Message::caused("写不了临时文件：{error}", error))?;
    let result = client
        .download(sub, &mut file)
        .map(|_| ())
        .map_err(|error| crate::i18n::Message::caused("下载失败：{error}", error));
    // 先放手这个文件：Windows 上握着的话，半截文件删不掉。
    drop(file);
    result
}

/// 把本地文件传回服务器（跑在后台线程上）。
fn upload_to(client: &dyn remote::Remote, sub: &str, local: &Path) -> Result<(), crate::i18n::Msg> {
    let mut file = std::fs::File::open(local)
        .map_err(|error| crate::i18n::Message::caused("读不了临时文件：{error}", error))?;
    let length = file
        .metadata()
        .map_err(|error| crate::i18n::Message::caused("看不了临时文件：{error}", error))?
        .len();
    client
        .upload(sub, &mut file, length)
        .map_err(|error| crate::i18n::Message::caused("回传失败：{error}", error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::AppContext as _;
    use gpui_kit::TestAppContext;
    use std::time::{Duration, Instant};

    /// 直接把一条连接塞进表里：测试不想碰钥匙串，也不必先"保存"一次。
    fn put_client(remotes: &mut Remotes, id: &str, url: &str) {
        let config = crate::remote::fake::config_with(id, url);
        let client = crate::remote::open(&config, "").expect("该连得上假服务器");
        remotes.clients.insert(id.to_string(), client);
    }

    fn seen(server: &crate::remote::fake::Server) -> Vec<String> {
        server.seen.lock().unwrap().clone()
    }

    fn uploads(server: &crate::remote::fake::Server) -> Vec<String> {
        let mut all: Vec<String> = seen(server)
            .into_iter()
            .filter(|it| it.starts_with("PUT "))
            .collect();
        all.sort();
        all
    }

    /// 等到没有回传在飞。
    ///
    /// 回传跑在真的操作系统线程上，测试调度器看不见它，只能靠"飞着的那趟
    /// 落地了"来判断：**没等稳就断言"没传"是假通过**——那一趟很可能还在路上。
    fn settle_uploads(cx: &mut TestAppContext, remotes: &gpui_kit::Entity<Remotes>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let busy = cx.update(|cx| remotes.read(cx).editing.iter().any(|edit| edit.busy));
            if !busy || Instant::now() > deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[gpui_kit::test]
    fn an_edit_is_uploaded_only_after_it_changes(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let root = tempfile::tempdir().expect("临时目录");
        let local = root.path().join("notes.txt");
        std::fs::write(&local, "hello").unwrap();

        // 真的发一次 PUT：回传最容易错的正是"路径拼成什么样"。
        let server = crate::remote::fake::start();
        let remotes = cx.update(|cx| {
            let remotes = cx.new(|_| Remotes::new());
            remotes.update(cx, |remotes, cx| {
                put_client(remotes, "t1", &server.url);
                remotes.note_edit("t1".to_string(), "folder/notes.txt".to_string(), &local, cx);
            });
            remotes
        });
        assert_eq!(
            cx.update(|cx| remotes.read(cx).editing_count()),
            1,
            "登记之后该算一个在编辑的文件"
        );

        // 刚下载下来那一份本来就和服务器一致，不该白传一趟。
        cx.update(|cx| remotes.update(cx, |remotes, cx| remotes.upload_edits(cx)));
        settle_uploads(cx, &remotes);
        assert!(
            uploads(&server).is_empty(),
            "没改就不该传：{:?}",
            seen(&server)
        );

        // 改了：这一趟必须真的落到服务器上，而且落在对的路径上。
        std::fs::write(&local, "hello, changed").unwrap();
        cx.update(|cx| remotes.update(cx, |remotes, cx| remotes.upload_edits(cx)));
        settle_uploads(cx, &remotes);
        assert_eq!(
            uploads(&server),
            vec!["PUT /dav/folder/notes.txt".to_string()],
            "改一次只该传一趟，且路径拼对：{:?}",
            seen(&server)
        );

        // 传完了还再调一次（监听事件会连着来好几个）：指纹没变就不该再传。
        cx.update(|cx| remotes.update(cx, |remotes, cx| remotes.upload_edits(cx)));
        settle_uploads(cx, &remotes);
        assert_eq!(
            uploads(&server).len(),
            1,
            "指纹一样就不该重复传：{:?}",
            seen(&server)
        );
    }

    #[gpui_kit::test]
    fn forgetting_a_connection_drops_its_edits(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let root = tempfile::tempdir().expect("临时目录");
        let local = root.path().join("notes.txt");
        std::fs::write(&local, "hello").unwrap();

        let server = crate::remote::fake::start();
        let remotes = cx.update(|cx| {
            Settings::update(cx, |settings| {
                settings
                    .remotes
                    .push(crate::remote::fake::config_with("t1", &server.url));
            });
            let remotes = cx.new(|_| Remotes::new());
            remotes.update(cx, |remotes, cx| {
                put_client(remotes, "t1", &server.url);
                remotes.note_edit("t1".to_string(), "folder/notes.txt".to_string(), &local, cx);
            });
            remotes
        });

        cx.update(|cx| remotes.update(cx, |remotes, cx| remotes.forget("t1", cx)));
        // 连接都没了还往它回传，只会弹出一串连不上的错误。
        assert_eq!(
            cx.update(|cx| remotes.read(cx).editing_count()),
            0,
            "删了连接就不该再留着它的编辑"
        );
    }

    #[gpui_kit::test]
    fn a_failed_upload_is_tried_again(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let root = tempfile::tempdir().expect("临时目录");
        let local = root.path().join("notes.txt");
        std::fs::write(&local, "hello").unwrap();

        let remotes = cx.update(|cx| {
            let remotes = cx.new(|_| Remotes::new());
            remotes.update(cx, |remotes, cx| {
                // 端口 9 是 discard 端口：本机立刻拒绝，不会真的发到网络上去。
                put_client(remotes, "t1", "http://127.0.0.1:9/dav/");
                let name =
                    remotes.note_edit("t1".to_string(), "folder/notes.txt".to_string(), &local, cx);
                assert_eq!(name, "notes.txt");
            });
            remotes
        });
        let original = cx.update(|cx| remotes.read(cx).editing[0].sent);

        std::fs::write(&local, "hello, changed").unwrap();
        cx.update(|cx| remotes.update(cx, |remotes, cx| remotes.upload_edits(cx)));
        settle_uploads(cx, &remotes);

        // 传砸了还记账的话，这一份就永远留在本地、再也不回传了。
        assert_eq!(
            cx.update(|cx| remotes.read(cx).editing[0].sent),
            original,
            "失败之后不该记账"
        );
        assert_ne!(fingerprint(&local), original, "文件确实改过了");
    }

    #[test]
    fn the_temp_file_sits_in_its_own_connection_folder() {
        // 按连接分开：两个服务器上同名的 `notes.txt` 不会互相盖。
        assert_eq!(
            temp_file("jg", "notes.txt"),
            edit_root().join("jg").join("notes.txt")
        );
    }

    #[test]
    fn a_file_that_is_gone_has_no_fingerprint() {
        // 编辑器保存时会"删掉再写回"，那一瞬间文件不在；
        // 这时宁可跳过这一趟，也不能把空文件传上去。
        assert_eq!(fingerprint(Path::new("/绝对/不存在/的地方")), None);
    }

    // ---- 离线缓存 ----

    /// 把离线缓存指向一个临时目录。返回守卫，掉了就恢复默认。
    ///
    /// `Store::new()` 每次都问 `Store::root()`，所以得让 `root()` 能被改——
    /// 用进程级的覆盖，而不是给 `Store` 加参数（加参数会让生产代码里
    /// 到处都得传一遍，只为测试方便是不值的）。
    struct OfflineOverride {
        // 字段顺序即析构顺序的逆序：`_lock` 最后声明，
        // 所以它最先被丢掉——覆盖恢复完之后锁才放掉。
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<PathBuf>,
        _dir: tempfile::TempDir,
    }

    impl OfflineOverride {
        fn arm(tag: &str) -> Self {
            // 拿锁再改覆盖：否则两个测试并行时会互相把对方的覆盖顶掉。
            let lock = crate::offline::test_guard();
            let dir = tempfile::tempdir().unwrap();
            let previous = crate::offline::take_root_override();
            crate::offline::set_root_override(Some(dir.path().join(tag)));
            Self {
                _lock: lock,
                previous,
                _dir: dir,
            }
        }
    }

    impl Drop for OfflineOverride {
        fn drop(&mut self) {
            crate::offline::set_root_override(self.previous.clone());
        }
    }

    /// 某个远程目录现在列着什么（按名字）。
    fn names_in(
        cx: &mut TestAppContext,
        remotes: &gpui_kit::Entity<Remotes>,
        id: &str,
        sub: &str,
    ) -> Vec<String> {
        let path = remote::location(id, sub);
        cx.update(|cx| {
            remotes
                .read(cx)
                .entries(&path)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| entry.name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    }

    /// 一直转到这个远程目录这一趟有结果为止（列回来了，或者确认列不回来）。
    ///
    /// 只等 `pending`：`failed` 是**终态**不是"在等"。
    /// 早先把 `failed` 也算进去，结果断网那一段会一直等下去——
    /// `failed` 一旦进去就再也不会出来。
    fn settle_list(cx: &mut TestAppContext, remotes: &gpui_kit::Entity<Remotes>, path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let waiting = cx.update(|cx| remotes.read(cx).pending.contains(path));
            if !waiting {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "这个远程目录一直取不回来：{path:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[gpui_kit::test]
    fn a_listing_is_still_there_after_the_server_stops_answering(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let _offline = OfflineOverride::arm("断网");
        let server = crate::remote::fake::start();
        let remotes = cx.new(|_| Remotes::new());

        // 先在服务器还活着的时候列一次，让缓存落盘。
        cx.update(|cx| {
            remotes.update(cx, |remotes, cx| {
                put_client(remotes, "t1", &server.url);
                assert!(
                    remotes.list("t1", "folder", cx).is_none(),
                    "第一次得去后台问服务器"
                );
            });
        });
        settle_list(cx, &remotes, &remote::location("t1", "folder"));
        let online = names_in(cx, &remotes, "t1", "folder");
        assert!(
            !online.is_empty(),
            "服务器在线时该列到 folder 里的东西：{online:?}"
        );
        // 落盘是 detach 的后台线程，等它把文件写出来。
        let store = crate::offline::Store::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while store.load("t1", "folder").is_none() && Instant::now() < deadline {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }

        // 现在断网，换一个干净的 Remotes（相当于重启应用）。
        server.go_offline();
        let offline_remotes = cx.new(|_| Remotes::new());

        let listed = cx.update(|cx| {
            offline_remotes.update(cx, |remotes, cx| {
                put_client(remotes, "t1", &server.url);
                remotes.list("t1", "folder", cx)
            })
        });
        let offline = listed
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(
            offline, online,
            "服务器答不出来了，那份列表还得原样在——这正是离线缓存的意义"
        );
        // 界面上必须写明这是旧的，否则用户会以为这就是现在的样子。
        let at = cx.update(|cx| {
            offline_remotes
                .read(cx)
                .staleness(&remote::location("t1", "folder"))
        });
        assert!(at.is_some(), "得能告诉界面这份有多旧");

        // 后台那一趟仍然要去问：万一只是临时抖了一下，通了就该换成新的。
        settle_list(cx, &offline_remotes, &remote::location("t1", "folder"));
        assert!(
            cx.update(|cx| offline_remotes
                .read(cx)
                .failed
                .contains(&remote::location("t1", "folder"))),
            "服务器确实没答上来"
        );
        assert_eq!(
            names_in(cx, &offline_remotes, "t1", "folder"),
            online,
            "问失败了也不能把用户正在看的内容抢走"
        );

        // 服务器回来了：刷新一下就该换成新的，"旧的"那层撤掉。
        server.come_online();
        cx.update(|cx| {
            offline_remotes.update(cx, |remotes, cx| remotes.reload("t1", "folder", cx))
        });
        settle_list(cx, &offline_remotes, &remote::location("t1", "folder"));
        assert_eq!(
            cx.update(|cx| offline_remotes
                .read(cx)
                .staleness(&remote::location("t1", "folder"))),
            None,
            "拿到真的了就不该再标成旧的"
        );
    }

    #[gpui_kit::test]
    fn forgetting_a_connection_takes_its_offline_listings_too(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let _offline = OfflineOverride::arm("删连接");
        let store = crate::offline::Store::new();
        let entries = vec![crate::remote::Entry {
            name: "旧文件.txt".to_string(),
            path: "旧文件.txt".to_string(),
            is_dir: false,
            size: 5,
            modified: None,
        }];
        store.save("gone", "x", &entries);
        assert!(store.load("gone", "x").is_some());

        let remotes = cx.new(|_| Remotes::new());
        cx.update(|cx| {
            remotes.update(cx, |remotes, cx| {
                let mut config = crate::remote::fake::config("http://127.0.0.1:9/dav/");
                config.id = "gone".to_string();
                remotes.save(config, "", cx).ok();
                remotes.forget("gone", cx);
            });
        });

        assert!(
            store.load("gone", "x").is_none(),
            "连接都没了，它的目录列表该一起没——留着是垃圾，也可能在下次同 id 时串味"
        );
    }
}
