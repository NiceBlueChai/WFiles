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
    /// 编辑中的远程文件：改了就会自动回传，这里只是让界面知道有这一回事。
    Editing { name: String },
    /// 一次自动回传的结果。
    Uploaded { name: String, error: Option<String> },
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
            cx.emit(RemotesEvent::Failed("没有这条连接".to_string()));
            return;
        };
        let name = sub.rsplit('/').next().unwrap_or("file").to_string();
        let local = temp_file(id, &name);
        if let Err(error) = std::fs::create_dir_all(local.parent().unwrap_or(Path::new("."))) {
            cx.emit(RemotesEvent::Failed(format!("建不了临时目录：{error}")));
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
            cx.emit(RemotesEvent::Failed(format!("起不了后台线程：{error}")));
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
                        cx.emit(RemotesEvent::Failed(format!("打不开：{error}")));
                    }
                    cx.notify();
                    return;
                }
                let name = remotes.note_edit(owner, sub, &opened, cx);
                if let Err(error) = open::that(&opened) {
                    cx.emit(RemotesEvent::Failed(format!("打不开：{error}")));
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
                cx.emit(RemotesEvent::Failed(format!("起不了后台线程：{error}")));
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
fn download_to(client: &dyn remote::Remote, sub: &str, local: &Path) -> Result<(), String> {
    let mut file =
        std::fs::File::create(local).map_err(|error| format!("写不了临时文件：{error}"))?;
    let result = client
        .download(sub, &mut file)
        .map(|_| ())
        .map_err(|error| format!("下载失败：{error}"));
    // 先放手这个文件：Windows 上握着的话，半截文件删不掉。
    drop(file);
    result
}

/// 把本地文件传回服务器（跑在后台线程上）。
fn upload_to(client: &dyn remote::Remote, sub: &str, local: &Path) -> Result<(), String> {
    let mut file =
        std::fs::File::open(local).map_err(|error| format!("读不了临时文件：{error}"))?;
    let length = file
        .metadata()
        .map_err(|error| format!("看不了临时文件：{error}"))?
        .len();
    client
        .upload(sub, &mut file, length)
        .map_err(|error| format!("回传失败：{error}"))
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
}
