// WFiles × NSH 客户端 —— TSX 界面。
//
// 这一层**只画界面**：目录内容、排序、过滤、搬运、撤销全在 Rust 侧
// （`wfiles_core`，与 GUI 版共用）。这里拿到的是一份快照 JSON，改完再要一份。
//
// 多选为什么是「模式」而不是 ⌘/⇧ 点击：NSH 的 `onPress` **不带事件参数**
// （`onPress?: () => void`），界面层拿不到修饰键状态，按一次分不清是
// 「替换选中」还是「追加选中」。所以改成显式开关：打开多选后，点一行就
// 切换它的选/不选。代价是多选期间点目录不再进入（它变成了"选中"），
// 要先退出多选——这比"按了没反应"好。
//
// 已知限制（当前阶段，见 docs/nsh-client.md）：
//  - 拖出到 Finder / 内嵌终端：2026-11-08 逐条拍板进 ROADMAP「明确不做」
//    （NSH 0.2.0 没有 drag source、没有 PTY / 子进程能力）。上游有了再重新提。
//  - 缩略图 / 比较 / 终端：这些在 core 里有模块，缺的是界面，不是能力。
//    但缩略图要真解码，属于界面层的活（core 判类型绝不 open 文件）。

import { effect, Image, keymap, List, mount, signal, Text, View } from "nsh";
// 0.4.0 起 `Input` 归到 `nsh/base`（`keymap` 反过来进了 `nsh`）。
import { Input } from "nsh/base";
import { isAvailable, Menu, Window } from "nsh/desktop";
import { Format, Wfiles } from "wfiles/native";

type Row = {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
  size_text: string;
  modified: string;
};

type Message = { text?: string; error?: boolean; undo?: boolean; undo_label?: string };

type Snapshot = {
  ok: boolean;
  path?: string;
  rows?: Row[];
  total?: number;
  shown?: number;
  show_hidden?: boolean;
  query?: string;
  sort?: { key: string; descending: boolean };
  can_undo?: boolean;
  /** 下一步会撤掉什么。权威值在 core 的撤销栈里；栈空就是 `null`。 */
  undo_label?: string | null;
  /** 重名策略的权威值在 core 里，这里是它的字符串形式。 */
  conflict_policy?: string;
  /** 重名要不要一项一项问。开关在桥接层，同样只有这一个来源。 */
  ask_conflicts?: boolean;
  messages?: Message[];
  error?: string;
};

const here = signal("");
const rows = signal<Row[]>([]);
const total = signal(0);
const shown = signal(0);
const showHidden = signal(false);
const sortKey = signal("name");
const sortDesc = signal(false);
const query = signal("");
const canUndo = signal(false);
/**
 * 下一步会撤掉什么（`HistoryEntry::describe()` 的中文一句）。
 *
 * **只在显示时用，权威值在 core 的快照里**（每次 `apply` 覆盖）。界面自己
 * 记一份的话，撤销按钮会指着上一步，而磁盘上撤的是另一步——这是「可撤销」
 * 最不能出错的地方。
 */
const undoLabel = signal<string | null>(null);
/** 重名策略：只在**显示**时用，权威值在 core 的快照里（每次 apply 覆盖）。 */
const conflictPolicy = signal("keep_both");
/**
 * 「重名一项一项问」有没有打开。
 *
 * 同样是快照回给界面的（`ask_conflicts`），不是这里自己记的：这个开关决定了
 * 下一趟搬运会不会停下来等人，界面要是另存一份，两边一旦不同步就会出现
 * "看着是逐项问、其实整批一个策略"这种最难查的错。
 */
const askConflicts = signal(false);
/** 当下在等回答的那一项重名。`null` = 没人在等。 */
const asking = signal<{ path: string; name: string; parent: string } | null>(null);
const status = signal("正在打开…");
const isError = signal(false);
/** 选中集合。顺序 = 点击顺序，保留它只为了「焦点项」这类语义还能取到。 */
const selection = signal<string[]>([]);
/** 多选模式。见文件头：没有修饰键可用，只能显式开关。 */
const multi = signal(false);
/**
 * 键盘焦点行（路径）。方向键移动它，⇧+方向键从 `anchor` 扩到它。
 *
 * 屏幕点击不带修饰键，但**键盘带**：`nsh/keymap` 把按键字符串原样给 handler，
 * `"shift+down"` 和 `"down"` 分得开。所以 ⇧ 扩选只能走键盘——这是 NSH 的
 * 现状，不是偷懒。
 */
const focus = signal<string | null>(null);
/** 范围选择的锚点（⇧ 从哪儿开始扩）。 */
const anchor = signal<string | null>(null);
/** `List` 的句柄：虚拟列表只有它知道哪一行在视野里。 */
type ListHandle = {
  scrollToIndex(options: { index: number; animated?: boolean; viewPosition?: number }): void;
};
const listNode = signal<ListHandle | null>(null);
const bench = signal(false);
const clipboard = signal<{ paths: string[]; cut: boolean } | null>(null);
const dragging = signal(false);

/** 搬运进度。`null` = 没有搬运在跑（进度行不出现）。 */
type Transfer = {
  label: string;
  done: number;
  total: number;
  bytes_done: number;
  bytes_total: number;
  bytes_text: string;
  bytes_total_text: string;
};
const transfer = signal<Transfer | null>(null);
/** 轮询定时器 id。没有搬运时为 `null`。 */
let ticker: number | null = null;
/** 收表定时器 id（"最少露脸时间"用）。 */
let hider: number | null = null;
/** 这一趟是什么时候开的，配合 `MIN_VISIBLE_MS` 算最少露脸时间。 */
let startedAt = 0;
/**
 * 进度行至少显示这么久。
 *
 * 两个理由：几毫秒就搬完的东西不该闪一下（视觉上像出错）；
 * 而且"进度行出现过"需要一个稳定可观察的窗口，否则测试只能碰运气。
 */
const MIN_VISIBLE_MS = 400;

/** 正在输入的那个操作。`null` = 没有面板。 */
type Pending =
  | { kind: "rename"; path: string; label: string }
  | { kind: "folder"; parent: string; label: string }
  | { kind: "file"; parent: string; template: string; label: string };

const pending = signal<Pending | null>(null);
const promptValue = signal("");

// ---- 搬运进度 ----
//
// NSH 的原生模块只有方法、没有"Rust 主动推给 JS"的路子，所以进度只能轮询。
// 100 毫秒一次：进度条本来就不是逐帧动画，人眼看不出差别；每次轮询只是
// Rust 侧一次 `try_recv` 的 drain，代价可以忽略。
//
// 只在数字真的变了才写信号：每 100 毫秒无条件 notify 会把整张列表跟着重画，
// 那才是真花钱的地方。

function pollProgress() {
  // 同一趟轮询顺手看有没有重名在等人回答：搬运会停在那一项上，
  // 进度不动，光看进度的话这一行要等到搬运结束才可能出现。
  pollConflict();
  const raw = Wfiles.progress();
  let next: Transfer | null = null;
  if (raw !== "null") {
    try {
      next = JSON.parse(raw) as Transfer;
    } catch {
      next = null;
    }
  }
  const before = transfer();
  if (next === null) {
    // 活干完了。但整行不能立刻撤：至少让用户看清它确实跑过。
    stopTicker();
    const left = startedAt + MIN_VISIBLE_MS - Date.now();
    if (left > 0) {
      if (hider === null) {
        hider = setTimeout(() => {
          hider = null;
          transfer.set(null);
        }, left);
      }
      return;
    }
    transfer.set(null);
    return;
  }
  const changed =
    before === null ||
    next.done !== before.done ||
    next.bytes_done !== before.bytes_done ||
    next.label !== before.label;
  if (changed) transfer.set(next);
}

function startTicker() {
  if (ticker !== null) return;
  ticker = setInterval(pollProgress, 100);
}

function stopTicker() {
  if (ticker === null) return;
  clearInterval(ticker);
  ticker = null;
}

/** 跑一次可能很久的搬运：开表 → 等 → 收表。`finally` 保证异常也不留定时器。 */
async function withProgress(run: () => Promise<string>): Promise<string> {
  // 上一趟的收表定时器可能还没响，先取消：新搬运一开始就得重新计时。
  if (hider !== null) {
    clearTimeout(hider);
    hider = null;
  }
  startedAt = Date.now();
  // 先摆一行占位再开跑：大目录要过一会儿才有第一条进度，中间这段时间
  // 界面不能看起来没反应。几毫秒的搬运靠 `MIN_VISIBLE_MS` 兜住，不会一闪而过。
  transfer.set({
    label: "准备搬运…",
    done: 0,
    total: 0,
    bytes_done: 0,
    bytes_total: 0,
    bytes_text: "0 B",
    bytes_total_text: "0 B",
  });
  startTicker();
  try {
    return await run();
  } finally {
    stopTicker();
    // 这里不直接撤行：交给 `pollProgress` 按最少露脸时间收尾。
    pollProgress();
  }
}

function abortTransfer() {
  Wfiles.abort();
  // 立刻说一句：中止位要等搬运线程走到下一个数据块才生效，
  // 这中间界面不能看起来像没反应。
  status.set("正在中止…");
  isError.set(false);
  // 中止也把在等回答的那一行撤掉：搬运已经知道要停了，不会再用这个答案。
  asking.set(null);
}

// ---- 逐项问重名 ----
//
// core 那边搬运会**停**在重名的那一项上等人回答（`KernelEvent::ConflictHit`
// 之后 apply 一直阻塞）。界面这边只能轮询：跟进度共用同一个 100 毫秒 ticker，
// 因为搬运停着的时候进度不动，另起一个定时器也解决不了"什么时候该消失"。

type Asked = { path: string; name: string; parent: string };

function pollConflict() {
  let next: Asked | null = null;
  try {
    const raw = Wfiles.conflictState();
    next = raw === "null" ? null : (JSON.parse(raw) as Asked);
  } catch {
    next = null;
  }
  // 只在换了问题才写信号：每 100 毫秒无条件 set 会把整个列表跟着重画。
  if (asking()?.path !== next?.path) asking.set(next);
}

function toggleAskConflicts() {
  apply(Wfiles.setAskConflicts(!askConflicts()));
}

// ---- 预览 ----
//
// 预览是 core 的**只读查询**（`Wfiles.preview(path)` → `preview::load`），
// 不进事件流、不改任何状态，所以没有 ticker：跟着焦点走就行。
//
// 默认**关**：一块内容区会占掉列表高度，而这是"想看才看"的东西。
// 打开之后焦点行一变就换内容（见 `setFocus`）。

type PreviewData = {
  kind: "directory" | "opaque" | "text" | "hex" | "unavailable";
  text?: string;
  language?: string | null;
  lines?: number;
  truncated?: boolean;
  rows?: string[];
  total_text?: string;
  label?: string;
  reason?: string;
  /** 机器可读的媒体类型（`image` / `pdf` / …），来自 core 的 `Kind::id()`。 */
  media?: string;
};

const previewOn = signal(false);
const previewData = signal<PreviewData | null>(null);

/** 预览区最多显示几行。渲染层不量高度，界面自己截断最稳。 */
const PREVIEW_LINES = 6;

function refreshPreview() {
  if (!previewOn()) {
    previewData.set(null);
    return;
  }
  const path = focus();
  if (path === null) {
    previewData.set(null);
    return;
  }
  try {
    previewData.set(JSON.parse(Wfiles.preview(path)) as PreviewData);
  } catch {
    previewData.set(null);
  }
}

function togglePreview() {
  previewOn.set(!previewOn());
  refreshPreview();
}

/**
 * 焦点行的**唯一写入口**：顺手把预览换过去。
 *
 * 分散着写 `focus.set(...)` 的话，漏一处就出现"焦点在 b、预览还是 a"——
 * 这种错在界面上看不出来，只有判据盯得住，所以入口收成一个。
 */
function setFocus(path: string | null) {
  focus.set(path);
  refreshPreview();
}

/** 预览标题：一眼看出这是什么、有多大、有没有被截断。 */
function previewTitle(): string {
  const data = previewData();
  if (data === null) return "没有选中项";
  switch (data.kind) {
    case "directory":
      return "文件夹";
    case "opaque":
      return data.label ?? "文件";
    case "unavailable":
      return `无法预览：${data.reason ?? ""}`;
    case "hex":
      return `二进制（共 ${data.total_text ?? ""}）`;
    case "text": {
      const language = data.language ? `${data.language} · ` : "";
      const cut = data.truncated ? "（已截断）" : "";
      return `${language}${data.lines ?? 0} 行${cut}`;
    }
  }
}

/** 预览正文：按行切好再画，每行一条 `Text`（不用 `numberOfLines`，那会让测试读不到）。 */
function previewRows(): { key: string; text: string }[] {
  const data = previewData();
  if (data === null) return [];
  let lines: string[] = [];
  if (data.kind === "text") lines = (data.text ?? "").split("\n");
  else if (data.kind === "hex") lines = data.rows ?? [];
  else if (data.kind === "opaque") lines = ["（内容交给解码器显示）"];
  else if (data.kind === "directory") lines = ["（目录内容看列表）"];
  return lines
    .slice(0, PREVIEW_LINES)
    .map((text, index) => ({ key: `${index}`, text: text === "" ? " " : text }));
}

// ---- 内容差异（两个文件比一比）----
//
// 与预览同类：core 的**只读查询**（`Wfiles.diff` → `diff::load` + `diff_text` /
// `diff_hex`），不碰目录、不进撤销栈、不推事件。用文本还是字节比由 core 按
// "开头有没有 NUL" 判，界面不按后缀猜；结论句也是 core 的 `TextStats::summary()`。

type DiffRowText = {
  /** `dir` 模式下 core 已经把整行拼好了（标记 + 相对路径），界面不再拆开。 */
  text?: string;
  mark: string;
  left_line: number | null;
  right_line: number | null;
  left: string;
  right: string;
};

type DiffRowHex = {
  offset: number;
  left: string;
  right: string;
  ascii_left: string;
  ascii_right: string;
};

type DiffData = {
  ok: boolean;
  error?: string;
  mode?: "text" | "hex" | "dir";
  truncated?: boolean;
  clipped?: boolean;
  summary?: string;
  rows?: (DiffRowText & DiffRowHex)[];
};

const diffOn = signal(false);
const diffData = signal<DiffData | null>(null);
/** 比的是哪两个文件：标题要写出来，否则用户不知道看的是谁跟谁。 */
const diffPair = signal<{ left: string; right: string } | null>(null);

/** 面板一次最多画多少行：几万行全塞进信号会把界面卡住。 */
const PAIR_LIMIT = 400;

function baseOf(path: string): string {
  const cut = path.lastIndexOf("/");
  return cut >= 0 ? path.slice(cut + 1) : path;
}

/**
 * 比一比：**选两个目录就比目录树，否则比两个文件的内容**。
 *
 * 一个入口两件事，因为对用户来说都是"这两样有什么不一样"；分成两颗按钮
 * 既要多占一个位置，还得让用户先判断"我该点哪颗"——而他手里已经选好了。
 */
function openPair() {
  const picked = selection();
  if (picked.length !== 2) {
    status.set("先在多选里挑两项（正好两项）再比");
    isError.set(true);
    return;
  }
  const [left, right] = picked;
  const bothDirs = picked.every(
    (path) => rows().find((row) => row.path === path)?.is_dir === true,
  );
  diffPair.set({ left, right });
  try {
    diffData.set(
      bothDirs
        ? (JSON.parse(Wfiles.compare(left, right, PAIR_LIMIT)) as DiffData)
        : // 字符级高亮开着：改了一两个字的行，光看"改了"两字等于没说。
          (JSON.parse(Wfiles.diff(left, right, true, PAIR_LIMIT)) as DiffData),
    );
  } catch {
    diffData.set({ ok: false, error: "比不了：界面拿到的不是 JSON" });
  }
  diffOn.set(true);
}

function closeDiff() {
  diffOn.set(false);
  diffData.set(null);
  diffPair.set(null);
}

/** 一行差异怎么画：文本比是「标记 左 右」，字节比是「偏移 左 右 ASCII」。 */
function diffLine(row: DiffRowHex & DiffRowText): string {
  // 目录比较那一趟，core 给的已经是成品行（`Status::label()` + 相对路径）。
  if (row.text !== undefined) return row.text;
  if (diffData()?.mode === "hex") {
    return `0x${row.offset.toString(16).padStart(8, "0")}  ${row.left.padEnd(47)} │ ${row.right.padEnd(47)}  ${row.ascii_left} ${row.ascii_right}`;
  }
  const mark = row.mark === " " ? " " : row.mark;
  const leftLine = row.left_line === null ? "   " : String(row.left_line).padStart(3);
  const rightLine = row.right_line === null ? "   " : String(row.right_line).padStart(3);
  return `${mark} ${leftLine} ${row.left}   │ ${rightLine} ${row.right}`;
}

/** 差异行转成 List 要的形状（每行一个 key，虚拟列表靠它复用格子）。 */
function diffLines(): { key: string; text: string }[] {
  return (diffData()?.rows ?? []).map((row, index) => ({
    key: String(index),
    text: diffLine(row),
  }));
}

/** 路径最后一段。缩略图的无障碍名里要认得出是哪个文件（判据靠它定位）。 */
function baseName(path: string): string {
  const cut = path.lastIndexOf("/");
  return cut >= 0 ? path.slice(cut + 1) : path;
}

function answerConflict(policy: string) {
  Wfiles.answerConflict(policy);
  // 立刻撤掉这一行：答案要等搬运线程下一轮轮询（20 毫秒）才发现，
  // 这中间界面不能看起来像没反应。
  asking.set(null);
}

// ---- 磁盘（卷 + 容量）----
//
// `Wfiles.volumes()` 是 core 的**只读查询**（跟 preview 同类）：三平台扫挂载点、
// 去重、算容量全在 core，这里只把那份 JSON 画出来。
//
// 默认**关**：卷列表是"想换盘才看"的东西，常态占着一块高度没意义。
// 每次打开都重取一次（core 那份有三秒缓存），所以插上 U 盘再开面板就看得见。
// 面板开着时不轮询——热插拔本来就没有跨平台通知，GUI 版也是靠三秒 TTL。

type Volume = {
  name: string;
  path: string;
  /** `null` = 这块盘探不出容量，界面整行不画，不许编一个 0 出来。 */
  capacity_text: string | null;
};

// ---- 收藏（数据归 core，与 GUI 读写同一份 settings.json）----
//
// 不另存一份：界面上"收藏空了"就真的是空了。名字由 core 从路径推，
// 文件夹改名之后这里跟着变。

type FavItem = { path: string; name: string; is_dir: boolean };

const favorites = signal<FavItem[]>([]);

function refreshFavorites() {
  try {
    favorites.set(JSON.parse(Wfiles.favorites()) as FavItem[]);
  } catch {
    favorites.set([]);
  }
}

/** 当前目录是不是已经收藏了（按钮文案要随它变）。 */
function favHere(): boolean {
  const target = here();
  return target !== "" && favorites().some((item) => item.path === target);
}

/** 收藏 / 取消收藏**当前目录**。收藏栏放的是"去哪儿"，不是"看哪个文件"。 */
function toggleFavoriteHere() {
  const target = here();
  if (target === "") return;
  const already = favHere();
  const name = baseOf(target) || target;
  try {
    const response = JSON.parse(Wfiles.toggleFavorite(target, true)) as {
      ok: boolean;
      items?: FavItem[];
      error?: string;
    };
    if (!response.ok) {
      status.set(response.error ?? "收藏没写进去");
      isError.set(true);
      return;
    }
    // 列表直接用 core 回的那份，不在这里自己增删——两条真相迟早会分叉。
    favorites.set(response.items ?? []);
    status.set(already ? `已取消收藏 ${name}` : `已收藏 ${name}`);
    isError.set(false);
  } catch {
    status.set("收藏没写进去：界面拿到的不是 JSON");
    isError.set(true);
  }
}

/** 点收藏项：目录就进去（文件留给系统打开，这个客户端不做"打开文件"）。 */
function goFavorite(item: FavItem) {
  if (!item.is_dir) {
    status.set("收藏的是一个文件：这个客户端只进目录");
    isError.set(true);
    return;
  }
  go(item.path);
}

// ---- 远程（SFTP）----
//
// 远程**不进本地目录模型**：它的路径是相对连接根的 `/`-字符串，不是 `PathBuf`。
// 所以这里单独一段列表，不硬塞进 `rows`——塞进去就要为远程给导航、撤销、
// 监听各分一次叉，而它们对远程都不成立。

type RemoteEntry = { name: string; path: string; is_dir: boolean; size_text: string };

const remoteUrl = signal("");
const remoteUser = signal("");
const remotePassword = signal("");
const remoteError = signal<string | null>(null);
const remoteEntries = signal<RemoteEntry[]>([]);

function connectRemote() {
  remoteError.set(null);
  try {
    const response = JSON.parse(
      Wfiles.sftpConnect(remoteUrl(), remoteUser(), remotePassword()),
    ) as { ok: boolean; entries?: RemoteEntry[]; error?: string };
    if (!response.ok) {
      // 原话上屏：core 的错里带着主机、端口和具体原因，"连不上"三个字排不了障。
      remoteError.set(response.error ?? "连不上，原因不明");
      remoteEntries.set([]);
      return;
    }
    remoteEntries.set(response.entries ?? []);
    status.set(`已连接：根目录 ${(response.entries ?? []).length} 项`);
    isError.set(false);
  } catch {
    remoteError.set("连不上：界面拿到的不是 JSON");
  }
}

/** 远程文件下到当前目录。目录先不支持（要递归，另有进度与冲突策略）。 */
function downloadRemote(entry: RemoteEntry) {
  if (entry.is_dir) {
    status.set("远程目录还不支持整个下载");
    isError.set(true);
    return;
  }
  const dest = `${here()}/${entry.name}`;
  const response = JSON.parse(Wfiles.sftpDownload(entry.path, dest)) as {
    ok: boolean;
    bytes?: number;
    error?: string;
  };
  if (!response.ok) {
    status.set(response.error ?? "下载失败");
    isError.set(true);
    return;
  }
  status.set(`已下载 ${entry.name}（${Format.fileSize(response.bytes ?? 0)}）`);
  isError.set(false);
  refresh();
}

const disksOn = signal(false);
const disks = signal<Volume[]>([]);

function refreshDisks() {
  if (!disksOn()) {
    disks.set([]);
    return;
  }
  try {
    disks.set(JSON.parse(Wfiles.volumes()) as Volume[]);
  } catch {
    disks.set([]);
  }
}

function toggleDisks() {
  refreshFavorites();
  disksOn.set(!disksOn());
  refreshDisks();
}

function goDisk(volume: Volume) {
  // 走的是跟列表行同一个 `go`：选中、焦点、多选状态一起归零，
  // 不然换了盘之后还高亮着上一块盘里的某一行。
  go(volume.path);
  disksOn.set(false);
  refreshDisks();
}

// ---- 搜索 ----
//
// 结果在 core 那边是**流式**来的（`SearchBatch` 一批批到），而 NSH 的原生模块
// 只有方法、没有"Rust 主动推给 JS"的路子，所以跟进度同一套路：100 毫秒轮询
// 一次 `Wfiles.searchState()`。
//
// 两条讲究：
//  - 只在真的变了才写信号。每 100 毫秒无条件 `set` 会把整张结果列表跟着重画。
//  - 攒结果的是桥接层（`SEARCH` 静态），不是这里：轮询有间隔，中间到的批次
//    要是没人接就永远丢了。

type Hit = {
  name: string;
  path: string;
  parent: string;
  is_dir: boolean;
  size_text: string;
  line: { number: number; text: string } | null;
};

type SearchState = {
  done: boolean;
  hits: Hit[];
  visited: number;
  matched: number;
  truncated: boolean;
  cancelled: boolean;
};

const searchOpen = signal(false);
const searchQuery = signal("");
const searchHits = signal<Hit[]>([]);
/** `SearchDone` 到了没有。没到之前"零命中"不许说成"搜完了"。 */
const searchDone = signal(true);

let searchTicker: number | null = null;

function startSearchTicker() {
  if (searchTicker !== null) return;
  searchTicker = setInterval(pollSearch, 100);
}

function stopSearchTicker() {
  if (searchTicker === null) return;
  clearInterval(searchTicker);
  searchTicker = null;
}

/** 收尾那句：命中几条、看过多少项；撞上限或被打断也要说清。 */
function searchSummary(state: SearchState): string {
  if (state.cancelled) return `已取消：已命中 ${state.matched} 条`;
  if (state.truncated) return `命中到上限了：${state.matched} 条（先看这些）`;
  return `命中 ${state.matched} 条（看过 ${state.visited} 项）`;
}

function pollSearch() {
  let state: SearchState;
  try {
    state = JSON.parse(Wfiles.searchState()) as SearchState;
  } catch {
    stopSearchTicker();
    return;
  }
  const wasDone = searchDone();
  if (state.hits.length !== searchHits().length) searchHits.set(state.hits);
  searchDone.set(state.done);
  if (!state.done) return;
  stopSearchTicker();
  // 只在"刚开始变成结束"那一下说话，别每次轮询都盖掉状态栏里的别的话。
  if (!wasDone) {
    status.set(searchSummary(state));
    isError.set(false);
  }
}

function openSearch() {
  searchOpen.set(true);
  searchHits.set([]);
  searchDone.set(true);
}

function closeSearch() {
  stopSearchTicker();
  Wfiles.cancelSearch();
  searchOpen.set(false);
  searchHits.set([]);
  searchDone.set(true);
}

function runSearch() {
  const query = searchQuery().trim();
  if (!query) {
    status.set("先输入搜索词");
    isError.set(false);
    return;
  }
  const root = here();
  if (!root) return;
  searchHits.set([]);
  searchDone.set(false);
  status.set("搜索中…");
  isError.set(false);
  apply(Wfiles.search(root, query));
  startSearchTicker();
  // 小目录可能在第一次轮询之前就搜完了，先当场问一次，省一帧空白。
  pollSearch();
}

function cancelRunningSearch() {
  Wfiles.cancelSearch();
  status.set("正在取消…");
  isError.set(false);
}

/** 结果右边那串字：内容命中就报「行号: 那一行」，否则报它所在的目录。 */
function hitDetail(hit: Hit): string {
  if (hit.line) return `${hit.line.number}: ${hit.line.text}`;
  return hit.parent;
}

/** 点一条命中：文件就去它所在的目录，目录就直接进去。 */
function openHit(hit: Hit) {
  const target = hit.is_dir ? hit.path : hit.parent;
  closeSearch();
  go(target);
}

/** 进度行右边那串字。字节数记不出来时按条目数显示。 */
function progressText(): string {
  const now = transfer();
  if (!now) return "";
  if (now.bytes_total > 0) {
    const percent = Math.min(100, Math.round((now.bytes_done / now.bytes_total) * 100));
    return `${now.bytes_text} / ${now.bytes_total_text}（${percent}%）`;
  }
  // 占位阶段总数还是 0（Rust 侧还没数完），别显示成「0 / 0 项」。
  return now.total > 0 ? `${now.done} / ${now.total} 项` : "";
}

/// 把一次后端调用的结果铺到界面上。后端的 JSON 是唯一事实来源——
/// 界面不自己算排序、不自己数条数，否则「列表和状态栏对不上」这类问题
/// 会在两个前端之间反复出现。
function apply(raw: string): boolean {
  let snap: Snapshot;
  try {
    snap = JSON.parse(raw) as Snapshot;
  } catch {
    status.set("后端返回的不是 JSON");
    isError.set(true);
    return false;
  }
  if (!snap.ok) {
    status.set(snap.error ?? "操作失败");
    isError.set(true);
    return false;
  }
  here.set(snap.path ?? "");
  rows.set(snap.rows ?? []);
  total.set(snap.total ?? 0);
  shown.set(snap.shown ?? 0);
  showHidden.set(snap.show_hidden ?? false);
  query.set(snap.query ?? "");
  sortKey.set(snap.sort?.key ?? "name");
  sortDesc.set(snap.sort?.descending ?? false);
  canUndo.set(snap.can_undo ?? false);
  undoLabel.set(snap.undo_label ?? null);
  conflictPolicy.set(snap.conflict_policy ?? "keep_both");
  askConflicts.set(snap.ask_conflicts ?? false);
  isError.set(false);

  // 刷新之后选中项可能已经不在了（删掉、改名、换了目录）。
  // 不剪枝的话「已选 3 项」会指着三个不存在的东西。
  const alive = new Set((snap.rows ?? []).map((row) => row.path));
  const kept = selection().filter((path) => alive.has(path));
  if (kept.length !== selection().length) selection.set(kept);
  // 焦点行同理：换目录后它就不在这张列表里了，留着会让方向键从奇怪的位置起跳。
  const spot = focus();
  if (spot !== null && !alive.has(spot)) {
    setFocus(null);
    anchor.set(null);
  }

  let spoke = false;
  for (const message of snap.messages ?? []) {
    if (message.undo !== undefined) {
      canUndo.set(message.undo);
    } else if (message.text) {
      status.set(message.text);
      isError.set(!!message.error);
      spoke = true;
    }
  }
  if (!spoke && !bench()) {
    // 什么都不说：右边那栏已经在显示「N / M 个项目」了，
    // 左边再重复一遍只是噪音。
    status.set("");
  }
  return true;
}

function go(path: string) {
  bench.set(false);
  // 换目录：选中与焦点都归零。不归零的话新目录里会莫名其妙高亮一行。
  selection.set([]);
  setFocus(null);
  anchor.set(null);
  multi.set(false);
  apply(Wfiles.enter(path));
}

function up() {
  bench.set(false);
  selection.set([]);
  setFocus(null);
  anchor.set(null);
  multi.set(false);
  apply(Wfiles.up());
}

function refresh() {
  if (bench()) return;
  apply(Wfiles.refresh());
}

function toggleHidden() {
  apply(Wfiles.setHidden(!showHidden()));
}

function sortBy(key: string) {
  apply(Wfiles.setSort(key));
}

// ---- 重名策略 ----
//
// 三档：保留两者（默认）/ 替换 / 跳过。**语义在 core 里**，这里只是把选择
// 发下去——搬运时内核按它走，界面不参与判重。
//
// 为什么用「按一下换一档」而不是下拉菜单：NSH 的 `Menu.popup` 是原生菜单，
// headless 测试点不到，而这条链恰恰是最需要判据的一段（策略有没有真的
// 传到搬运上）。按钮上写着当前档位，按一下换下一档，测试与人都看得见。
// 桌面菜单里另有三个显式条目（`编辑 → 重名时…`），那里是给不熟悉循环的人。

const CONFLICT_LABELS: Record<string, string> = {
  keep_both: "保留两者",
  replace: "替换",
  skip: "跳过",
};
/** 循环顺序：先安全档，再替换，最后跳过，转一圈回来。 */
const CONFLICT_ORDER = ["keep_both", "replace", "skip"];

function conflictLabel(): string {
  return CONFLICT_LABELS[conflictPolicy()] ?? CONFLICT_LABELS.keep_both;
}

function setConflictPolicy(next: string) {
  apply(Wfiles.setConflictPolicy(next));
}

function cycleConflictPolicy() {
  const at = CONFLICT_ORDER.indexOf(conflictPolicy());
  setConflictPolicy(CONFLICT_ORDER[(at + 1) % CONFLICT_ORDER.length]);
}

// ---- 选中 ----

function has(path: string): boolean {
  return selection().includes(path);
}

/** 单选模式下点一行：替换选中。 */
function selectOnly(path: string) {
  selection.set([path]);
}

/** 多选模式下点一行：切换它的选/不选。 */
function toggleSelected(path: string) {
  const now = selection();
  selection.set(now.includes(path) ? now.filter((p) => p !== path) : [...now, path]);
}

function selectAll() {
  selection.set(rows().map((row) => row.path));
}

function clearSelection() {
  selection.set([]);
}

function leaveMulti() {
  multi.set(false);
  selection.set([]);
  anchor.set(null);
}

// ---- 键盘导航 ----
//
// 为什么只有键盘能 ⇧ 扩选：鼠标的 `onPress` 不带事件参数，拿不到修饰键；
// `nsh/keymap` 却把按键字符串原样给 handler。所以「⇧+方向键扩选」是
// 这个客户端里唯一能表达"范围选择"的地方。

function indexOf(path: string | null): number {
  if (path === null) return -1;
  return rows().findIndex((row) => row.path === path);
}

/** 让第 `at` 行成为焦点，并把它滚进视野（虚拟列表才知道它在哪）。 */
function focusAt(at: number) {
  const list = rows();
  if (at < 0 || at >= list.length) return;
  setFocus(list[at].path);
  listNode()?.scrollToIndex({ index: at, viewPosition: 0.5, animated: true });
}

function moveFocus(delta: number, extend: boolean) {
  const list = rows();
  if (list.length === 0) return;
  const at = indexOf(focus());
  // 还没有焦点时：向下从第一行起，向上从最后一行起（跟列表一致的直觉）。
  const next = at === -1 ? (delta > 0 ? 0 : list.length - 1) : at + delta;
  const clamped = Math.max(0, Math.min(list.length - 1, next));
  const target = list[clamped].path;

  if (!extend) {
    setFocus(target);
    anchor.set(target);
    selection.set([target]);
    listNode()?.scrollToIndex({ index: clamped, viewPosition: 0.5, animated: true });
    return;
  }

  // ⇧：从锚点扩到新焦点。没有锚点时把"起点"当成锚点（第一次按 ⇧ 不会跳）。
  const from = anchor() ?? (at === -1 ? target : list[at].path);
  anchor.set(from);
  const a = indexOf(from);
  if (a === -1) {
    selection.set([target]);
  } else {
    const lo = Math.min(a, clamped);
    const hi = Math.max(a, clamped);
    selection.set(list.slice(lo, hi + 1).map((row) => row.path));
  }
  // 范围选择天然是多选，把多选条亮出来（否则用户看不到"选了 7 个"）。
  if (!multi()) multi.set(true);
  setFocus(target);
  listNode()?.scrollToIndex({ index: clamped, viewPosition: 0.5, animated: true });
}

/** 回车：目录就进去，文件就明说打不开（这个客户端没有"用默认应用打开"）。 */
function openFocused() {
  const at = indexOf(focus());
  if (at === -1) return;
  const row = rows()[at];
  if (row.is_dir) {
    go(row.path);
  } else {
    status.set("这个客户端还不能用默认应用打开文件");
    isError.set(true);
  }
}

/**
 * 撤销按钮上的整句：光写「撤销」等于让用户闭眼按 Cmd+Z。
 *
 * `HistoryEntry::describe()` 会把完整文件名拼进来（「重命名 X 为 Y」），
 * 两个长名字能把按钮撑到上千像素，把地址栏其它按钮挤出可见区——GUI 版
 * 为这件事专门有 `max_w(260.)` 和一条判据。NSH 不量像素，改成**字符截断**；
 * 完整描述不丢，挂在按钮的 `accessibilityLabel` 上（读屏与判据都读得到）。
 */
const UNDO_LABEL_CHARS = 24;

function undoText(): string {
  const label = undoLabel();
  if (!label) return "撤销";
  const cut =
    label.length > UNDO_LABEL_CHARS
      ? `${label.slice(0, UNDO_LABEL_CHARS)}…`
      : label;
  return `撤销：${cut}`;
}

function undoA11y(): string {
  const label = undoLabel();
  return label ? `撤销：${label}` : "撤销";
}

function undo() {
  void (async () => {
    apply(await Wfiles.undoAsync());
  })();
}

async function trashSelected() {
  const paths = selection();
  if (paths.length === 0) return;
  apply(await Wfiles.deleteAsync(paths, false));
  selection.set([]);
}

function copySelected(cut: boolean) {
  const paths = selection();
  if (paths.length === 0) return;
  clipboard.set({ paths, cut });
  const what =
    paths.length === 1 ? displayName(paths[0]) : `${paths.length} 个项目`;
  status.set(cut ? `已剪切 ${what}` : `已复制 ${what}`);
  isError.set(false);
}

async function paste() {
  const held = clipboard();
  if (!held || !here()) return;
  // 异步版：搬运的阻塞部分在 Rust 侧的独立线程上跑，界面不会被卡住；
  // `withProgress` 负责在这期间轮询进度、结束后收表。
  apply(await withProgress(() => Wfiles.copyAsync(held.paths, here())));
  if (held.cut) clipboard.set(null);
}

// ---- 新建 / 重命名 ----
//
// 用一条**内联输入行**而不是浮层：浮层要自己算位置和遮罩，而这里需要的是
// 「输入 → 回车 → 校验」这条链。失败时面板留着、错误显示在状态栏，
// 用户可以改个名字再试（与 GUI 版的行为一致）。

const TEMPLATE_LABELS: Record<string, string> = {
  plain: "空白文件",
  markdown: "Markdown 文档",
  rust: "Rust 源文件",
  python: "Python 脚本",
  shell: "Shell 脚本",
  json: "JSON 文件",
  yaml: "YAML 文件",
  html: "HTML 文件",
};

function startRename() {
  const paths = selection();
  // 一次只能改一个名字——选中多个时不猜用户指哪个，直说。
  if (paths.length !== 1) {
    status.set("重命名一次只能改一个");
    isError.set(true);
    return;
  }
  const path = paths[0];
  pending.set({ kind: "rename", path, label: "重命名" });
  promptValue.set(displayName(path));
}

function startFolder() {
  const parent = here();
  if (!parent) return;
  pending.set({ kind: "folder", parent, label: "新建文件夹" });
  // 预填一个没被占用的名字，用户按回车就能建出来。
  promptValue.set(Wfiles.freshFolderName(parent));
}

function startFile(template: string) {
  const parent = here();
  if (!parent) return;
  pending.set({
    kind: "file",
    parent,
    template,
    label: `新建${TEMPLATE_LABELS[template] ?? "文件"}`,
  });
  promptValue.set(Wfiles.freshFileName(parent, template));
}

async function submitPrompt() {
  const current = pending();
  if (!current) return;
  const value = promptValue();
  let ok = false;
  if (current.kind === "rename") {
    ok = apply(await Wfiles.renameAsync(current.path, value));
  } else if (current.kind === "folder") {
    ok = apply(await Wfiles.createFolderAsync(current.parent, value));
  } else {
    ok = apply(await Wfiles.createFileAsync(current.parent, value, current.template));
  }
  // 失败时面板不关：错误已经在状态栏，用户改个名字就能重试。
  if (ok) pending.set(null);
}

function displayName(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.length ? parts[parts.length - 1] : path;
}

async function dropCopied(uris: string[]) {
  dragging.set(false);
  if (!here() || uris.length === 0) return;
  const paths = uris.map(uriToPath);
  apply(await withProgress(() => Wfiles.copyAsync(paths, here())));
}

// NSH 给的是转义过的绝对 file: URL，别手剥前缀。
function uriToPath(uri: string): string {
  try {
    return decodeURIComponent(new URL(uri).pathname);
  } catch {
    return decodeURIComponent(uri.replace(/^file:\/\//, ""));
  }
}

function toggleBench() {
  if (bench()) {
    bench.set(false);
    apply(Wfiles.refresh());
    return;
  }
  apply(Wfiles.benchRows(10000));
  bench.set(true);
  status.set("基准模式：10000 行（用 nsh run --fps 量）");
}

function rowMenu(at: { x: number; y: number } | undefined) {
  const count = selection().length;
  const many = count > 1;
  const items = [
    { label: "刷新", shortcut: "mod+r", onSelect: refresh },
    { label: "上层", shortcut: "mod+up", onSelect: up },
    { type: "separator" as const },
    {
      label: many ? `复制 ${count} 项` : "复制",
      shortcut: "mod+c",
      disabled: count === 0,
      onSelect: () => copySelected(false),
    },
    {
      label: many ? `剪切 ${count} 项` : "剪切",
      shortcut: "mod+x",
      disabled: count === 0,
      onSelect: () => copySelected(true),
    },
    {
      label: "粘贴",
      shortcut: "mod+v",
      disabled: !clipboard(),
      onSelect: paste,
    },
    { type: "separator" as const },
    {
      label: "重命名",
      disabled: count !== 1,
      onSelect: startRename,
    },
    { label: "新建文件夹", onSelect: startFolder },
    {
      label: multi() ? "退出多选" : "多选",
      checked: multi(),
      onSelect: () => (multi() ? leaveMulti() : multi.set(true)),
    },
    { type: "separator" as const },
    {
      label: many ? `移到回收站（${count} 项）` : "移到回收站",
      shortcut: "mod+backspace",
      disabled: count === 0,
      onSelect: trashSelected,
    },
    {
      label: bench() ? "退出 10000 行基准" : "装载 10000 行基准",
      checked: bench(),
      onSelect: toggleBench,
    },
  ];
  return Menu.popup(items, at);
}

// ---- 首次加载 + 原生菜单 ----

apply(Wfiles.boot());

// 键盘导航。**不放在 `if (isAvailable)` 里面**：快捷键本身是跨平台的
// （手机上没有键去触发而已），而 `isAvailable` 在 headless 测试里是 false
// （默认平台 ios），放进去就等于测试永远驱动不到这段——第一版就是这么红的。
// `when: "notEditing"` 是关键：重命名/新建的输入框有焦点时方向键归输入框，
// 否则在名字里按一下就把选中丢了。
// 界面里只有这里能拿到修饰键信息，所以 ⇧ 扩选只能绑在键盘上。
keymap.bind(
  {
    down: () => moveFocus(1, false),
    up: () => moveFocus(-1, false),
    "shift+down": () => moveFocus(1, true),
    "shift+up": () => moveFocus(-1, true),
    enter: () => openFocused(),
    home: () => focusAt(0),
    end: () => focusAt(rows().length - 1),
  },
  { when: "notEditing" },
);

if (isAvailable) {
  // 0.4.0：`Window.setTitle` 没了，改成对主窗口 `WindowRef` 下属性。
  Window.get("main")?.set({ title: "WFiles" });
  effect(() => {
    Menu.setApplicationMenu([
      {
        label: "文件",
        submenu: [
          { label: "刷新", shortcut: "mod+r", onSelect: refresh },
          { label: "上层", shortcut: "mod+up", onSelect: up },
          { type: "separator" },
          { label: "新建文件夹", shortcut: "mod+shift+n", onSelect: startFolder },
          {
            label: "新建文件（空白）",
            shortcut: "mod+alt+n",
            onSelect: () => startFile("plain"),
          },
          { label: "新建 Markdown", onSelect: () => startFile("markdown") },
          { label: "重命名", onSelect: startRename },
          { type: "separator" },
          {
            label: undoText(),
            shortcut: "mod+z",
            disabled: !canUndo(),
            onSelect: undo,
          },
          { type: "separator" },
          { label: "移到回收站", shortcut: "mod+backspace", onSelect: trashSelected },
          { role: "quit" },
        ],
      },
      {
        label: "编辑",
        submenu: [
          { label: "复制", shortcut: "mod+c", onSelect: () => copySelected(false) },
          { label: "剪切", shortcut: "mod+x", onSelect: () => copySelected(true) },
          { label: "粘贴", shortcut: "mod+v", onSelect: paste },
          { type: "separator" },
          {
            label: "全选",
            shortcut: "mod+a",
            disabled: !multi(),
            onSelect: selectAll,
          },
          {
            label: "多选",
            checked: multi(),
            onSelect: () => (multi() ? leaveMulti() : multi.set(true)),
          },
          { type: "separator" },
          // 重名策略：与工具栏那个循环按钮是同一份状态（权威值在 core）。
          {
            label: "重名时保留两者",
            checked: conflictPolicy() === "keep_both",
            onSelect: () => setConflictPolicy("keep_both"),
          },
          {
            label: "重名时替换",
            checked: conflictPolicy() === "replace",
            onSelect: () => setConflictPolicy("replace"),
          },
          {
            label: "重名时跳过",
            checked: conflictPolicy() === "skip",
            onSelect: () => setConflictPolicy("skip"),
          },
        ],
      },
      {
        label: "视图",
        submenu: [
          {
            label: showHidden() ? "隐藏隐藏项" : "显示隐藏项",
            shortcut: "mod+shift+.",
            onSelect: toggleHidden,
          },
          { type: "separator" },
          { label: "按名称排序", checked: sortKey() === "name", onSelect: () => sortBy("name") },
          { label: "按大小排序", checked: sortKey() === "size", onSelect: () => sortBy("size") },
          {
            label: "按修改时间排序",
            checked: sortKey() === "modified",
            onSelect: () => sortBy("modified"),
          },
          { type: "separator" },
          {
            label: bench() ? "退出 10000 行基准" : "装载 10000 行基准",
            checked: bench(),
            onSelect: toggleBench,
          },
        ],
      },
    ]);
  });
}

// ---- 界面 ----

function chip(
  label: string,
  onPress: () => void,
  testID: string,
  enabled = true,
  a11y?: string,
) {
  return (
    <View
      class={enabled ? "rounded px-3 py-1 bg-muted" : "rounded px-3 py-1 bg-muted opacity-40"}
      onPress={enabled ? onPress : undefined}
      testID={testID}
      accessibilityLabel={a11y}
    >
      <Text class="text-sm">{label}</Text>
    </View>
  );
}

function headerCell(label: string, key: string) {
  const active = sortKey() === key;
  const arrow = active ? (sortDesc() ? " ↓" : " ↑") : "";
  return (
    <View class="px-1" onPress={() => sortBy(key)} testID={`sort-${key}`}>
      <Text class={active ? "text-xs font-medium" : "text-xs opacity-60"}>{label + arrow}</Text>
    </View>
  );
}

function App() {
  return (
    <View
      class="flex-1 bg-background text-foreground"
      onFileDragEnter={() => dragging.set(true)}
      onFileDragLeave={() => dragging.set(false)}
      onFileDrop={(e) => dropCopied(e.uris)}
    >
      {/* 地址栏 */}
      <View class="flex-row items-center gap-2 px-3 py-2 border-b border-border">
        {chip("上级", up, "up-button")}
        {chip("刷新", refresh, "refresh-button")}
        <Text class="flex-1 text-sm opacity-70" style={{ numberOfLines: 1 }} testID="path">
          {here()}
        </Text>
        {chip(showHidden() ? "隐藏隐藏项" : "显示隐藏项", toggleHidden, "hidden-button")}
        {chip("新建文件夹", startFolder, "new-folder-button")}
        {chip("新建文件", () => startFile("plain"), "new-file-button")}
        {chip("重命名", startRename, "rename-button", selection().length === 1)}
        {chip(undoText(), undo, "undo-button", canUndo(), undoA11y())}
      </View>

      {/* 多选条：只在多选模式出现，免得常态挤掉地址栏的宽度 */}
      {multi() ? (
        <View class="flex-row items-center gap-2 px-3 py-2 border-b border-border">
          <Text class="text-sm" testID="selection-count">{`已选 ${selection().length} 项`}</Text>
          {chip("全选", selectAll, "select-all-button")}
          {chip("清空", clearSelection, "clear-selection-button", selection().length > 0)}
          <View class="flex-1" />
          {chip("比较", openPair, "pair-button", selection().length === 2)}
          {chip("复制", () => copySelected(false), "copy-button", selection().length > 0)}
          {chip(
            "移到回收站",
            trashSelected,
            "trash-button",
            selection().length > 0,
          )}
          {chip("退出多选", leaveMulti, "leave-multi-button")}
        </View>
      ) : null}

      {/* 工具栏第一行：过滤 + 粘贴（跟着当前目录走的操作） */}
      <View class="flex-row items-center gap-2 px-3 py-2 border-b border-border">
        <Input
          class="flex-1 text-sm rounded px-2 py-1 bg-muted"
          value={query()}
          placeholder="过滤：名字、*.rs、size:>1M、modified:today"
          // 0.4.0：`onChange` 的载荷从裸字符串变成 `{ value }`。
          onChange={(e) => {
            query.set(e.value);
            apply(Wfiles.setQuery(e.value));
          }}
          testID="filter"
        />
        {chip("粘贴", paste, "paste-button", !!clipboard())}
      </View>

      {/* 工具栏第二行：策略与面板开关。
          这两行必须分开：原来一行要装「过滤框 + 7 颗按钮」，再加「磁盘」这颗
          就把最后两颗挤出可见区（headless 里连 `press` 都找不到它们）。
          固定部分之和明显大于一行能给的宽，是结构错了，不是尺寸没调好——
          砍按钮、加横向滚动都是在给错误的结构打补丁。 */}
      <View class="flex-row items-center gap-2 px-3 py-2 border-b border-border">
        {chip(`重名：${conflictLabel()}`, cycleConflictPolicy, "conflict-button")}
        {chip(
          `逐项问：${askConflicts() ? "开" : "关"}`,
          toggleAskConflicts,
          "ask-conflicts-button",
        )}
        {chip(`预览：${previewOn() ? "开" : "关"}`, togglePreview, "preview-button")}
        {chip(`磁盘：${disksOn() ? "开" : "关"}`, toggleDisks, "disks-button")}
        {chip("搜索", openSearch, "search-button", !searchOpen())}
        {chip("多选", () => multi.set(true), "multi-button", !multi())}
        {chip(bench() ? "退出基准" : "10000 行", toggleBench, "bench-button")}
      </View>

      {/* 磁盘面板：卷名 + 「还剩多少」。那行字是 core 拼的，界面不许按
          total/free 自己再算一遍；探不到容量的卷只留名字。 */}
      {disksOn() ? (
        <View class="border-b border-border" testID="disks-pane">
          <Text class="px-3 pt-2 text-xs opacity-60" testID="disks-title">
            {`磁盘（${disks().length}）`}
          </Text>

          {/* 收藏段：与卷放在同一个面板里，因为两者都是"去哪儿"。
              数据来自 core（同一个 settings.json），不是界面另存的一份。 */}
          <View class="flex-row items-center gap-2 px-3 pt-2">
            <Text class="flex-1 text-xs opacity-60" testID="favorites-title">
              {`收藏（${favorites().length}）`}
            </Text>
            {chip(
              favHere() ? "取消收藏" : "收藏当前目录",
              toggleFavoriteHere,
              "favorite-here",
            )}
          </View>
          {favorites().length === 0 ? (
            <Text class="px-3 py-1 text-xs opacity-40" testID="favorites-empty">
              还没有收藏
            </Text>
          ) : (
            favorites().map((item, index) => (
              <View
                class="flex-row items-center gap-2 px-3 py-1"
                onPress={() => goFavorite(item)}
                testID={`favorite-row:${index}`}
              >
                <Text class="text-sm" testID={`favorite-name:${index}`}>
                  {item.name}
                </Text>
                <View class="flex-1" />
                <Text class="text-xs opacity-50">{item.is_dir ? "目录" : "文件"}</Text>
              </View>
            ))
          )}
          {disks().length === 0 ? (
            <Text class="px-3 py-1 text-xs opacity-40" testID="disks-empty">
              没有探测到其它卷
            </Text>
          ) : (
            disks().map((volume, index) => (
              <View
                class="flex-row items-center gap-2 px-3 py-1"
                onPress={() => goDisk(volume)}
                testID={`disk-row:${index}`}
              >
                <Text class="text-sm" testID={`disk-name:${index}`}>{volume.name}</Text>
                <View class="flex-1" />
                {volume.capacity_text !== null ? (
                  <Text class="text-xs opacity-60" testID={`disk-capacity:${index}`}>
                    {volume.capacity_text}
                  </Text>
                ) : null}
              </View>
            ))
          )}

          {/* 远程段：SFTP 单独一套（远程路径不是 PathBuf，进不了本地目录模型）。
              连不上就把 core 的原话贴出来——主机、端口、具体原因都在里面。 */}
          <View class="flex-row items-center gap-2 px-3 pt-2">
            <Text class="flex-1 text-xs opacity-60" testID="remote-title">
              远程（SFTP）
            </Text>
          </View>
          <View class="flex-row items-center gap-2 px-3 py-1">
            <Input
              class="flex-1 text-xs rounded px-2 py-1 bg-muted"
              value={remoteUrl()}
              placeholder="sftp://host:22/path/"
              onChange={(e) => remoteUrl.set(e.value)}
              testID="remote-url"
            />
            <Input
              class="w-24 text-xs rounded px-2 py-1 bg-muted"
              value={remoteUser()}
              placeholder="用户名"
              onChange={(e) => remoteUser.set(e.value)}
              testID="remote-user"
            />
            <Input
              class="w-24 text-xs rounded px-2 py-1 bg-muted"
              value={remotePassword()}
              placeholder="密码"
              onChange={(e) => remotePassword.set(e.value)}
              testID="remote-password"
            />
            {chip("连接", connectRemote, "remote-connect")}
          </View>
          {remoteError() !== null ? (
            <Text class="px-3 py-1 text-xs text-red-500" testID="remote-error">
              {remoteError()}
            </Text>
          ) : null}
          {remoteEntries().map((entry, index) => (
            <View
              class="flex-row items-center gap-2 px-3 py-1"
              onPress={() => downloadRemote(entry)}
              testID={`remote-row:${index}`}
            >
              <Text class="text-sm" testID={`remote-name:${index}`}>
                {entry.name}
              </Text>
              <View class="flex-1" />
              <Text class="text-xs opacity-50">{entry.is_dir ? "目录" : entry.size_text}</Text>
            </View>
          ))}
        </View>
      ) : null}

      {/* 搜索行：输入 + 开始 / 取消 / 关闭。回车也等于「开始」。 */}
      {!diffOn() && searchOpen() ? (
        <View class="flex-row items-center gap-2 px-3 py-2 border-b border-border" testID="search-row">
          <Input
            class="flex-1 text-sm rounded px-2 py-1 bg-muted"
            value={searchQuery()}
            placeholder="搜索：名字、*.rs、content:词、size:>1M"
            onChange={(e) => searchQuery.set(e.value)}
            onSubmit={runSearch}
            testID="search-input"
          />
          {chip("开始", runSearch, "search-start")}
          {chip("取消", cancelRunningSearch, "search-cancel", !searchDone())}
          {chip("关闭", closeSearch, "search-close")}
        </View>
      ) : null}

      {/* 新建 / 重命名的输入行：有操作在等名字时才出现 */}
      {pending() ? (
        <View class="flex-row items-center gap-2 px-3 py-2 border-b border-border">
          <Text class="text-xs opacity-70" testID="prompt-label">
            {pending()?.label ?? ""}
          </Text>
          <Input
            class="flex-1 text-sm rounded px-2 py-1 bg-muted"
            value={promptValue()}
            placeholder="名字"
            onChange={(e) => promptValue.set(e.value)}
            onSubmit={submitPrompt}
            testID="prompt-input"
          />
          {chip("确定", submitPrompt, "prompt-ok")}
          {chip("取消", () => pending.set(null), "prompt-cancel")}
        </View>
      ) : null}

      {/* 重名提问行：搬运停在这一项上等人回答时才出现 */}
      {asking() ? (
        <View class="flex-row items-center gap-2 px-3 py-2 border-b border-border" testID="conflict-row">
          <Text class="flex-1 text-xs opacity-70" testID="conflict-question">
            {`目标里已经有「${asking()?.name ?? ""}」，这一项怎么办？`}
          </Text>
          {chip("保留两者", () => answerConflict("keep_both"), "conflict-keep")}
          {chip("替换", () => answerConflict("replace"), "conflict-replace")}
          {chip("跳过", () => answerConflict("skip"), "conflict-skip")}
        </View>
      ) : null}

      {/* 搬运进度行：只在真的有搬运在跑时出现 */}
      {transfer() ? (
        <View class="flex-row items-center gap-2 px-3 py-2 border-b border-border" testID="progress-row">
          <Text class="flex-1 text-xs opacity-70" style={{ numberOfLines: 1 }} testID="progress-label">
            {transfer()?.label ?? ""}
          </Text>
          <Text class="text-xs opacity-60" testID="progress-text">
            {progressText()}
          </Text>
          {chip("中止", abortTransfer, "abort-button")}
        </View>
      ) : null}

      {/* 列头：搜索态下换一副——结果不是目录清单，列不一样 */}
      {/* 差异面板：替换掉表头与列表（它自带标题和一份结论句）。
          每一行都是 core 给的内容，界面只排版。 */}
      {diffOn() ? (
        <View class="flex-1" testID="diff-pane">
          <View class="flex-row items-center gap-2 px-3 py-2 border-b border-border">
            <Text class="flex-1 text-xs opacity-70" testID="diff-title">
              {`${diffData()?.mode === "dir" ? "比较" : "差异"}：${baseOf(diffPair()?.left ?? "")} ↔ ${baseOf(diffPair()?.right ?? "")}`}
            </Text>
            <Text class="text-xs opacity-60" testID="diff-summary">
              {diffData()?.ok === false
                ? (diffData()?.error ?? "比不了")
                : (diffData()?.summary ?? "")}
            </Text>
            {chip("关闭", closeDiff, "close-diff-button")}
          </View>
          <List
            each={diffLines()}
            keyExtractor={(line) => line.key}
            style={{ flex: 1 }}
            testID="diff-list"
            empty={() => (
              <View class="flex-1 items-center justify-center p-8">
                <Text class="text-sm opacity-60" testID="diff-empty">
                  两边一样，没得看
                </Text>
              </View>
            )}
          >
            {(line) => (
              <Text class="px-3 text-xs" testID="diff-line">
                {line().text}
              </Text>
            )}
          </List>
        </View>
      ) : null}

      {!diffOn() && searchOpen() ? (
        <View class="flex-row items-center gap-2 px-3 py-1 border-b border-border">
          <View class="w-10">
            <Text class="text-xs opacity-60">类型</Text>
          </View>
          <View class="flex-1">
            <Text class="text-xs opacity-60">命中</Text>
          </View>
          <View class="flex-1">
            <Text class="text-xs opacity-60">位置 / 那一行</Text>
          </View>
        </View>
      ) : (
        <View class="flex-row items-center gap-2 px-3 py-1 border-b border-border">
          <View class="w-10">
            <Text class="text-xs opacity-60">类型</Text>
          </View>
          <View class="flex-1">{headerCell("名称", "name")}</View>
          <View class="w-24">{headerCell("大小", "size")}</View>
          <View class="w-24">{headerCell("修改时间", "modified")}</View>
        </View>
      )}

      {!diffOn() && searchOpen() ? (
        <List
          each={searchHits()}
          keyExtractor={(hit) => hit.path}
          style={{ flex: 1 }}
          testID="search-list"
          empty={() => (
            <View class="flex-1 items-center justify-center p-8">
              <Text class="text-sm opacity-60" testID="search-empty">
                {searchDone() ? "没有命中" : "搜索中…"}
              </Text>
            </View>
          )}
        >
          {(item) => (
            <View
              class="flex-row items-center gap-2 px-3 py-1.5"
              onPress={() => openHit(item())}
              testID="hit-row"
            >
              <View class="w-10">
                <Text class="text-xs opacity-60">{item().is_dir ? "目录" : "文件"}</Text>
              </View>
              <Text class="flex-1 text-sm" style={{ numberOfLines: 1 }} testID={`hit:${item().name}`}>
                {item().name}
              </Text>
              <Text class="flex-1 text-xs opacity-60" style={{ numberOfLines: 1 }}>
                {hitDetail(item())}
              </Text>
            </View>
          )}
        </List>
      ) : (
        <List
          each={rows()}
          keyExtractor={(row) => row.path}
          style={{ flex: 1 }}
          testID="file-list"
          ref={(node) => listNode.set(node as unknown as ListHandle)}
          empty={() => (
            <View class="flex-1 items-center justify-center p-8">
              <Text class="text-sm opacity-60">这里没有项目，或者目录打不开</Text>
            </View>
          )}
        >
        {(item) => (
          <View
            class={
              has(item().path)
                ? "flex-row items-center gap-2 px-3 py-1.5 bg-muted"
                : "flex-row items-center gap-2 px-3 py-1.5"
            }
            onPress={() => {
              const row = item();
              // 多选期间一行都只表示"选/不选"。要进目录先退出多选——
              // 没有修饰键就分不出"点开"和"点选"，只能二选一。
              if (multi()) {
                toggleSelected(row.path);
                setFocus(row.path);
                anchor.set(row.path);
              } else if (row.is_dir) {
                go(row.path);
              } else {
                selectOnly(row.path);
                // 点过的那一行就是焦点：接着按方向键要从这里走。
                setFocus(row.path);
                anchor.set(row.path);
              }
            }}
            onContextMenu={(e) => {
              // 右键点的是"没选中过的那一行"时先把它选中，
              // 否则菜单里的操作会对着上一次的选中项下手。
              if (!has(item().path)) selectOnly(item().path);
              rowMenu(e.pagePosition);
            }}
            testID="file-row"
          >
            <View class="w-10">
              <Text class="text-xs opacity-60">{item().is_dir ? "目录" : "文件"}</Text>
            </View>
            <Text class="flex-1 text-sm" style={{ numberOfLines: 1 }} testID={`row:${item().name}`}>
              {item().name}
            </Text>
            <View class="w-24">
              <Text class="text-xs opacity-60">{item().size_text}</Text>
            </View>
            <View class="w-24">
              <Text class="text-xs opacity-60">{item().modified}</Text>
            </View>
          </View>
        )}
        </List>
      )}

      {/* 预览区：焦点行的一个只读快照（`Wfiles.preview` → core 的 preview::load）。
          行数由界面截断，不靠渲染层量高度。 */}
      {previewOn() ? (
        <View class="border-t border-border" testID="preview-pane">
          <Text class="px-3 pt-2 text-xs opacity-60" testID="preview-title">
            {previewTitle()}
          </Text>

          {/* 缩略图：**只有 core 说是图片**才画（`media === "image"`，来自
              `Kind::id()`，不是拿中文 label 比）。真的解码交给 `Image`——
              core 判类型时绝不开文件，云占位会因此触发下载。
              路径直接给磁盘上那个文件，不经桥接层复制一份字节。 */}
          {previewData()?.media === "image" && focus() !== null ? (
            <View class="px-3 pt-1" testID="preview-thumb">
              <Image
                src={focus()!}
                accessibilityLabel={`thumb:${baseName(focus()!)}`}
                style={{ width: 120, height: 80, objectFit: "contain" }}
              />
            </View>
          ) : null}
          <List
            each={previewRows()}
            keyExtractor={(line) => line.key}
            style={{ height: 96 }}
            testID="preview-list"
            empty={() => (
              <Text class="px-3 py-1 text-xs opacity-40" testID="preview-empty">
                选中一个文件看看
              </Text>
            )}
          >
            {(line) => (
              <Text class="px-3 text-xs" testID="preview-line">
                {line().text}
              </Text>
            )}
          </List>
        </View>
      ) : null}

      <View class="flex-row items-center gap-3 px-3 py-2 border-t border-border">
        <Text class={isError() ? "flex-1 text-xs text-red-500" : "flex-1 text-xs opacity-70"} testID="status">
          {dragging() ? "松手即把文件复制到当前目录" : status()}
        </Text>
        <Text class="text-xs opacity-50" testID="shown">
          {searchOpen() ? `命中 ${searchHits().length} 条` : `${shown()} / ${total()}`}
        </Text>
        <Text class="text-xs opacity-50" testID="proof">
          {Format.fileSize(1536)}
        </Text>
      </View>
    </View>
  );
}

mount(App);
