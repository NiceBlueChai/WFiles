# NSH 客户端（TSX 界面 + 共享核心）

> 位置：`nsh-poc/`（**不在 cargo workspace 里**，独立 `[workspace]`）。
> 跑法：`cd nsh-poc && ~/.nsh/bin/nsh check | test | run --target macos`。
> （`nsh` 不在 `PATH` 里，直接敲 `nsh` 会 `command not found`——2026-11-07 实测。）
>
> 这不是"另写一个文件管理器"。界面是 TSX，**目录模型、排序过滤、搬运、撤销、
> 特殊文件对象、新建模板、删除语义全在 `wfiles-core`**——与 GUI 版共用同一份代码。
> 两边看到同一个目录，必须一模一样。

## 一、结构

```
App.tsx                 TSX 界面：地址栏、过滤框、列头、列表、状态栏、原生菜单
App.test.tsx            headless 测试（43 条）
src/lib.rs              桥接层：持有 contract::Core，快照 ↔ JSON
Cargo.toml              nsh-core/nsh-base（path 到源码 checkout）+ wfiles-core（features = test-support）
```

桥接层只做三件事，**不许再写一遍业务逻辑**：

1. 持有 `contract::Core`（浏览状态 + 内核的唯一入口）；
2. 把 `Snapshot` 转成 JSON；
3. 把字符串参数转回 core 的类型（`SortKey` / `PathBuf` / 模板 id）。

上一版 PoC 的 `Fs.list` 是自写的 `fs::read_dir`——排序、大小文案、隐藏项、过滤
全得重写一遍，正是这次要消灭的东西。

## 二、已接上的能力

| 能力 | 走 core 的什么 | 状态 |
|---|---|---|
| 列目录 / 进目录 / 上级 / 刷新 | `Command::{Enter, Up, Refresh}` | ✅ |
| 排序（点列头，同列翻向） | `Command::SetSort` → `Sort::toggled` | ✅ |
| 过滤（名字 / `*.rs` / `size:>1M` …） | `Command::SetQuery` → `query::Query` | ✅ |
| 隐藏项开关 | `Command::SetHidden` | ✅ |
| 复制 / 粘贴 | `Command::Copy` | ✅ |
| 剪切（复制后清剪贴板） | 前端记剪贴板，仍走 `Copy` | ✅ |
| 移到回收站（可移动介质自动彻底删） | `Command::Delete { permanent: false }` → `ops::delete_mode_for` | ✅ |
| 彻底删除 | `Command::Delete { permanent: true }` | ✅ |
| 撤销（含撤销新建/改名/删除） | `Command::Undo` → `Kernel` 的唯一撤销栈 | ✅ |
| 重命名 | `Command::Rename` | ✅ |
| 新建文件夹 | `Command::CreateFolder` | ✅ |
| 按模板新建文件（8 种） | `Command::CreateFile { template }` → `templates` | ✅ |
| 建议名字（避免预填名撞车） | `Wfiles.freshFolderName` / `freshFileName` → `ops::fresh_*` | ✅ |
| 外部拖入复制 | `onFileDrop` → `Command::Copy` | ✅ |
| 多选（模式开关 + 全选/清空） | 界面持选中集合，操作时整体交给 `Command::{Copy,Delete}` | ✅ |
| 键盘导航（↑↓ / ⇧↑⇧↓ / Enter / Home·End） | `nsh/keymap`，焦点行经 `List` 的 `ref` 滚进视野 | ✅ |
| **搬运进度行** | `Wfiles.progress()` 轮询 `KernelEvent::Progress`，100 毫秒一次 | ✅ |
| **搬运中止** | `Wfiles.abort()` 设 `ops::Control` 的中止位，**不碰 `CORE` 的锁** | ✅ |
| **重名策略（保留两者 / 替换 / 跳过）** | `Command::SetConflictPolicy` → `Kernel` 里的权威值；按钮只是发选择 | ✅ |
| **逐项问重名** | `Core::begin_conflicts()` + 轮询 `KernelEvent::ConflictHit` → `Wfiles.answerConflict()` | ✅ |
| **搜索（名字 / 通配 / `content:` 内容）** | `Command::Search` + 轮询 `KernelEvent::{SearchBatch, SearchDone}` | ✅ |
| **预览（文本 / 十六进制 / 类型占位）** | `Wfiles.preview(path)` → `contract::Core::preview` → `preview::load` | ✅ |
| **缩略图** | core 的 `Kind::id()` 判"是不是图片"，界面把路径交给原生 `Image` 解码 | ✅ |
| **内容差异（两个文件）** | `Wfiles.diff(l, r, char_level, limit)` → `diff::{load, diff_text, diff_hex}` | ✅ |
| **目录比较（两个目录）** | `Wfiles.compare(l, r, limit)` → `compare::compare` | ✅ |
| **收藏** | `Wfiles.favorites()` / `toggleFavorite()` → `wfiles_core::favorites`（与 GUI 同一份 `settings.json`） | ✅ |
| **远程 SFTP** | `Wfiles.sftpConnect/list/download/disconnect` → `wfiles_core::sftp` | ✅ |
| **磁盘（卷 / 剩多少）** | `Wfiles.volumes()` → `volumes::detected()`（core 的只读查询，`Core::volumes()` 的同一份实现） | ✅ |
| **撤销说清撤什么** | 快照里的 `undo_label` → `contract::Core::undo_label()` → `History::last_describe()` | ✅ |
| 原生菜单 / 右键菜单 | `nsh/desktop` 的 `Menu` | ✅ |
| 10000 行基准 | `Wfiles.benchRows`（假数据，量帧成本） | ✅ |

### 2.1 多选与键盘导航（修饰键只有键盘给得到）

NSH 的 `onPress` **不带事件参数**（`onPress?: () => void`），界面层拿不到鼠标的修饰键状态，
按一次分不清是"替换选中"还是"追加选中"。所以：

- **多选是显式模式**：点「多选」进入，此时点一行 = 切换它的选/不选；
- 「全选」取**过滤后**的当前行（`rows()`），不是磁盘全部——用户看到什么就选什么；
- 代价：多选期间点目录不再进入（它变成"选中"）。没有修饰键就只能二选一，
  这比"按了没反应"好。要进目录先退出多选。
- 选中集合是**界面状态**，不进 core：core 只收"要操作哪些路径"这个参数。
  这与 GUI 版一致（键盘光标 vs 多选拖放的语义本来就不同）。

**键盘能拿到修饰键**：`nsh/keymap` 把按键字符串原样交给 handler，`"shift+down"` 与
`"down"` 分得开。所以 ⇧ 扩选只能绑在键盘上：

| 键 | 干什么 |
|---|---|
| `↑` / `↓` | 移动焦点行；选中替换成那一行（与 Finder 一致，不累积） |
| `⇧↑` / `⇧↓` | 从锚点扩选到新焦点（范围是"锚点→焦点"，往回缩也按锚点算） |
| `Enter` | 目录就进去；文件明说这个客户端打不开 |
| `Home` / `End` | 跳到第一行 / 最后一行 |

两条实现细节，改之前先读：

1. **绑定不能放进 `if (isAvailable)`**。`isAvailable` 在 headless 测试里是 false
   （`open()` 的默认平台是 `ios`），放进去等于测试永远驱动不到这段。
   快捷键本身是跨平台的（手机上没键去触发而已）。
2. **`when: "notEditing"` 不能省**：重命名/新建的输入框有焦点时，方向键要归输入框，
   否则在名字里按一下就换了选中。
3. 焦点行靠 `List` 的 `ref` → `scrollToIndex({ viewPosition: 0.5 })` 滚进视野。
   虚拟列表只有它知道自己量到多高，别自己算。

## 三、操作异步化（2026-11-07）

**问题**：`#[module]` 方法在界面线程跑完才返回。本地小目录几毫秒无所谓，搬几万文件
会整窗卡死。

**解法**：`#[module]` **本来就支持 `async fn`**（生成的 `.d.ts` 里是 `Promise<T>`），
所以每个操作都配了一对同步/异步版本（`copy` / `copyAsync`，`delete` / `deleteAsync` …），
界面统一调异步版。

异步实现不用 `tokio::spawn_blocking`：`nsh-poc` 自己不依赖 tokio（core 那份
tokio 是给 SFTP 客户端用的 `rt-multi-thread`，见 §四.1），也没有现成的 runtime
可挂。改用最朴素的一条：开一个 `std::thread`，在 `async-channel` 的 bounded(1)
上把结果回传。

```rust
async fn on_worker<R, F>(work: F) -> Option<R>
where R: Send + 'static, F: FnOnce(&mut contract::Core) -> R + Send + 'static
{
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = with_core_mut(work);
        let _ = tx.send_blocking(result);
    });
    rx.recv().await.ok()
}
```

这是**过渡方案，不是终点**——上面那句话只对"界面不卡"成立；进度与中止的
真身见 §三.1。

### 3.1 进度与中止：两个锁要分清

搬运期间 `Core` 被 `CORE` 这把 `Mutex` 锁着（工作线程在 `apply` 里搬几个文件）。
这决定了两件事的形状：

- **进度**：core 把 `KernelEvent::Progress` 广播到 `subscribe()` 的接收端，
  事件 channel 和 `CORE` 的锁是两回事，所以界面线程随时读得到。
  NSH 没有"Rust 主动推给 JS"的路子（原生模块只有方法），所以 `Wfiles.progress()`
  是**轮询**：界面每 100 毫秒问一次，顺便 drain 一遍内核事件（状态栏消息攒进
  `PENDING`，下次渲染取走——不会因为轮到进度问就被吃掉）。
- **中止**：界面线程**绝不能**为了叫停去拿 `CORE` 的锁——那是死锁
  （锁被搬运线程占着）。所以 `Core::begin_transfer()` 在开工前把 `ops::Control`
  的副本交出来，桥接层存进独立的 `CONTROL` 静态；`Wfiles.abort()` 只碰它，
  一个 `AtomicU8` 的事，搬运线程在每个数据块之间自己看见。
- **中止位不可逆**，所以每趟搬运换一个新控制手柄（`arm_transfer` 在 `apply` 之前调），
  复用会让第二趟一开始就自己停下。

界面侧的进度行有**最少露脸 400 毫秒**（`MIN_VISIBLE_MS`）：几毫秒就搬完的东西
不该闪一下（视觉上像出错），而且"进度行出现过"需要一个稳定可观察的窗口。

### 3.2 重名策略：按钮是循环，值是 core 的

三档（保留两者 / 替换 / 跳过）都是 `ConflictPolicy` 的枚举值，**权威值存在
`Kernel` 里**——跟链接策略同一套路。NSH 这边：

- 按钮上写着当前档（`重名：保留两者`），按一下换下一档，转一圈回来；
- 每次 `apply()` 都从快照的 `conflict_policy` 字段覆盖本地信号——
  **本地信号只负责显示**，不参与判定。这样 `reset()` 之后界面不会显示一个
  core 里其实不是的档位；
- 桌面菜单里另有三个显式条目（`编辑 → 重名时…`），给不熟悉循环的人；
- 搬运的结论由 core 的 `ops::summarize` 拼（`拷贝完成 0 项，跳过 1 项`），
  界面只负责显示——跳过是唯一"成功但磁盘上什么都没变"的结果，不吭声等于
  静默失败。

**为什么是循环按钮而不是下拉菜单**：`Menu.popup` 是原生菜单，headless 测试点不到，
而"选择有没有真的传到搬运上"恰恰是最该有判据的一段。循环按钮让这条链可测。

### 3.3 逐项问重名：搬运会**停下来等人**

打开「逐项问」之后，同一批里可以既替换又跳过——整批策略做不到这件事。代价是
搬运变成"要等"的函数，所以这条链是三样东西凑起来的：

- **口子**：`arm_transfer` 里 `core.begin_conflicts()`，**在 `apply` 之前**取
  （理由同 `CONTROL`：搬运期间 `Core` 被工作线程独占），句柄存进 `static ANSWERS`。
- **等**：搬运线程每碰到一项重名就广播 `KernelEvent::ConflictHit { path }`，
  然后在那条通道上等答案。这期间它仍然盯着中止位——用户完全可以看完问题再决定不搬了。
- **答**：界面 100 毫秒轮询 `Wfiles.conflictState()`（跟进度共用同一个 ticker——
  搬运停着的时候进度不动，另起一个定时器也解决不了"什么时候该消失"），
  点按钮走 `Wfiles.answerConflict(policy)`，只碰 `ANSWERS`，不碰 `CORE` 的锁。

三个坑：

1. **问题要在桥接层攒着**（`static ASKING`）。轮询有间隔，两轮之间到的那个问题
   要是没人接，搬运就一直停在那儿。
2. **答案在 core 那边是"一问一答、按顺序配对"**：界面只能回答刚收到的那一条，
   不许提前攒答案（攒了就会错位——答的是第二项、落到了第一项上）。
3. **`reset()` 必须把开关也关掉**。`ASK_CONFLICTS` 是进程级静态，这一条测试打开的
   逐项问会跟着污染后面每一条搬运，而它们没人回答——表现是挂着不动，不是报错。

**判据为什么是「完成 1 项 + 跳过 1 项」**：这是整批策略永远出不来的结果。
整批「跳过」= 完成 0 项，整批「替换」/「保留两者」= 完成 2 项。只看
"问句出现过"是挑错层——问是问了、答案没用上也照样绿（core 侧做过这个负验证：
把收到的答案换成兜底策略，`tests/contract.rs` 的逐项那条红、其余全绿）。

### 3.4 搜索：结果在 core 那边是流式的

`Command::Search` 立刻返回（回执是当前清单，没变），命中分批发
`KernelEvent::SearchBatch`、收尾发 `SearchDone`。NSH 这边接法与进度同源但更麻烦：

- **结果由桥接层攒**（`static SEARCH: Mutex<Option<SearchRun>>`）。界面每 100 毫秒
  轮询一次 `Wfiles.searchState()`——轮询有间隔，中间到的批次要是没人接就永远丢了。
- **`done` 到了才准说话**：`SearchDone` 之前是"搜索中…"，之后才报
  「命中 N 条（看过 M 项）」。把零命中当成"搜完了"是一种会骗人的快。
- 状态栏右侧在搜索态改报命中条数（不是「N / M 个项目」）——那里本来就是
  "现在看着多少东西"的位置。
- 取消：`Wfiles.cancelSearch()` 走契约命令，不碰 `CORE` 的锁（搜索的结果是
  事件流推的，`apply` 早返回了，跟搬运那种独占不一样）。
- 「关闭」= 叫停 + 收起搜索行 + 回到目录清单；点一条命中 = 去它所在的目录。

### 3.5 预览：不做 ticker，跟着焦点走

`Wfiles.preview(path)` 是**只读查询**（core 的 `preview::load`）：不进事件流、
不改任何状态、不入撤销栈。所以它不需要进度/搜索那套轮询——焦点一变就同步取一次。

- 开关默认**关**：一块内容区会占掉列表高度，而这是"想看才看"的东西。
- 焦点行的写入收成**一个入口** `setFocus()`。分散写 `focus.set(...)` 的话，
  漏一处就出现"焦点在 b、预览还是 a"——这种错在界面上看不出来，只有判据盯得住。
- 界面自己截断到 6 行，**不用 `numberOfLines`**：那个属性会让文本不进 `texts()`，
  判据就看不见内容（地址栏踩过这个坑）。
- 标题里的语言名（`Some("Rust")`）与二进制那几行的排版（偏移 / 分栏 /
  不可打印字节写成点）全是 core 的纯函数，界面不重写。
- 图片 / PDF / 音视频只显示类型占位：真解码在界面层，而 core **判类型不 open
  文件**——空内容的 `.png` 必须报「图像」而不是「0 行」，这条就是判据 28。

### 3.6 磁盘面板：只读查询，不是命令，也不抢 `CORE` 的锁

`Wfiles.volumes()` 返回 core 的 `volumes::detected()`：三平台扫挂载点、去重、
`statvfs` / `GetDiskFreeSpaceExW` 算容量全在 core，桥接层只把结构翻成 JSON。

- **只读查询**，跟 `preview` 同类：不改当前目录、不进撤销栈、不推事件。
  所以没有 `Command` 变体，也没有 ticker——打开面板时取一次就够了
  （core 那份有三秒 TTL，跟 GUI 侧栏一样）。
- **刻意不走 `CORE` 的锁**：`contract::Core::volumes()` 本身就是转发
  `volumes::detected()`，而搬运期间那把锁被工作线程独占，界面不该为一次
  只读查询卡到搬完。契约那条路留给只认识 `Core` 的前端，两边是同一份实现。
- `capacity_text` 由 core 的 `Capacity::describe()` 拼（`Display` 恒给中文），
  界面只负责画；`null` = 探不出容量，**整行不画**，不许把"不知道"写成"0 可用"。
- 夹具走 core 的 `volumes::set_override`（`test-support` feature），**不是桥接层
  另存一份**：它覆盖的是 `detected()` 的返回值，所以把那行摘掉判据会当场红。
  `Wfiles.reset()` 里同时 `clear_override()`——`CORE` 和这份夹具都是进程级静态。
- 工具栏因此**按用途分成了两行**（过滤+粘贴 / 策略+面板开关）。原来一行要装
  「过滤框 + 7 颗按钮」，加「磁盘」这颗就把「多选」「基准」挤出可见区，
  9 条旧判据一起报 `no pressable view`。固定部分之和明显大于一行宽，
  是结构错了；分行之后 30 条全绿。

### 3.7 撤销按钮说清「撤什么」

`can_undo()` 只回答"能不能"，按钮光写「撤销」等于让用户闭眼按 `Cmd+Z`。
core 的 `History::last_describe()` 早就在给 GUI 的撤销按钮写「撤销 XXX」，
契约补上 `Core::undo_label() -> Option<Msg>` 之后，NSH 也拿得到同一句。

- **走快照，不走事件流**：内核其实会推 `KernelEvent::UndoChanged { available, label }`，
  桥接层也把它攒进了 `PENDING`。但事件要靠 `drain_events` 攒、靠轮询取，
  两轮之间到的那条会丢。`snapshot_json` 里 `undo_label` 跟 `can_undo` 一样
  **当场查 core**，权威且没有时序；界面每次 `apply()` 覆盖 `undoLabel` 信号，
  不自己另存一份。
- 文案由 `HistoryEntry::describe()` 出（`Display` 恒给中文）：新建是
  「新建“草稿”」、复制是「复制 2 项」。界面不许按操作类型自己拼。
- **长标签要截短**：`describe()` 会把两个完整文件名拼进「重命名 X 为 Y」，
  GUI 版为这个专门有 `max_w(260.)` 与一条判据。NSH 不量像素，改成字符截断
  （24 字 + 「…」），**完整描述挂在按钮的 `accessibilityLabel` 上**，读屏和
  判据都读得到（判据用 `app.shows("label:…")`）。
- 一条实测过的取舍：判据里原本还有「长标签没把别的按钮挤出可见区」，
  故意把截断摘掉后 `app.shows("multi-button")` **照样是 true**（地址栏的
  `flex-1` 路径区自己让位，headless 量不到这种挤出）。永远绿的断言已删掉，
  别再加回来；真正抓得住的是「上屏那句必须短」和「全文仍在 a11y」。

### 3.8 缩略图：core 判类型，界面才解码

预览的图片一直是"类型占位"（§3.5）。真图要能看见，缺的是**解码**——那在界面层，
core 只回 `Preview::Opaque(Kind)`。接法：

- **判据用 `Kind::id()`，不用 `Kind::label()`**。`preview::Kind` 新加了 `id()`
  （`"image"` / `"pdf"` / `"text"` …），界面按 `media === "image"` 分支。
  `label()` 是**文案**（「图像」），会改、会本地化，拿它当分支条件是在字符串上
  做类型判断。core 侧有三条单测钉住 id 的稳定与唯一。
- 桥接层在 `Opaque` 分支里多回一个 `media` 字段，值就是 `kind.id()`：
  ```rust
  Preview::Opaque(kind) => json!({
      "kind": "opaque", "label": kind.label(), "media": kind.id(), …
  })
  ```
- 界面拿到 `media === "image"` 就把**焦点行的路径**交给原生 `Image` 的 `src`。
  File 源（绝对路径 / `file://…`）是 `Image` 的四种源之一
  （上游 `~/.nsh-source/source/docs/dev/image-loading-design.md` 的 Sources 表），
  所以不用先读成 base64 再塞进去。
- **别在界面按后缀再判一遍**：那就等于把 `preview.rs` 的后缀表抄了第二份，
  而且和"core 判类型不 open 文件"的约定（TN3150：判类型不许碰内容）打架。

判据是两条**成对**的：同一个流程下，`logo.svg` 上出现 `thumb:logo.svg`
（无障碍名点得出是哪个文件），`notes.txt` 上不出现。只写第一条的话，
"无条件画一个 Image"照样绿——那就变成"界面会画图"而不是"界面听 core 的"。

> 夹具要走 `Wfiles.writeFixture`：`createFile` 的后缀由模板定死（`.txt` / `.md`），
> 造不出 `.svg` / `.bin`。

### 3.9 差异与比较：一颗按钮，两件事

对用户来说都是「这两样有什么不一样」，所以只有一颗「比较」：

- 选中的**两项都是目录** → `Wfiles.compare` → `compare::compare`（比目录树，
  一样的那部分只计数）；
- 否则 → `Wfiles.diff` → `diff::{load, diff_text, diff_hex}`。

两边都要求**正好两项**（`pair-button` 在 `selection().length !== 2` 时是 disabled）。
按钮共用一颗的代价是"我该点哪颗"这个问题消失了，好处是多占的位置也消失了——
多选条上的位置很紧（§3.6 末尾就是被挤出可见区教过一次）。

几个必须落在 core 的点：

- **文本还是字节比，由 core 判**（看开头有没有 NUL），界面不按后缀猜。
- **`limit` 是界面要的**：一次比 8 MB 文本可能几万行，全塞进 JSON 会把界面卡住。
  超出只回前 `limit` 行，并用 `clipped` 说清"还有没画完的"；`truncated` 是
  core 那边的"文件本身只读了开头一段"——两个概念别混。
- **结论句**：`TextStats::summary()`（"改了 1 行 · 新增 1 行"）、
  `HexStats::summary()`（"N 个字节不同，第一处偏移 0x…"）、
  `Stats::summary()`（"只有左 1 项 · 只有右 1 项（另有 1 项一样）"）。
  0 的子句不出现。界面只画。
- **排法也是 core 的**：文本行的标记是 `diff::Kind::label()`（`" "` / `"改"` / `"左"` / `"右"`），
  十六进制的每 16 字节分栏与 ASCII 那一栏是 `diff::hex_line` / `ascii_line`。
- 目录模式的行是 core **整行拼好**的（`"不同  子/文件.txt"`），界面不再拆开重排。

面板**替换**表头与列表（不是叠在上面）：它自带标题（`差异：a.txt ↔ b.txt`）和
一份结论句，留着目录列表反而让人以为是叠加的浮层。

### 3.10 收藏：数据归 core，界面只画

收藏是**用户数据**，GUI 与 NSH 必须同一份——所以它进了 core（`favorites` 模块），
不是界面自己存一份。

- **只动 `favorites` 一个键**：`settings.json` 里还有界面新加的设置项，
  用 `serde_json::Value` 取出整棵树、改一个键、再写回，别用结构体整体覆盖。
  写盘走临时文件 + rename；坏 JSON 当空设置继续（不能让一份坏文件卡死收藏）。
- **名字不存副本**：列表里的 `name` 由 `path.file_name()` 推。用户把文件夹改名，
  收藏栏跟着变；存一份名字就会出现"点进去是好路径、显示的是旧名"。
- 入口只有 `toggle()`（同一个路径点两次就是取消），所以按钮文案由
  **"在不在收藏里"推**（「收藏当前目录」/「取消收藏」），不是另存一个开关。
- 收藏与卷放在**同一个面板**里：两者都是"去哪儿"。判据两头都钉——屏幕上
  `收藏（1）` 数得出来、core 里 `Wfiles.favorites()` 也真有那一条。只钉一头，
  就会出现"界面看着有、其实没存"或者反过来。
- 测试隔离靠 `favorites::set_dir_override()`：**覆盖的是"文件在哪"，不是
  "收藏存了什么"**——所以逻辑本身一点没被替代，`reset()` 里清掉。

### 3.11 远程（SFTP）：单独一套，错误原样说

远程路径不是 `PathBuf`，进不了本地目录模型，所以它在磁盘面板里自成一格，
不混进列表。

- **连上先列一次根目录再存连接**：连上了但列不出来的连接没有用，不如当场报。
  连接存在桥接层的 `REMOTE` 静态里（`Arc<dyn remote::Remote>`），
  重连顶掉上一条。
- **失败原因原样回给界面**：core 的 `Msg` 里带主机、端口和具体错误。
  只写「连不上」三个字排不了障，判据因此特意断言那行里有 `127.0.0.1` 和端口。
- **下载不经过内存中转**：`download` 直接往 `sink` 写。半路失败就把目标文件删掉
  ——半个文件比报错更难查。
- **没连上就如实说没连上**，不返回空列表冒充"这个目录是空的"。

> 这两条判据**只能验失败路径**：跑判据的机器上没有 SFTP 服务器。所以钉的是
> 「错误不许被吞」和「没连上不许假装成功」；成功路径靠 core 自己的单测
> （`sftp.rs` 里有 russh 的 server 夹具）。别为了凑一条"成功路径"去起一个
> 假服务器——那是换到 core 那一层该做的事。

## 四、两个必须知道的坑

### 1. `pkcs1` 版本死锁 → 从 `nsh-engine` 剪掉一条 feature 边

`nsh check` 会为**所有平台**建 host（macos / linux / windows / ios / android / web /
test）。web host → `quickjs-jit-stdlib` 的 **optional** `rsa ^0.10.0-rc.19` 由
`nsh-js` 的 `crypto` feature 打开；而 `wfiles-core` → `russh 0.64` 精确钉
`rsa =0.10.0-rc.18`。真正没有交集的是更底下一层：**`pkcs1`**——`russh` 要
`=0.8.0-rc.4`（连带 `rsa =rc.18`），`rsa rc.19` 要 `pkcs1 ^0.8.0-rc.5`。

查过 crates.io index：`russh` 0.63–0.65 全部钉死 `rc.4`，升级无解；
`quickjs-jit-stdlib` 只有 0.14.x 带 `rsa`，降级也无解。

**解法在上游源码 checkout 里，不在本仓**：`crates/nsh-engine/Cargo.toml` 的
`crypto` feature 从 `["js-core", "nsh-js/crypto"]` 改成 `["js-core"]`——host 侧
用不到 JS 的 crypto 开关。**一条边同时解掉所有平台的 host**，比逐个 crate 去关
默认的 `crypto` 干净得多。于是：

- `wfiles-core` 的 `sftp` feature **保持默认开**，SFTP 能用了；
- `nsh-poc` 不再需要 `default-features = false`（§四.1 的旧版本曾这么绕）。

代价是这条改动会被上游升级覆盖，恢复办法写在 **§6.5**（`nsh-engine` 的注释里
引的就是那一节）。

两个踩过的坑：

- `.nsh/rust/Cargo.lock` 里的 `pkcs1` 可能被锁成正式版 `0.8.0`（有 `rsa rc.19`
  时留下的），此时报错看起来像"无交集"，其实是旧锁在撒谎。**删掉 lock 重新解析**
  就落到 `rc.4`；`cargo update -p pkcs1 --precise` 会把 lock 弄成半更新，越弄越乱。
- **别为了让 SFTP 回来去降级 russh**：0.61–0.65 全系钉 `=rc.18`，只有 ≤0.60 用
  `^rc.10`；跨 4 个 minor 降级会撞 API 变更，代价远大于收益。

### 2. Rust 函数名导出到 TS 会转 camelCase

`set_hidden` → `setHidden`、`bench_rows` → `benchRows`、`file_size` → `fileSize`
（异步版同理：`copy` 不变、`copy_async` → `copyAsync`）。
写 TSX 时照 `.nsh/types/wfiles/native.d.ts` 里的写法，别照 Rust 源码写。
类型检查会报 `TS2551: Property 'set_hidden' does not exist`——这条错就是这个坑。

## 五、验证

```bash
cd nsh-poc
# 0.4.0，源码构建的那份（带 `--target tui`，理由见 §六）：
~/.nsh-source/bin/nsh check   # 首次 40~45s（编 Rust crate），增量 1-3s
~/.nsh-source/bin/nsh test    # 首次 ~1m25s（还要建 headless runner），热 30~50s
~/.nsh-source/bin/nsh run --target macos
~/.nsh-source/bin/nsh run --target tui     # 终端里跑同一份 App.tsx
```

发行版 CLI（`~/.nsh/bin/nsh`，0.2.0）仍在，但**它没有 tui target**，且 0.2.0 与
0.4.0 的接口差异见 §六.4；本仓现在按 0.4.0 写。

`App.test.tsx` 的四十三条判据（前三条不画界面，其余真的驱动界面；表按加入顺序，
第 27、28 条（预览）与第 29、30 条（磁盘）在文件里排在"逐项问"之后、"搜索"之前，
第 31–33 条（撤销标签）排在文件末尾，第 34–43 条（缩略图 / 差异 / 比较 / 收藏 /
远程）紧跟在预览之后、磁盘之前）：

| # | 判据 | 抓什么 |
|---|---|---|
| 1 | 文本里有 `1.5 KB` | 界面真的用了 Rust 的 `Format.fileSize` |
| 2 | 目录优先、快照字段齐全 | 列表来自 core，不是这里重写的读盘 |
| 3 | 过滤让 `shown` 归零、`total` 不变；`setSort("size")` 生效 | 过滤/排序走 core |
| 4 | 点 `hidden-button` 后按钮文案变「隐藏隐藏项」 | 开关真的落到 `Command::SetHidden` |
| 5 | 打字进 `filter` 后状态栏出现 `0 / N` | 输入 → `Command::SetQuery` 通了 |
| 6 | 点列头 → `大小 ↑`，再点 → `大小 ↓` | `Sort::toggled` 的翻向规则 |
| 7 | 新建文件夹：面板 → 落盘 → 列表刷新 | `Command::CreateFolder` + 异步回灌 |
| 8 | 新建文件：按模板后缀预填，落盘后出现在列表 | `Command::CreateFile` → `templates` |
| 9 | 重命名：按钮接上了、弹出的是重命名面板 | 菜单 → `PaneCommand` → 面板（只验证到这一步） |
| 10 | 多选两项 → 复制 → 进目标目录 → 粘贴，`b.txt` 出现且总数是 `2 / 2` | 选中集合**真的**喂给了操作（只复制第一项就红） |
| 11 | 全选覆盖过滤后的全部行，清空归零 | 全选取的是 `rows()` 不是磁盘全部 |
| 12 | 多选期间点目录行只切换选中、不进目录 | 没有修饰键时的取舍被写成了契约 |
| 13 | `↓` 再 `Enter` → 进了第一行的目录（判据是它里面那个独有的文件） | 焦点真的在动（不是画了个高亮） |
| 14 | `↓` → `⇧↓` → `⇧↓` → `⇧↑`，选中数 2 → 3 → 2 | 范围由锚点算，不是累加 |
| 15 | 多选下连按两次 `↓` 仍是「已选 1 项」 | 不按 ⇧ 是替换选中（与 Finder 一致） |
| 16 | 按 `conflict-button` 三下：`重名：保留两者` → `替换` → `跳过` → 回到第一档 | 按钮真的在改 core 的状态（文案来自快照，不是本地瞎猜） |
| 17 | 设「替换」后粘贴 → 目标目录仍是 `1 / 1`、没有 `a 2.txt` | 策略被 `Kernel::transfer` 用上了，不是只改了个标签 |
| 18 | 设「跳过」后粘贴 → 目标不动，**状态栏说「跳过 1 项」** | 跳过不静默：命令成功但磁盘没变化时必须有话说 |
| 19 | 开「逐项问」后粘贴两项：先答「替换」、再答「跳过」，状态栏说「拷贝完成 1 项，跳过 1 项」 | **同一批里两项各答各的**——整批策略永远做不出这个结果（跳过 = 0 完成，替换/保留两者 = 2 完成） |
| 20 | 开关关着：一次都不问，`a 2.txt` 直接出现 | 默认不逐项问，走了整批那条老路（19 的对照） |
| 21 | 搜 `alpha`：子目录里的 `alpha-two.txt` 也上屏，状态栏报「命中 2 条」，桥接层的 `matched` 也是 2 | 输入 → 起搜 → 分批结果上屏这条链（只测"没命中"是挑错层） |
| 22 | 搜 `content:标题`：结果里出现 `1: # 标题` | 内容命中要带行号与那一行原文，不是只说"有这个文件" |
| 23 | 搜完点「关闭」：回到目录清单（`3 / 3`），命中列表消失 | 搜索态是**套在**浏览之上的，关掉要回得去 |
| 24 | 没有搬运时进度行不出现，`progress()` 是 `"null"` | 空闲态干净 |
| 25 | 400 个文件的搬运里进度行**在显示**，结束后自动撤掉 | 行只挂在真的还忙的时候 |
| 26 | 中途 `abort()`：目标目录**真的少搬了**（< 400），消息里说「中止」，进度归零 | 中止位被搬运线程看见，不是摆设 |
| 27 | 开「预览」后点 `main.rs`：屏上出现 `println!("Hello, world!")`，标题是「Rust · 3 行」 | 预览真的读了磁盘上那份文件；语言名与行数来自 core，界面不维护后缀表 |
| 28 | 点一个空内容的 `photo.png`：标题是「图像」 | 按扩展名判类型、**不读内容**（读了会变成「0 行」） |
| 29 | 开「磁盘」：`Wfiles.volumes()` 给两块盘，屏上出现「备份盘」「光盘」和逐字的「可用 120.0 GB / 共 500.0 GB」，容量文案总共只一条 | 卷与那行字都来自 core；探不到容量时**不许编一个 0 出来** |
| 30 | 点第一块盘：屏上出现只有它的挂载点里才有的 `marker.txt` | 点的是导航而不是一个框（`go(path)` 真走了 `Command::Enter`） |
| 31 | 界面新建文件夹后按钮写「撤销：新建“未命名文件夹”」，撤销后收回成「撤销」 | 标签来自 core 快照、跟着栈变；不是界面自己记的一份 |
| 32 | 复制两项后按钮写「撤销：复制 2 项」 | 标签跟着**操作类型**变，不是写死「新建…」 |
| 33 | 60 字的长名字：上屏那句 ≤ 28 字且以「…」结尾，完整描述仍在 `label:` 选择器里 | 长标签既要截短、又不能把信息弄丢（GUI 版的 `max_w(260.)` 同一件事） |
| 34 | 焦点在一张真 SVG 上：出现 `thumb:logo.svg`，类型仍是「图像」 | core 说 `media === "image"` 才画缩略图（`Kind::id()`，不是 `label()`） |
| 35 | 同一个流程换成 `notes.txt`：**没有** `thumb:notes.txt` | 34 的对照——无条件画一个 Image 会让 34 绿、35 红 |
| 36 | 选两个文件（差一行的两份文本）→ 面板标题带两个名字，结论句是「改了 1 行」，那行以 core 的标记「改」开头且两边内容同行 | 界面读的是 `TextStats::summary()` 与 `Kind::label()`，不是自己拼的 |
| 37 | 两个带 NUL 的文件：出现 `0x00000000` 与「个字节不同」 | 文本还是字节比由 core 判；十六进制分栏与偏移都是 core 的排法 |
| 38 | 选两个目录 → 标题是「比较：」，结论句含「只有左」「只有右」「另有 1 项一样」 | 同一个按钮按类型分流到 `compare`，不是一律当文件比 |
| 39 | 只选一项时 `diff-pane` 不出现 | "可比"的门槛没被丢掉（disabled 的 chip 点不到） |
| 40 | 收藏当前目录：屏上 `收藏（1）`+「取消收藏」，core 的 `favorites()` 也真有那一条且 `is_dir` 为真 | 两头都钉：只改信号不改文件 = 界面自己演了一遍 |
| 41 | 从收藏栏点回那个目录（出现只有里面才有的文件），再点一次按钮就取消、core 也空 | 点的是导航；按钮文案由"在不在收藏里"推 |
| 42 | 连 `sftp://127.0.0.1:1/`：屏上那行含 `127.0.0.1` 与端口 | core 的错误原话不许被压成「连不上」三个字 |
| 43 | 没连上时 `sftpList` / `sftpDownload` 都 `ok:false`，且不留下半个文件 | 不返回空列表冒充"空目录"、不写半个文件冒充"下载完了" |

**每条测试开头都要 `Wfiles.reset()`**：`CORE` 是进程级静态，同一个 runner 里
隐藏项开关、过滤词、排序方向都会留给下一条（第 6 条第一次跑就是因为这个红的）；
`volumes::set_override` 那份假卷夹具同样在 `reset()` 里清掉。

第 7、8 条跑在 `Wfiles.tempDir()` 自造的目录里，不碰 home。

**第 9 条为什么只验到"面板出现"**：`app.type` 是追加式输入、`Input` 不可 `press`、
`mod+a` 也清不掉预填内容（想把 `a.txt` 换成 `b.txt`，得到的是 `a.txtb.txt`）。
再往下写就变成"测测试工具"了。提交链由第 7、8 条共用同一个 `submitPrompt` 覆盖，
改名语义本身由 `crates/wfiles-core/tests/contract.rs` 覆盖。

负验证：把 `set_sort` 改成永远按名称排 → 第 3、6 条同时红；
把 `copySelected` 的参数改成 `paths.slice(0, 1)` → 第 10 条红（屏幕上只剩 `1 / 1`）；
把 `shift+down` 的 `true` 改成 `false` → 只第 14 条红（选中数停在 1）；
把桥接层 `abort()` 改成空操作 → 只第 26 条红（400 个全搬过去了）；
把 `MIN_VISIBLE_MS` 改成 0 → 只第 25 条红（进度行一闪而过）；
把结果行右侧那串字改成永远报目录（不报 `行号: 原文`）→ 只第 22 条红；
把 `arm_transfer` 里的 `ask.then(|| core.begin_conflicts())` 改成 `None`（开关接不上 core）
→ 只第 19 条红（问句永远不出现），第 20 条照样绿——它测的正是"关着的时候不问"；
把桥接层 `Opaque` 分支写成 `{"kind":"text","lines":0}`（把"不读内容"的占位当成文本）
→ 只第 28 条红；
把桥接层 `set_conflict_policy` 的参数丢成固定的 `KeepBoth` → 第 16、17、18 条全红
（按钮文案卡在「保留两者」——**文案来自 core 的快照**，所以这一个改动同时证明了
"选择传下去了"和"界面没自己另存一份"）；
把桥接层 `volumes()` 的 `capacity.describe()` 换成 `null` → 只第 29 条红
（`expected null to be "可用 120.0 GB / 共 500.0 GB"`）；
把 `goDisk()` 里的 `go(volume.path)` 摘掉 → 只第 30 条红（屏上还是 `Backup` / `CD` 两行，
没有 `marker.txt`）；
把容量行改成无条件画、探不到就写「可用 0 B / 共 0 B」→ 只第 29 条红（`expected 2 to be 1`）；
把 `UNDO_LABEL_CHARS` 改成 1000（等于不截短）→ 只第 33 条红
（`expected 73 to be less than or equal to 28.0`），31、32 照样绿；
把 `snapshot_json` 的 `undo_label` 换成 `None::<String>`（桥接层不接 core）
→ **正好第 31、32、33 条三条红**、其余全绿（当时共 33 条）；
契约侧把 `Core::undo_label()` 写成永远 `None` → 主仓 `tests/contract.rs`
只第 24 条红、其余 23 条绿；
把桥接层 `Opaque` 分支的 `"media": kind.id()` 改成固定的 `"nope"` → 只第 34 条红
（`label:thumb:logo.svg` 消失；35 条本来就该没有它，照样绿）；
把 `TextStats::summary()` 里的 `"改了 {} 行"` 改成 `"changed {}"` → 只第 36 条红；
把 `openPair` 里"两项都是目录"那一支换成 `Wfiles.diff` → 只第 38 条红
（屏上变成「差异：left ↔ right」+「读不了：Is a directory (os error 21)」，
**这也正是"两个目录不能当文件内容比"的证据**）；
把桥接层 `favorites()` 写成直接 `return "[]"` → 只有第 40 条红（这次用
`--filter 收藏` 只跑这两条，41 条照样绿：`expected 0 to be 1`，而那屏上仍写着
「收藏（1）」——界面吃的是 `toggle` 的返回值，只有 core 那条查询能证明"真的存进去了"）；
把 `sftp_list` 的"还没连上"改回 `{"ok":true,"entries":[]}` → 只第 43 条红
（`expected true to be false`——空列表冒充"这个目录是空的"就现形）。

最后两条（38 与 43）是在**同一趟**里一起破坏的：全量 43 条跑出**正好 2 红 41 绿**，
两条红各自对应自己的破坏点（38 的屏上出现了「差异：」而 43 是 `ok` 变成了 true），
所以这次算两条独立证据。

**第 29 条差点落在错误的一层上**：最初用 `app.shows("disk-capacity:1") === false`
判"探不到就别画那一行"，负验证（无条件画了一个**空文本**的元素）居然全绿——
headless 里空文本元素同样报 `shows: false`，于是"多画了个空行"根本测不到。
改成**数容量文案的条数**（`texts().filter(t => t.startsWith("可用 ")).length === 1`）
之后，同一个破坏点才现形。判据要盯用户看得见的那个后果（多出一行假容量），
不是盯元素在不在树里。

**第 33 条也删过一条假断言**：原本还有「长标签没把别的按钮挤出可见区」
（`app.shows("multi-button")`），实测把截断摘掉、按钮真的变成 73 字时它
**照样是 true**——地址栏的 `flex-1` 路径区自己让位，headless 量不到这种挤出。
永远绿的断言只会稀释判据，删掉；理由写在 §3.7 里，别再加回来。

**第 17 条为什么能只靠落盘判据**：改成「替换」之后，如果策略没传到 `transfer`，
macOS 上会得到 `a.txt` + `a 2.txt` 两行，`1 / 1` 立刻变 `2 / 2`。
core 那一层还有一条更狠的：把 `Kernel::transfer` 里的 `self.conflict_policy`
换回写死的 `KeepBoth`，`tests/contract.rs` 的 `replace_*` 与 `skip_*` 两条同时红，
而作对照的 `the_default_policy_keeps_both_copies` 照样绿——说明红的是策略接线，
不是夹具坏了。

**键盘那三条必须 `open("./App.tsx", { platform: "macos" })`**：`nsh/keymap` 在非桌面
平台上"保留绑定但没有键去触发"，而 headless 的默认平台是 `ios`。忘了写就是三条全红，
且失败信息里什么逻辑错都看不出来（`已选 0 项` 一直不动）。

**进度/中止那几条上的三个坑**（都踩过，改之前先读）：

1. `press` 会等到界面**完全静止**才返回——小搬运在它返回前就跑完了，
   "进度行正在显示"必须用搬得够久的夹具（400 个文件）才观察得到。
2. `waitUntil` 只在界面**有变化**时重算条件。条件写成"Rust 侧某个值变了"
   （比如 `progress() === "null"`）时，搬运结束后没有重绘，它会一直等到超时。
   判据挂在界面状态上（进度行消失），值的断言放在那之后再直接读。
3. 虚拟列表会把旧格子留在树里：`row:dest` 可能匹配到已经不在屏幕上的那一份。
   换目录用 `Wfiles.enter` + 刷新按钮，别点列表行；实在要点就 `>> nth=0`。

**第 21 条为什么不走界面按钮**：`press` 的"等到静止"让按钮发起的搬运必然在断言前
结束，那时候再叫停等于测"搬完之后点中止"。所以直接在桥接层 `copyAsync` 发起
（不 await）、立刻 `abort()`——GUI 按钮和右键菜单与它共用同一个 `withProgress`，
测桥就是测路。

**第 17、18 条为什么要先 `copyOneInto`**：搬运得多慢才能被 `press` 观察到是另一回事，
这两条只要"粘贴那一下按策略走"。`copyOneInto` 把源目录里的一个文件复制到剪贴板
（走界面按钮，`>> nth=0` 绕开虚拟列表留下的旧格子），换到目标目录后再由调用方
按 `paste-button`——策略必须在粘贴那一刻之前设好。

## 六、终端（`--target tui`）

NSH 自己的终端后端：同一份 `App.tsx`，界面对象落到 ratatui 的 Cell 网格上
（默认 1 Cell = 8×16 points，`nsh.json` 的 `tui.pointsPerCell` 可调）。

### 6.1 工具链：必须源码构建，且 app 得按 path 依赖

- `--target tui` 是 **0.4.0 才有**的（0.2.0 与 v0.4.0 的发行版都没有；实测
  `nsh run --target tui` 在 0.2.0 上直接 `invalid value 'tui'`）。
- `--target tui` 要的两块 `nsh-tui-host` / `nsh-platform-tui` **没发布到 crates.io**
  （`index.crates.io/ns/h-/nsh-platform-tui` 404）。走 registry 的话 `nsh check`
  会给 `.nsh/rust/tui-host` 生成 `nsh-tui-host = { version = "=0.4.0" }`，
  解析必然失败。
- 解法是官方给「app 跟着源码一起改」留的入口：`nsh-poc/Cargo.toml` 里把
  `nsh-core` / `nsh-base` 写成**到某个 checkout 的 path 依赖**。
  `nsh-cli` 的 `Nsh::of()` 读 app 的 `cargo metadata --no-deps`，看到 path 就
  让生成的 host 也用 path——app 与 host 天然共用同一份 nsh，不需要 `[patch]`
  （何况 `.nsh/rust/*` 每次 `nsh run` 都会重写，补丁活不过下一次）。
- 本机落点：源码 `~/.nsh-source/source`，二进制 `~/.nsh-source/bin/nsh`
  （`CARGO_TARGET_DIR=/Volumes/Ventoy/nsh-source-target`）。0.2.0 的
  `~/.nsh/bin/nsh` 没动。
  **`~/.nsh-source/bin/nsh` 是个符号链接**（→ `…/release/nsh`），不是拷贝：
  换机器重建时这一步要自己补，漏了它就变成「命令找不到」而不是「版本旧了」，
  两种故障长得完全不一样。
- **2026-11-08 拍板：不追上游。** 这两块没发布到 registry，就一直用 path 依赖——
  **不报 issue、不 fork、不等新版**。理由很实际：path 依赖是官方给「app 跟着源码
  一起改」留的正规入口，不是绕路；报 issue 要等一个不确定的时间，而这条线现在
  已经能跑、能判。所以「path 依赖 + 本地源码 checkout」**是长期形态，不是临时方案**，
  换机器时按本节重建一遍即可（步骤在上面三条里，一条都不省）。
  什么时候重新考虑：真要在 CI 上跑 TUI 判据、或者要把 `nsh-poc` 交给别人构建时——
  那时才会需要一个能 `cargo install` 的上游版本。

### 6.2 `nsh run --target tui` 能跑，`nsh test --target tui` 不能

真终端里跑起来是这样（120×40，夹具目录三个条目）：

```text
 上级    刷新   /var/folders/…/T/…    显示隐藏项    新建文件夹    新建文件    重命名    撤销
 过滤：名字、*.rs、size:>1M、modified:today                                          粘贴
 重名：保留两者    逐项问：关    预览：关    磁盘：关    搜索    多选
 类型   名称 ↑                                          大小          修改时间
 目录   子目录                                          —            1 分钟前
 文件   alpha.txt                                       0 B          1 分钟前
 文件   beta.txt                                        12 B         1 分钟前
                                                                      3 / 3  1.5 KB
```

**`nsh test --target tui` 在带 Rust 原生模块的 app 上一次都跑不起来**：

```text
error bundle failed: Error: could not load module 'wfiles/native'
    (only registered native modules and bundle chunks can be imported at run time)
```

不是我们的接线问题，做了个 A/B：

| 夹具 | `nsh test --target tui` |
|---|---|
| `/tmp` 下一个纯 TSX app（没有 `#[native_module]`） | 1/1 绿 |
| 同一个 app，`nsh run --target tui` | 正常渲染（上面那张屏） |
| `nsh-poc`（有 `#[native_module("wfiles/native")]`） | `could not load module 'wfiles/native'` |

宿主拿到了正确的 UI set（`run` 证明这一点），是**测试通道**那一侧的引擎/config
没带 app 自己的 set。上游修之前，TUI 的判据只能落在「真跑 + 看屏」上。

**2026-11-08 拍板：不追上游，这条路就是长期做法。** 不报 issue、不为了迁就它去
拆 `#[native_module]`（拆了就不是同一个 app 了，判据也就不算数）。因此：

- 以后**加 TUI 判据就往 `scripts/smoke-nsh-tui.sh` 里加**，按 §6.3 那三个坑写，
  别指望某天能搬进 `nsh test --target tui`。这是既定形态，不是权宜之计。
- 判据写法要跟着这个限制走：**只判屏上看得见的东西**（文件上屏、计数、文案），
  不判内部状态；按键只能靠真 pty 发，一次只信「按键前后两屏的差别」。
- 真到了要在 CI 上跑 TUI 的那天，再回来看这一条——那时缺的就不是 issue 了。

### 6.3 判据：真 pty + 回放最后一屏

`scripts/smoke-nsh-tui.sh`（`scripts/tty_capture.py` 是它的眼睛）。三个必须自己
处理的坑，都在 `tty_capture.py` 里写着：

1. `script` 给的 pty 默认 **0×0**，TUI 会画个空屏——必须 `TIOCSWINSZ` 显式设尺寸；
2. 输出是**光标定位的增量帧**，直接按行 `grep` 读到的是历史帧叠在一起（同一个
   文件里 `alpha.txt` 会出现两次，位置还不对）——必须回放成最后一屏再判；
3. 汉字占两格：回放时宽字符的右半格要**不输出**而不是补空格，否则屏上成了
   「目 录」，判据写 `子目录` 永远匹配不上（第一版就是这样白跑了两轮）。

判据 7 条静态 + 3 条键盘，挑的都是「core 的东西真的到了屏上」：

| 判据 | 抓什么 |
|---|---|
| `alpha.txt` / `beta.txt` | 列出的是磁盘上的真文件（core 遍历 → 桥接 → ratatui） |
| `子目录` | 目录也在，且排序里目录优先 |
| `0 B` / `12 B` | 大小是 `core::format_size` 出的（读了文件元数据，不是猜的） |
| `重名：保留两者` | 冲突策略的文案来自 core 快照，界面不另存 |
| `3 / 3` | 底栏的「显示数 / 总数」是 core 的计数 |
| ↓ 然后回车 → `inside.txt` 上屏 | 键盘真的是入口：终端收到键 → `keymap` → `Command::Enter` |
| 同一屏不再有 `alpha.txt` | 真的换了目录，不是把新行叠上去了 |
| 同一屏底栏变 `1 / 1` | 计数跟着目录走，不是缓存的那一份 |

**键盘那条必须先按 `↓`**：焦点初始是空的（没有选中行），直接回车
`openFocused()` 什么都不做——第一版就是这么红的，屏上一动不动，看不出是键没到
还是没焦点。`↓` 之后焦点落在第一行（目录优先 → `子目录`），再回车才进得去。

**`--ready` 等界面真的上屏再起算**，不按绝对秒数发键：首次跑要编 host（20 多秒），
按秒数发的键会打进编译期。`tty_capture.py` 的参数也因此要能写在位置参数**后面**
（早期版本只认写在前面，脚本里写后面整趟只打了 usage，判据红得不明不白）。

**夹具靠 `WFILES_HOME` 指路**，别用 `HOME` 顶替：rustup / cargo 的 toolchain 就在
`$HOME` 底下，一改 nsh 连 `cargo` 都起不来（`rustup could not choose a version of
cargo`）。`Wfiles::home()` 因此多了一个只在设了该变量时才走的分支。

负验证（三条，都做了）：

- 把 `snapshot_json` 的清单倒空（`rows.iter().take(0)`）→ 前 5 条红、骨架断 0 条，
  **`3 / 3` 照样绿**——计数取的是 `snapshot.total_count`，不是 `rows.len()`。
  这条正好说明：**别拿计数器当"列表画出来了"的证据**。
- 把 `conflict_policy` 写死成 `"replace"`（不读 core 的权威值）→ **只**那条红。
- 把 `--send 2:enter` 摘掉（只按 `↓` 不回车）→ **只有那三条键盘判据红**，
  7 条静态照样绿。

**回放器自己撒过一次谎**：ratatui 只重画脏格子，一行文字变短时用 **ECH
（`\x1b[nX`）** 擦尾巴，第一版回放器不认这条，屏上就成了「12 小前时 前」。当时
差点据此去给终端做「窄屏收紧列宽」——重抓一次干净的全量帧才发现 120 列本来排得
整整齐齐，**挤是回放器的假象**。判据工具的 bug 和被测代码的 bug 长得一模一样，
先证明工具再改界面。

### 6.4 0.2.0 → 0.4.0 的接口差异（升级时全部踩过）

| 0.2.0 | 0.4.0 |
|---|---|
| `use nsh_core::ui::prelude::*` | `use nsh_core::prelude::*`（`ui` 这层没了） |
| `import { Input } from "nsh"` | `import { Input } from "nsh/base"` |
| `import { keymap } from "nsh/keymap"` | `import { keymap } from "nsh"`（模块没了，改具名导出） |
| `Window.setTitle("WFiles")` | `Window.get("main")?.set({ title: "WFiles" })` |
| `<Text numberOfLines={1}>` | `style={{ numberOfLines: 1 }}`（挪进样式；另有 `textOverflow`） |
| `onChange={(text) => …}` | `onChange={(e) => e.value}`（载荷从裸字符串变成对象，**不报错但静默出错**） |
| 选择器可以裸写 id（`row:a.txt`） | 必须带前缀：`id:row:a.txt`（`id` / `label` / `role` / `text` / `view`） |

`Input` 在 0.4.0 的**生成类型里缺失**（`nsh/base.d.ts` 只有 `InputHostProps`，没有
组件本体：它在 `nsh-js` 的 `ELEMENTS` 里只是 `Registry::native_values` 的运行时注册，
`declarations.rs` 不吐它）。框架自己的夹具照 `nsh/base` import，但夹具不做类型检查，
所以上游没暴露。`nsh-poc/nsh-input.d.ts` 是补丁，上游补上后删掉即可。

### 6.5 对 nsh 源码做的那一处改动（升级会被覆盖，此处留底）

`nsh-poc` 用的是源码 checkout（§6.1），所以能改上游——**目前只改了两行，都是
feature 边**，为的是把 `rsa` 从 host 的依赖图里彻底摘掉（成因见 §四.1）：

| 文件 | 原来 | 改成 |
|---|---|---|
| `crates/nsh-engine/Cargo.toml` | `crypto = ["js-core", "nsh-js/crypto"]` | `crypto = ["js-core"]` |
| `crates/nsh-testing/Cargo.toml` | `nsh-js = { …, features = ["zlib", "crypto"] }` | `features = ["zlib"]` |

第二条是同一个道理的另一处入口：测试宿主也把 `nsh-js/crypto` 打开了，
不关它照样会把 `rsa ^rc.19` 带进来。

**为什么改 feature 而不改本仓**：冲突发生在 host（`nsh-engine` → `nsh-js` →
`quickjs-jit-stdlib` → `rsa`）这一侧，跟 `wfiles-core` 无关。本仓能做的只有
「关掉自己的 sftp feature」——那是上一版的做法，代价是这个客户端没有 SFTP。
剪掉 host 那条边之后两边都留着，而且**对所有平台一次生效**。

**升级步骤**（`~/.nsh-source/source` 换了版本之后）：

```bash
cd ~/.nsh-source/source
# 1. 两个 feature 重新剪一遍（内容照上表）
grep -n '^crypto' crates/nsh-engine/Cargo.toml
grep -n 'nsh-js = ' crates/nsh-testing/Cargo.toml
# 2. 删 lock 重解析（旧 lock 里 pkcs1 可能锁在正式版 0.8.0，见 §四.1）
rm -f .nsh/rust/Cargo.lock
# 3. 重建二进制（产物在 /Volumes/Ventoy/nsh-source-target）
cargo build --release -p nsh-cli
```

改完 `nsh check` 必须过——不过就是这两处又被上游改回去了。

**这条改动的性质**：它是**本机工具链的裁剪**，不是给上游的补丁。上游真把
`rsa` 那一组依赖理顺了（比如 `quickjs-jit-stdlib` 不再可选依赖 `rsa`），
这两行就该删掉——那时 `crypto` feature 恢复原样即可，本仓一行都不用动。

## 七、还没接的（按重要性）

原来这张表里有三行（缩略图 / 比较 / 差异、收藏侧栏、SFTP），2026-11-08 全部做完，
对应实现与判据见 §3.8–§3.11。剩下的都是**做不了**而不是**没做**的，所以挂在
[`ROADMAP.md`](../ROADMAP.md)「明确不做」里，不在这张表上。

2026-11-08 用户逐条拍板，两项**正式移入 [`ROADMAP.md`](../ROADMAP.md)「明确不做」**，不再挂在候选清单里：
**NSH 内嵌终端**（0.2.0 没有 PTY / 子进程能力）与**拖出到 Finder**（只有 drop destination、没有 drag source）。两条都只限 NSH 客户端——GUI 版的内嵌终端与系统拖出是做完的，别误读成"项目不做"。理由逐条写在 ROADMAP 里，上游有能力了再单独重新提。

## 八、跟 GUI 版的关系

- **共享**：`crates/wfiles-core` 全部（模型、排序、搬运、撤销、特殊对象、模板、i18n 纯函数）。
- **不共享**：界面与调度。GUI 用 GPUI + 自己的后台任务队列；NSH 用 TSX + `on_worker` 线程。
- **同一件事不许有两份实现**。要加能力，先看 `docs/core-capabilities.md` 里
  core 有没有；没有就先加进 core 并补测试，再在两边各自画界面。
