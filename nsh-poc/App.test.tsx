// WFiles × NSH 客户端的 headless 测试。
//
// 这几条都不画界面就能跑，钉的是「桥」有没有接对：
//  1. 界面真的把 Rust 侧格式化出来的文本搬上了屏（Format.fileSize）；
//  2. 列表来自 `wfiles_core` 的 `contract::Core`，不是这里重写的一份读盘逻辑——
//     判据是**目录优先**的排序约定和快照字段齐全，这两点只有真核心里有；
//  3. 过滤与排序也走核心，界面不自己算（`total` 不被过滤改掉是核心的约定）。

import { expect, open, test } from "nsh/test";
import { Wfiles } from "wfiles/native";

test("界面把 Rust 核心格式化出来的大小搬上屏", async () => {
  Wfiles.reset();
  const app = await open("./App.tsx");
  const texts = app.texts();
  expect(texts).toContain("类型");
  // 状态栏那个 `Format.fileSize(1536)`。
  expect(texts).toContain("1.5 KB");
});

test("列表来自核心：目录优先、字段齐全", () => {
  Wfiles.reset();
  const snap = JSON.parse(Wfiles.open(Wfiles.home()));
  expect(snap.ok).toBe(true);
  expect(Array.isArray(snap.rows)).toBe(true);
  expect(typeof snap.total).toBe("number");
  expect(typeof snap.shown).toBe("number");
  expect(snap.sort.key).toBe("name");

  // 目录优先是 WFiles 的排序约定，不是纯字母序。核心里改坏了这条会红。
  let seenFile = false;
  let dirAfterFile = false;
  for (const row of snap.rows) {
    if (row.is_dir) {
      if (seenFile) dirAfterFile = true;
    } else {
      seenFile = true;
    }
  }
  expect(dirAfterFile).toBe(false);
});

test("过滤与排序都走核心，界面不自己算", () => {
  Wfiles.reset();
  const all = JSON.parse(Wfiles.open(Wfiles.home()));
  expect(all.total).toBeGreaterThan(0);

  // 一个命中不了任何东西的词：`shown` 归零，但 `total` 保持磁盘真实数
  //（这正是 GUI 版「N / M」那一栏依赖的东西）。
  const filtered = JSON.parse(Wfiles.setQuery("zzzz-no-such-file-zzzz"));
  expect(filtered.shown).toBe(0);
  expect(filtered.total).toBe(all.total);

  // 清掉过滤，再按大小排一次：`sort` 要跟着变。
  Wfiles.setQuery("");
  const sorted = JSON.parse(Wfiles.setSort("size"));
  expect(sorted.sort.key).toBe("size");
});

test("隐藏项开关真的落到核心：按钮文案跟着翻", async () => {
  Wfiles.reset();
  const app = await open("./App.tsx");
  // 初始状态由核心的快照决定（`show_hidden: false`）。
  expect(app.texts()).toContain("显示隐藏项");
  await app.press("hidden-button");
  // 按钮上写的是"按下之后会变成什么"，所以打开之后文案是「隐藏隐藏项」。
  expect(app.texts()).toContain("隐藏隐藏项");
});

test("过滤框打字 → 列表归零，但总数不变", async () => {
  Wfiles.reset();
  const app = await open("./App.tsx");
  await app.type("filter", "zzzz-no-such-file-zzzz");
  // 状态栏右侧是「shown / total」：命中不了任何东西时是 `0 / N`，
  // N 仍然是磁盘上的真实条目数（核心的约定，界面不许自己算）。
  expect(app.texts().some((text) => /^0 \/ \d+/.test(text))).toBe(true);
  Wfiles.setQuery("");
});

test("点列头排序：方向标记跟着变", async () => {
  Wfiles.reset();
  const app = await open("./App.tsx");
  expect(app.texts()).toContain("名称 ↑");
  await app.press("sort-size");
  expect(app.texts()).toContain("大小 ↑");
  // 再点同一列要翻向，这是 `Sort::toggled` 的规则。
  await app.press("sort-size");
  expect(app.texts()).toContain("大小 ↓");
});

// ---- 新建 / 重命名（界面到磁盘的整条链）----
//
// 这几条**不往用户主目录里建东西**：先在自造的临时目录里 `open`，
// 界面顶层的 `boot()` 发现已有内核就只刷新，于是测试跑在临时目录上。

test("新建文件夹：面板 → 落盘 → 列表刷新", async () => {
  Wfiles.reset();
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  expect(temp.ok).toBe(true);
  const app = await open("./App.tsx");

  await app.press("new-folder-button");
  // 输入行的标题写的是「新建文件夹」，预填名不冲突。
  expect(app.texts()).toContain("新建文件夹");
  await app.press("prompt-ok");

  // 列表来自核心，所以「看得到」就说明真的建出来了并且刷新过。
  expect(app.texts()).toContain("未命名文件夹");
});

test("新建文件：按模板后缀预填，落盘后出现在列表里", async () => {
  Wfiles.reset();
  Wfiles.open(Wfiles.tempDir());
  const app = await open("./App.tsx");

  await app.press("new-file-button");
  expect(app.texts()).toContain("新建空白文件");
  await app.press("prompt-ok");

  expect(app.texts()).toContain("未命名.txt");
});

test("重命名：按钮接上了，弹出的是重命名面板", async () => {
  Wfiles.reset();
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFile(temp.path, "a.txt", "plain");

  const app = await open("./App.tsx");
  await app.press("file-row"); // 选中唯一那一行
  await app.press("rename-button");

  expect(app.texts()).toContain("重命名");
  expect(app.texts()).toContain("确定");
  expect(app.texts()).toContain("取消");

  // 提交那段代码与「新建」共用（`submitPrompt`），那条链由上面两条测试覆盖；
  // 改名本身由 `tests/contract.rs` 覆盖。这里只钉「按钮接上了」——
  // `app.type` 是追加式输入，输入框预填了 `a.txt` 时清不掉，
  // 硬提交只会得到 `a.txtb.txt`，那是在测测试工具而不是测功能。
});

// ---- 多选（选中集合真的喂给了操作，不只是改了个标签）----
//
// 这几条钉的是「选中集合 → 操作参数」这一段。如果界面只是把数字画对、
// 操作仍然只拿第一项，最后一条的「两个文件都落到目标目录」会红。

test("多选：选中的两项真的被复制到目标目录", async () => {
  Wfiles.reset();
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFile(temp.path, "a.txt", "plain");
  Wfiles.createFile(temp.path, "b.txt", "plain");
  Wfiles.createFile(temp.path, "c.txt", "plain");
  Wfiles.createFolder(temp.path, "dest");

  const app = await open("./App.tsx");
  await app.press("multi-button");
  expect(app.texts()).toContain("已选 0 项");

  await app.press("id:row:a.txt");
  await app.press("id:row:b.txt");
  expect(app.texts()).toContain("已选 2 项");

  await app.press("copy-button");
  // 退出多选、进目标目录、粘贴。落盘结果是唯一不会骗人的判据：
  // 如果操作只拿了第一项，`b.txt` 不会出现。
  await app.press("leave-multi-button");
  await app.press("id:row:dest >> nth=0");
  await app.press("paste-button");
  await app.waitUntil(() => app.texts().includes("b.txt"));
  expect(app.texts()).toContain("a.txt");
  // c.txt 没被选，不许跟着过去（这是"多选了全部"和"只多选了这两项"的分水岭）。
  expect(app.texts()).toContain("2 / 2");
});

test("全选与清空：全选覆盖过滤后的全部行，清空归零", async () => {
  Wfiles.reset();
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFile(temp.path, "a.txt", "plain");
  Wfiles.createFile(temp.path, "b.txt", "plain");

  const app = await open("./App.tsx");
  await app.press("multi-button");
  await app.press("select-all-button");
  expect(app.texts()).toContain("已选 2 项");

  await app.press("clear-selection-button");
  expect(app.texts()).toContain("已选 0 项");
});

test("多选期间点行只切换选中，不进目录", async () => {
  Wfiles.reset();
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFolder(temp.path, "sub");

  const app = await open("./App.tsx");
  await app.press("multi-button");
  await app.press("id:row:sub");
  // 还停在原地，只是把它选上了。
  expect(app.texts()).toContain("已选 1 项");
  expect(app.texts()).toContain("sub");
});

// ---- 键盘导航（唯一的"修饰键"入口）----
//
// 鼠标点击不带事件参数，键盘带：`nsh/keymap` 把 `"shift+down"` 原样给
// handler。所以这几条钉的是"方向键真的动了选中与焦点"，**不是**样子货——
// 判据落在"进入哪个目录"和"选中几项"上，改坏必然红。
//
// **必须 `platform: "macos"` 打开**：`nsh/keymap` 在非桌面平台上"保留绑定但
// 没有键去触发"，而 headless 默认平台是 ios。第一版忘了写，三条全红
// （`已选 0 项` 一直不动），排查半天才发现是平台而不是逻辑。

test("方向键移动焦点，回车进入目录", async () => {
  Wfiles.reset();
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  // 目录优先、按名排：d1 在第一行。
  Wfiles.createFolder(temp.path, "d1");
  Wfiles.createFolder(temp.path, "d2");
  // 给 d1 塞一个只有它才有的东西——判据就落在这上面。
  // （不拿地址栏文案当判据：headless 下那行 `numberOfLines={1}` 的 Text
  // 不进 `texts()`，实测过，会得到一个永远失败的好判据。）
  Wfiles.createFile(`${temp.path}/d1`, "inside.txt", "plain");

  const app = await open("./App.tsx", { platform: "macos" });
  // 起点是父目录：d1 / d2 两行。
  expect(app.texts()).toContain("2 / 2");

  await app.keyboard.press("down");
  await app.keyboard.press("enter");

  expect(app.texts()).toContain("inside.txt");
  expect(app.texts()).toContain("1 / 1");
});

test("⇧+方向键从焦点扩选，并亮出多选条", async () => {
  Wfiles.reset();
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFile(temp.path, "a.txt", "plain");
  Wfiles.createFile(temp.path, "b.txt", "plain");
  Wfiles.createFile(temp.path, "c.txt", "plain");

  const app = await open("./App.tsx", { platform: "macos" });
  await app.keyboard.press("down"); // 焦点到 a.txt
  await app.keyboard.press("shift+down"); // 扩到 b.txt
  expect(app.texts()).toContain("已选 2 项");

  await app.keyboard.press("shift+down"); // 再扩到 c.txt
  expect(app.texts()).toContain("已选 3 项");

  // 松开 ⇧ 往回缩：范围由锚点算，不是累加。
  await app.keyboard.press("shift+up");
  expect(app.texts()).toContain("已选 2 项");
});

test("不按 ⇧ 时方向键是替换选中，不累积", async () => {
  Wfiles.reset();
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFile(temp.path, "a.txt", "plain");
  Wfiles.createFile(temp.path, "b.txt", "plain");

  const app = await open("./App.tsx", { platform: "macos" });
  await app.press("multi-button");
  await app.keyboard.press("down");
  expect(app.texts()).toContain("已选 1 项");
  await app.keyboard.press("down");
  // 还是 1 项——移动焦点替换选中（与 Finder 一致），不是累加。
  expect(app.texts()).toContain("已选 1 项");
  expect(app.texts()).not.toContain("已选 2 项");
});

// ---- 重名策略 ----
//
// 策略是 core 的状态（`Command::SetConflictPolicy`），界面只负责把选择发下去。
// 判据落在**落盘结果**上：目标目录里有几行、内容是谁的。
// 只断言按钮文案变了是挑错层——策略压根没传到搬运上也照样绿。

/** 源和目标各有一个同名文件；两者都在自己单独的目录里。 */
function conflictFixture(): { root: string; src: string; dest: string } {
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  const src = `${temp.path}/src`;
  const dest = `${temp.path}/dest`;
  Wfiles.createFolder(temp.path, "src");
  Wfiles.createFolder(temp.path, "dest");
  Wfiles.createFile(src, "a.txt", "plain");
  Wfiles.createFile(dest, "a.txt", "plain");
  return { root: temp.path, src, dest };
}

/** 把 src 里的一个文件复制到 dest，**不粘贴**（粘贴那一刻由调用方掌握）。 */
async function copyOneInto(
  app: Awaited<ReturnType<typeof open>>,
  src: string,
  name: string,
  dest: string,
) {
  // 换目录走桥接层 + 刷新按钮，不点列表行：虚拟列表会把旧格子留在树里。
  Wfiles.enter(src);
  await app.press("refresh-button");
  await app.press("multi-button");
  // `>> nth=0`：虚拟列表会把上一个目录的旧格子留在树里，`row:a.txt` 会匹配到两份
  //（两份指向同一个路径，点哪个都一样）。
  // 0.4.0 的选择器必须带前缀（`id:` / `label:` / `text:` / `view:`），裸 id
  // 不再被当成 id 猜——这是升级后一次红 12 条的根因。
  await app.press(`id:row:${name} >> nth=0`);
  await app.press("copy-button");
  await app.press("leave-multi-button");
  Wfiles.enter(dest);
  await app.press("refresh-button");
}

test("重名策略：按一下换一档，按钮上写着当前档", async () => {
  Wfiles.reset();
  Wfiles.open(Wfiles.tempDir());
  const app = await open("./App.tsx");

  // 默认「保留两者」（core 的 `ConflictPolicy::default()`）。
  expect(app.texts()).toContain("重名：保留两者");
  await app.press("conflict-button");
  expect(app.texts()).toContain("重名：替换");
  await app.press("conflict-button");
  expect(app.texts()).toContain("重名：跳过");
  // 转一圈回来，不会卡在最后一档。
  await app.press("conflict-button");
  expect(app.texts()).toContain("重名：保留两者");
});

test("重名策略「替换」：粘贴后目标目录只留一份", async () => {
  Wfiles.reset();
  const fixture = conflictFixture();
  const app = await open("./App.tsx");

  await copyOneInto(app, fixture.src, "a.txt", fixture.dest);
  await app.press("conflict-button"); // → 替换
  expect(app.texts()).toContain("重名：替换");

  await app.press("paste-button");
  await app.waitUntil(() => app.texts().includes("拷贝完成 1 项"));

  // 替换了就不会多出「a 2.txt」，目标目录仍然只有一行。
  expect(app.texts()).not.toContain("a 2.txt");
  expect(app.texts()).toContain("1 / 1");
});

test("重名策略「跳过」：目标原封不动，状态栏说清跳过了几项", async () => {
  Wfiles.reset();
  const fixture = conflictFixture();
  const app = await open("./App.tsx");

  await copyOneInto(app, fixture.src, "a.txt", fixture.dest);
  await app.press("conflict-button"); // → 替换
  await app.press("conflict-button"); // → 跳过
  expect(app.texts()).toContain("重名：跳过");

  await app.press("paste-button");
  await app.waitUntil(() =>
    app.texts().some((text) => text.includes("跳过") && text !== "重名：跳过"),
  );

  // 一个字节都没动：还是那一行，也没有多出副本。
  expect(app.texts()).not.toContain("a 2.txt");
  expect(app.texts()).toContain("1 / 1");
  // 而且说清了——跳过是唯一"成功但磁盘上没变化"的结果，不吭声等于静默失败。
  expect(app.texts().some((text) => text.includes("跳过 1 项"))).toBe(true);
});

// ---- 逐项问重名 ----
//
// 打开开关之后，搬运会**停**在重名的那一项上等人回答。这条链只有一种判据
// 能证明它真的逐项了：同一批里既完成一项、又跳过一项。
// 整批策略永远做不出这个结果（跳过 = 0 完成，替换 / 保留两者 = 2 完成）。

/** 等某一项的问题出现在屏上，然后按下对应的答案。 */
async function answerWhenAsked(
  app: Awaited<ReturnType<typeof open>>,
  name: string,
  button: string,
) {
  await app.waitUntil(() => app.texts().some((text) => text.includes(`「${name}」`)));
  await app.press(button);
}

test("逐项问：同一批里既替换又跳过，两项各答各的", async () => {
  Wfiles.reset();
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  const src = `${temp.path}/src`;
  const dest = `${temp.path}/dest`;
  Wfiles.createFolder(temp.path, "src");
  Wfiles.createFolder(temp.path, "dest");
  // 两份都重名。src 用 plain、dest 用 markdown：这样"换成谁的"在字节数上就能看出来。
  Wfiles.createFile(src, "a.txt", "plain");
  Wfiles.createFile(dest, "a.txt", "markdown");
  Wfiles.createFile(src, "b.txt", "plain");
  Wfiles.createFile(dest, "b.txt", "markdown");

  const app = await open("./App.tsx");
  expect(app.texts()).toContain("逐项问：关");
  await app.press("ask-conflicts-button");
  expect(app.texts()).toContain("逐项问：开");

  // 两个都选上，复制 → 进目标目录 → 粘贴
  Wfiles.enter(src);
  await app.press("refresh-button");
  await app.press("multi-button");
  await app.press("id:row:a.txt >> nth=0");
  await app.press("id:row:b.txt >> nth=0");
  await app.press("copy-button");
  await app.press("leave-multi-button");
  Wfiles.enter(dest);
  await app.press("refresh-button");
  await app.press("paste-button");

  // 按问到的名字决定，不依赖搬运处理它们的先后。
  await answerWhenAsked(app, "a.txt", "conflict-replace");
  await answerWhenAsked(app, "b.txt", "conflict-skip");

  await app.waitUntil(() =>
    app
      .texts()
      .some((text) => text.includes("拷贝完成 1 项") && text.includes("跳过 1 项")),
  );
  // 替换不留副本、跳过不另存——所以目标目录还是两行。
  expect(app.texts()).not.toContain("a 2.txt");
  expect(app.texts()).not.toContain("b 2.txt");
  // 问完就收工：不该还有一行在等人回答。
  expect(app.texts().some((text) => text.includes("这一项怎么办"))).toBe(false);
});

test("开关关着就是整批一个策略，一次都不问", async () => {
  Wfiles.reset();
  const fixture = conflictFixture();
  const app = await open("./App.tsx");
  // 默认关。
  expect(app.texts()).toContain("逐项问：关");

  await copyOneInto(app, fixture.src, "a.txt", fixture.dest);
  await app.press("paste-button");
  // 没开逐项问：会一口气搬完（默认保留两者）。
  await app.waitUntil(() => app.texts().includes("拷贝完成 1 项"));
  expect(app.texts()).toContain("a 2.txt");
  expect(app.texts().some((text) => text.includes("这一项怎么办"))).toBe(false);
});

// ---- 预览 ----
//
// 预览是 core 的**只读查询**（`preview::load`），界面只负责画。判据落在
// **文件内容**与**类型判断**上：只断言"预览区出现了"是挑错层——一块空面板
// 也满足，而它最容易出的错恰恰是"该显示内容的地方显示了个框"。

test("预览：开关打开后，焦点行的内容真的上屏，语言名来自 core", async () => {
  Wfiles.reset();
  const dir = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFile(dir.path, "main.rs", "rust");
  const app = await open("./App.tsx");

  // 默认关，而且关着的时候一个字节内容都不该出现在屏上。
  expect(app.texts()).toContain("预览：关");
  expect(app.texts().some((text) => text.includes("Hello, world!"))).toBe(false);

  await app.press("preview-button");
  expect(app.texts()).toContain("预览：开");
  // 点一下那一行：焦点变了，预览跟着换。
  await app.press("id:row:main.rs >> nth=0");

  // 内容来自磁盘上那份文件（模板 `rust` 的内容），不是界面自己编的。
  expect(app.texts().some((text) => text.includes('println!("Hello, world!")'))).toBe(true);
  // 语言名与行数都由 core 认出来，界面不维护后缀表。
  expect(app.texts()).toContain("Rust · 3 行");
});

test("预览：图片只报类型，不去读它的内容", async () => {
  Wfiles.reset();
  const dir = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  // 内容是空文本、扩展名是图片：读了内容就会变成「0 行」，只有按扩展名判才是「图像」。
  Wfiles.createFile(dir.path, "photo.png", "plain");
  const app = await open("./App.tsx");
  expect(app.texts()).toContain("预览：关");

  await app.press("preview-button");
  await app.press("id:row:photo.png >> nth=0");
  expect(app.texts()).toContain("图像");
});

test("缩略图：core 说是图片，界面才把路径交给 Image 去解码", async () => {
  Wfiles.reset();
  const dir = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  // 真 SVG：16×16。用夹具直写而不是 `createFile`——那个后缀由模板定死（.txt/.md），
  // 造不出图片。
  Wfiles.writeFixture(
    dir.path,
    "logo.svg",
    '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#c00"/></svg>',
  );
  Wfiles.refresh();
  const app = await open("./App.tsx");

  await app.press("preview-button");
  await app.press("id:row:logo.svg >> nth=0");

  // 缩略图那块必须出现，且无障碍名点得出是哪个文件：**只有**
  // `media === "image"` 那一支会画它（media 来自 core 的 `Kind::id()`）。
  expect(app.shows("label:thumb:logo.svg")).toBe(true);
  // 类型也还是 core 认出来的那个（不是界面按后缀猜的）。
  expect(app.texts()).toContain("图像");
});

test("缩略图不该出现在文本文件上：分支靠 core 的类型，不靠后缀或猜测", async () => {
  Wfiles.reset();
  const dir = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFile(dir.path, "notes.txt", "plain");
  const app = await open("./App.tsx");

  await app.press("preview-button");
  await app.press("id:row:notes.txt >> nth=0");
  // 同一个位置、同一套流程，只因为 core 说这是文本而不是图片，就不该有 Image。
  expect(app.shows("label:thumb:notes.txt")).toBe(false);
});

// ---- 内容差异（两个文件比一比）----
//
// 判据落在**屏幕上那行字**与**core 拼的结论句**上：把 `TextStats::summary`
// 的措辞改掉会当场红（证明界面读的是 core，不是自己拼的）。

test("差异：两项一选就能比，标记与结论句都来自 core", async () => {
  Wfiles.reset();
  const dir = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.writeFixture(dir.path, "old.txt", "alpha\nbeta\ngamma\n");
  Wfiles.writeFixture(dir.path, "new.txt", "alpha\nBETA\ngamma\n");
  Wfiles.refresh();

  const app = await open("./App.tsx");
  await app.press("multi-button");
  await app.press("id:row:new.txt");
  await app.press("id:row:old.txt");

  // 选两项之前这颗按钮是灰的（`!` 表示 disabled），点不动也不该比。
  await app.press("pair-button");

  // 面板标题写清是谁跟谁：只写"差异"的话，用户不知道比的是哪两个。
  expect(app.texts().some((text) => text.includes("new.txt") && text.includes("old.txt"))).toBe(
    true,
  );
  // 结论句是 core 的 `TextStats::summary()`："改了 1 行"。
  expect(app.texts()).toContain("改了 1 行");
  // 改了的那行带 core 的标记「改」，两边内容都在同一行上。
  const changed = app.texts().find((text) => text.includes("BETA"));
  expect(changed).toBeDefined();
  expect(changed?.startsWith("改")).toBe(true);
  expect(changed?.includes("beta")).toBe(true);
});

test("差异：二进制按字节比，偏移与字节计数都来自 core", async () => {
  Wfiles.reset();
  const dir = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  // 带 NUL → core 判成二进制，走 diff_hex。内容不同在第二个字节。
  Wfiles.writeFixture(dir.path, "a.bin", "\u0000A\u0000B");
  Wfiles.writeFixture(dir.path, "b.bin", "\u0000A\u0000C");
  Wfiles.refresh();

  const app = await open("./App.tsx");
  await app.press("multi-button");
  await app.press("id:row:a.bin");
  await app.press("id:row:b.bin");
  await app.press("pair-button");

  // 十六进制那一栏是 `diff::hex_line` 出的（core 的排法），偏移按 0x 报。
  expect(app.texts().some((text) => text.includes("0x00000000"))).toBe(true);
  // 结论句 `HexStats::summary()`："N 个字节不同"。
  expect(app.texts().some((text) => text.includes("个字节不同"))).toBe(true);
});

test("比较：选两个目录就比目录树，差在哪由 core 说", async () => {
  Wfiles.reset();
  const outer = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFolder(outer.path, "left");
  Wfiles.createFolder(outer.path, "right");
  Wfiles.writeFixture(`${outer.path}/left`, "same.txt", "一样\n");
  Wfiles.writeFixture(`${outer.path}/right`, "same.txt", "一样\n");
  Wfiles.writeFixture(`${outer.path}/left`, "moved.txt", "左边有\n");
  Wfiles.writeFixture(`${outer.path}/right`, "extra.txt", "右边多出来\n");
  Wfiles.refresh();

  const app = await open("./App.tsx");
  await app.press("multi-button");
  await app.press("id:row:left");
  await app.press("id:row:right");

  // 同一个按钮：选中的是目录就走 compare（不是 diff——那两个目录不是"文件内容"）。
  await app.press("pair-button");

  expect(app.texts().some((text) => text.startsWith("比较："))).toBe(true);
  // 结论句由 `Stats::summary()` 出：只有左 1 项 + 只有右 1 项，一样的那项只计数。
  const summary = app.texts().find((text) => text.includes("只有左") && text.includes("只有右"));
  expect(summary).toBeDefined();
  expect(summary).toContain("另有 1 项一样");
  // 差异行的标记与相对路径都是 core 给的。
  expect(app.texts().some((text) => text.includes("moved.txt"))).toBe(true);
  expect(app.texts().some((text) => text.includes("extra.txt"))).toBe(true);
});

test("差异：只选一项时按钮不可点，也不会把面板打开", async () => {
  Wfiles.reset();
  const dir = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFile(dir.path, "only.txt", "plain");
  const app = await open("./App.tsx");
  await app.press("multi-button");
  await app.press("id:row:only.txt");
  // disabled 的 chip 点不到；面板不该出现（否则就是把"可比"的门槛丢了）。
  expect(app.shows("id:diff-pane")).toBe(false);
});

// ---- 收藏 ----
//
// 数据归 **core**（`wfiles_core::favorites`，与 GUI 同一份 settings.json）。
// 判据两头都钉：屏幕上那份（面板里数得出来）与 core 里那份（`Wfiles.favorites()`），
// 少一头就会出现"界面看着有、其实没存"或者反过来。

test("收藏：收藏当前目录，界面与 core 同时有", async () => {
  Wfiles.reset();
  const outer = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFolder(outer.path, "项目A");
  Wfiles.refresh();
  const target = `${outer.path}/项目A`;
  Wfiles.enter(target);

  const app = await open("./App.tsx");
  await app.press("disks-button");
  // 收藏与卷在同一个面板里（都是"去哪儿"）；一开始是空的。
  expect(app.texts()).toContain("收藏（0）");
  expect(app.texts()).toContain("还没有收藏");

  await app.press("favorite-here");

  expect(app.texts()).toContain("收藏（1）");
  expect(app.texts()).toContain("项目A");
  // 按钮跟着变成"取消收藏"——文案由"在不在收藏里"推，不是另存的一个开关。
  expect(app.texts()).toContain("取消收藏");

  // core 那份也要真有：只改信号不改文件，就是界面自己演了一遍。
  const stored = JSON.parse(Wfiles.favorites()) as { path: string; is_dir: boolean }[];
  expect(stored.length).toBe(1);
  expect(stored[0].path).toBe(target);
  expect(stored[0].is_dir).toBe(true);
});

test("收藏：点一下真的进那个目录，再点一次就取消", async () => {
  Wfiles.reset();
  const outer = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFolder(outer.path, "项目A");
  Wfiles.createFile(`${outer.path}/项目A`, "inside.txt", "plain");
  Wfiles.refresh();

  const target = `${outer.path}/项目A`;
  // **先进目录再开界面**：`Wfiles.enter` 改的是 core，界面的信号不会自己跟过去，
  // 反过来写就会"人站在项目A里、收藏的却是外层目录"——这条判据第一次就是这么红的。
  Wfiles.enter(target);
  const app = await open("./App.tsx");
  await app.press("disks-button");
  await app.press("favorite-here");

  // 走到外面，再从收藏栏点回去：屏上要出现只有里面才有的那个文件。
  Wfiles.enter(outer.path);
  await app.press("refresh-button");
  await app.press("id:favorite-row:0");
  await app.waitUntil(() => app.texts().includes("inside.txt"));
  expect(app.texts()).toContain("inside.txt");

  // 再点一次按钮＝取消收藏（按钮文案已经变成"取消收藏"，但 id 不变）；core 那份也要空掉。
  await app.press("id:favorite-here");
  expect(JSON.parse(Wfiles.favorites()).length).toBe(0);
  expect(app.texts()).toContain("收藏（0）");
});

// ---- 远程（SFTP）----
//
// 这两条**只能验失败路径**：跑判据的机器上没有 SFTP 服务器，"连上之后列目录、
// 下载"在这里没法真跑。所以钉的是另外两件事——**错误不许被吞**、**没连上不许
// 假装成功**。成功路径靠 core 自己的单测（`sftp.rs` 里有 russh 的 server 夹具）。

test("远程：连不上就把原因原样说出来，不假装连上了", async () => {
  Wfiles.reset();
  Wfiles.open(Wfiles.tempDir());
  const app = await open("./App.tsx");
  await app.press("disks-button");

  // 端口 1 上不会有服务：立刻被拒，不用等 60 秒超时。
  await app.type("remote-url", "sftp://127.0.0.1:1/");
  await app.type("remote-user", "nobody");
  await app.type("remote-password", "nope");
  await app.press("remote-connect");

  await app.waitUntil(() => app.texts().some((text) => text.includes("连不上")));
  const line = app.texts().find((text) => text.includes("连不上")) ?? "";
  // core 的错里带主机和端口；只说"连不上"排不了障，这条就是防它退化成三个字。
  expect(line).toContain("127.0.0.1");
  expect(line).toContain("1");
});

test("远程：没连上时列目录与下载都如实拒绝，不返回空列表冒充成功", () => {
  Wfiles.reset();
  Wfiles.sftpDisconnect();

  const listed = JSON.parse(Wfiles.sftpList("")) as { ok: boolean; error?: string };
  expect(listed.ok).toBe(false);
  expect(listed.error).toContain("还没连上");

  // 没有会话时连本地文件都不该被创建出来——半个文件比报错更难查。
  const dest = `${Wfiles.tempDir()}/never.txt`;
  const downloaded = JSON.parse(Wfiles.sftpDownload("a.txt", dest)) as { ok: boolean };
  expect(downloaded.ok).toBe(false);
});

// ---- 磁盘（卷 + 容量）----
//
// `Wfiles.volumes()` 是 core 的**只读查询**（跟 preview 同类）。夹具走的是 core 的
// `volumes::set_override`，不是桥接层另存的一份：把 `detected()` 那行摘掉会当场红。
// 判据落在**那行字本身**与**点了真的换目录**——只断言「磁盘面板出现了」是挑错层。

type VolumeJson = {
  name: string;
  path: string;
  capacity_text: string | null;
};

/** 钉两块盘：一块报得出容量，一块报不出。第一块里放一个只有进去了才看得见的文件。 */
function diskFixture(): string {
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  const backup = `${temp.path}/Backup`;
  Wfiles.createFolder(temp.path, "Backup");
  Wfiles.createFile(backup, "marker.txt", "plain");
  const cd = `${temp.path}/CD`;
  Wfiles.createFolder(temp.path, "CD");
  const gb = 1024 * 1024 * 1024;
  return JSON.stringify([
    { name: "备份盘", path: backup, capacity: { total: 500 * gb, free: 120 * gb } },
    { name: "光盘", path: cd, capacity: null },
  ]);
}

test("磁盘面板列出 core 的卷，容量那行字逐字来自 core", async () => {
  Wfiles.reset();
  Wfiles.setTestVolumes(diskFixture());

  // 先看桥：文案是 core 的 `Capacity::describe()` 拼的，界面不许自己再拼一遍。
  const fromCore = JSON.parse(Wfiles.volumes()) as VolumeJson[];
  expect(fromCore.length).toBe(2);
  expect(fromCore[0].capacity_text).toBe("可用 120.0 GB / 共 500.0 GB");
  expect(fromCore[1].capacity_text).toBe(null);

  const app = await open("./App.tsx");
  // 默认不占列表高度；开了才画。
  expect(app.texts()).not.toContain("备份盘");
  await app.press("disks-button");
  expect(app.texts()).toContain("备份盘");
  expect(app.texts()).toContain("光盘");
  expect(app.texts()).toContain("可用 120.0 GB / 共 500.0 GB");
  // 探不出来的那块：**容量文案总共只该出现一条**，更不许编一个 0 出来。
  //
  // 别用 `app.shows("disk-capacity:1")` 判"没画那一行"——headless 里一个
  // 空文本的 Text 同样报 `shows: false`，于是"无条件画了个空行"照样绿，
  // 判据就落在了错误的一层上。数文案本身才抓得住。
  expect(app.shows("id:disk-capacity:0")).toBe(true);
  expect(app.texts().filter((text) => text.startsWith("可用 ")).length).toBe(1);
  expect(app.texts().some((text) => text.includes("可用 0 B"))).toBe(false);
});

test("点磁盘那一行真的进到挂载点，不是只画了个框", async () => {
  Wfiles.reset();
  Wfiles.setTestVolumes(diskFixture());
  const app = await open("./App.tsx");
  await app.press("disks-button");
  await app.press("id:disk-row:0");
  // 这个文件只在 Backup 里：上屏就说明 `Enter(path)` 真的走通了。
  expect(app.texts()).toContain("marker.txt");
});

// ---- 搜索 ----
//
// 结果在 core 那边是流式来的，界面只能轮询（NSH 的原生模块没有"Rust 推给 JS"
// 的路子）。这几条钉的是「输入 → 起搜 → 结果上屏」这条链：
// 判据落在**命中项的名字与行号**上，光看「搜索中…」变成了「没有命中」是挑错层。

/** 一个含子目录的夹具：`alpha` 在两层里各有一个。 */
function searchFixture(): string {
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFile(temp.path, "alpha.txt", "plain");
  Wfiles.createFile(temp.path, "beta.txt", "plain");
  Wfiles.createFolder(temp.path, "sub");
  Wfiles.createFile(`${temp.path}/sub`, "alpha-two.txt", "plain");
  return temp.path;
}

test("搜索：子目录里的命中也会出现在结果里", async () => {
  Wfiles.reset();
  searchFixture();
  const app = await open("./App.tsx");

  await app.press("search-button");
  await app.type("search-input", "alpha");
  await app.press("search-start");

  // 轮询是 100 毫秒一次，等结果上屏而不是睡固定时长。
  await app.waitUntil(() => app.texts().includes("alpha-two.txt"));
  expect(app.texts()).toContain("alpha.txt");
  // 状态栏右侧在搜索态改报命中条数（不是「N / M 个项目」）。
  expect(app.texts()).toContain("命中 2 条");

  // 桥接层那边的账也要对得上：只看界面的话，"命中了 2 条" 可能是蒙的。
  const state = JSON.parse(Wfiles.searchState());
  expect(state.done).toBe(true);
  expect(state.matched).toBe(2);
  expect(state.cancelled).toBe(false);
});

test("内容搜索：结果带行号与那一行原文", async () => {
  Wfiles.reset();
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  // markdown 模板的内容是「# 标题\n\n」，第一行就是它——不另造写文件的口子。
  Wfiles.createFile(temp.path, "notes.md", "markdown");

  const app = await open("./App.tsx");
  await app.press("search-button");
  await app.type("search-input", "content:标题");
  await app.press("search-start");

  await app.waitUntil(() => app.texts().some((text) => text.includes("1: # 标题")));
  expect(app.texts()).toContain("notes.md");
});

test("搜完可以关掉，回到目录列表", async () => {
  Wfiles.reset();
  Wfiles.open(searchFixture());
  const app = await open("./App.tsx");

  await app.press("search-button");
  await app.type("search-input", "alpha");
  await app.press("search-start");
  await app.waitUntil(() => app.texts().includes("alpha-two.txt"));

  await app.press("search-close");
  // 回到目录清单：右侧恢复「N / M」，命中列表不在了。
  expect(app.texts()).toContain("3 / 3");
  expect(app.texts()).not.toContain("命中 2 条");
});

// ---- 搬运进度与中止 ----
//
// 进度来自 core 的 `KernelEvent::Progress`（每搬完一项、每写完一个数据块一次），
// 桥接层把它攒成一份 JSON，界面每 100 毫秒问一次——NSH 的原生模块只有方法、
// 没有"Rust 主动推给 JS"的路子，只能轮询。
//
// 三条测试上的讲究（都踩过）：
// ① `press` 会等到界面**完全静止**才返回，小搬运在它返回前就结束了，
//    所以"进度行正在显示"必须用搬得够久的夹具（400 个文件）才观察得到。
// ② `waitUntil` 只在界面有变化时重算条件。条件写成"Rust 侧某个值变了"时，
//    搬运结束后没有重绘，它会一直等到超时——判据要挂在界面状态上（进度行消失），
//    值的断言放在那之后再直接读。
// ③ 换目录用 `Wfiles.enter` + 刷新按钮，不点列表行：虚拟列表会把旧格子留在树里，
//    `row:dest` 可能匹配到已经不在屏幕上的那一份。

/** 造一个「400 个文件 + 一个目标目录」的夹具，够搬一会儿。 */
function bigFixture(): { root: string; src: string; dest: string } {
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  const src = `${temp.path}/src`;
  Wfiles.createFolder(temp.path, "src");
  for (let i = 0; i < 400; i += 1) {
    Wfiles.createFile(src, `f${i}.txt`, "plain");
  }
  Wfiles.createFolder(temp.path, "dest");
  return { root: temp.path, src, dest: `${temp.path}/dest` };
}

/** 把 src 全选复制到 dest，但**不粘贴**——粘贴那一刻由调用方掌握。 */
async function copyAllInto(app: Awaited<ReturnType<typeof open>>, dest: string) {
  await app.press("multi-button");
  await app.press("select-all-button");
  await app.press("copy-button");
  await app.press("leave-multi-button");
  Wfiles.enter(dest);
  await app.press("refresh-button");
}

test("没有搬运时：进度行不出现，进度查询是空", async () => {
  Wfiles.reset();
  Wfiles.open(Wfiles.tempDir());
  const app = await open("./App.tsx");

  expect(app.shows("progress-row")).toBe(false);
  // 桥接层对"没有活"的回答就是 JSON 里的 null，界面据此不画那一行。
  expect(Wfiles.progress()).toBe("null");
});

test("搬运时出现进度行与中止按钮，搬完就撤掉", async () => {
  Wfiles.reset();
  const fixture = bigFixture();
  const app = await open("./App.tsx");
  await app.press("id:row:src");
  await copyAllInto(app, fixture.dest);

  await app.press("paste-button");
  // 进度行有「最少露脸 400 毫秒」的规则（见 App.tsx 的 `MIN_VISIBLE_MS`），
  // 所以哪怕这 400 个文件在 `press` 返回前就搬完了，这一行此刻也一定还在。
  expect(app.shows("progress-row")).toBe(true);
  expect(app.texts()).toContain("中止");

  await app.waitUntil(() => !app.shows("progress-row"));
  expect(Wfiles.progress()).toBe("null");
});

test("搬运中途叫停：真的少搬了，且说清是中止的", async () => {
  Wfiles.reset();
  const fixture = bigFixture();
  const app = await open("./App.tsx");

  // 这一条**不走界面按钮**：`press` 会一直等到界面静止，搬运早跑完了，
  // 那时候再叫停等于测"搬完之后点中止"。所以直接在桥接层发起、不 await。
  const sources = Array.from({ length: 400 }, (_, i) => `${fixture.src}/f${i}.txt`);
  const running = Wfiles.copyAsync(sources, fixture.dest);

  // 等它真的开动（工作线程要先起线程、数一遍总量）。`wait` 让出一帧。
  let sawProgress = false;
  for (let i = 0; i < 200 && !sawProgress; i += 1) {
    await app.wait(1);
    sawProgress = Wfiles.progress() !== "null";
  }
  expect(sawProgress).toBe(true);

  Wfiles.abort();
  const result = JSON.parse(await running);

  // ① 中止位真的被搬运线程看见了：目标目录里的条目数少于源。
  const copied = JSON.parse(Wfiles.open(fixture.dest)).total as number;
  expect(copied).toBeLessThan(400);
  // ② 而且说清了是"中止"，不是悄悄少搬。
  expect(
    (result.messages as { text?: string }[]).some((m) => (m.text ?? "").includes("中止")),
  ).toBe(true);
  // ③ 进度收干净了，界面不会挂着一条不动的进度条。
  expect(Wfiles.progress()).toBe("null");
});

// ---- 「撤销什么」----
//
// core 的 `History::last_describe()` 早就在给 GUI 的撤销按钮写「撤销 XXX」，
// 但 NSH 的按钮只写「撤销」——按下去之前不知道会撤掉什么。
// 判据落在**那句文案本身**：只验"按钮能点"是挑错层。

test("撤销按钮说清下一步会撤掉什么", async () => {
  Wfiles.reset();
  Wfiles.open(Wfiles.tempDir());
  const app = await open("./App.tsx");
  // 空栈：按钮还在，但不该编出一句描述。
  expect(app.texts()).toContain("撤销");
  expect(app.texts().some((text) => text.startsWith("撤销："))).toBe(false);

  await app.press("new-folder-button");
  await app.press("prompt-ok");
  // 逐字来自 core 的 `HistoryEntry::describe`，界面不许自己拼。
  expect(app.texts()).toContain("撤销：新建“未命名文件夹”");

  await app.press("undo-button");
  await app.waitUntil(() => !app.texts().some((text) => text.startsWith("撤销：")));
  expect(app.texts()).toContain("撤销");
});

test("撤销标签跟着操作类型变：复制两项就写复制 2 项", async () => {
  Wfiles.reset();
  const temp = JSON.parse(Wfiles.open(Wfiles.tempDir()));
  Wfiles.createFile(temp.path, "a.txt", "plain");
  Wfiles.createFile(temp.path, "b.txt", "plain");
  Wfiles.createFolder(temp.path, "dest");

  const app = await open("./App.tsx");
  await app.press("multi-button");
  await app.press("id:row:a.txt");
  await app.press("id:row:b.txt");
  await app.press("copy-button");
  await app.press("leave-multi-button");
  await app.press("id:row:dest >> nth=0");
  await app.press("paste-button");
  // 写死「新建…」的实现在这里会一直等不到。
  await app.waitUntil(() =>
    app.texts().some((text) => text.includes("撤销：复制 2 项")),
  );
  expect(app.texts()).toContain("撤销：复制 2 项");
});

test("长撤销标签截短上屏，完整描述留在无障碍名里", async () => {
  Wfiles.reset();
  Wfiles.open(Wfiles.tempDir());
  const long = "长".repeat(60);
  const app = await open("./App.tsx");
  await app.press("new-folder-button");
  await app.type("prompt-input", long);
  // 读输入框里真正的值，别猜 `type` 是追加还是替换。
  const name = app.value("prompt-input") ?? "";
  await app.press("prompt-ok");

  // 屏上那句要短：完整 60 多字的名字不该把地址栏撑开。
  const shown = app.texts().find((text) => text.startsWith("撤销：")) ?? "";
  expect(shown.length).toBeLessThanOrEqual(28);
  expect(shown.endsWith("…")).toBe(true);
  // 完整描述没丢：读屏与判据都从 a11y 拿得到（GUI 版是留在 tooltip 里）。
  expect(app.shows(`label:撤销：新建“${name}”`)).toBe(true);
  // 这里**故意不写**「其它按钮还在吗」：实测过，把截断摘掉（按钮真的变成
  // 73 字）时 `app.shows("multi-button")` 照样是 true——地址栏的 flex-1
  // 路径区会自己让位，headless 量不到这种挤出。写进来就是一条永远绿的假判据。
});
