# WFiles 界面重设计方案 v1

> **依据（全部实测，非推测）**
> - 现有 UI：`src/` 下 22 个界面/面板（清单见 §5）
> - 组件能力：`gpui-kit 0.7.1` → `component` 层（`gpui-component 0.7.1`）+ `assets` 层（`gpui-kit-assets 0.7.1`）逐条读源码核对
> - 设计令牌：`src/theme/default-theme.json`（shadcn neutral，99 个颜色令牌）+ `Theme` 结构非颜色字段
> - 图标：`assets/icons` 共 **1830** 个（Lucide + GPUI Kit），`default-icons.txt` 决定默认打包 **106** 个
> - 范围：只做 UI 形态与行为规格，**本文不含代码改动**；实施批次见 §8。与 `ROADMAP.md` 的 P5 项（原生菜单/通知/无障碍/i18n）对齐，不碰"明确不做"清单

---

## 0. 结论摘要

**一句话**：结构不用推翻，**"每个面板各写一套"要推翻**。

| 判断 | 说明 |
|---|---|
| 骨架是对的 | 标签栏 / 侧栏 / 双栏窗格 / 详情板 / 状态栏 / 底部终端，这套结构对齐 Finder、Explorer、Dolphin，改动价值低、风险高——**保留** |
| 真问题在重复实现 | 6 处遮罩对话框各写一遍"绝对定位遮罩 + 卡片 + Esc + stop_propagation"；工具栏平铺 12 颗图标按钮；几何常量散落在 8 个文件顶部；45 个图标里 19 个没打包 |
| 重设计主线 | **手写 → 组件**。组件复用从 **6 类 → 20 类**，遮罩卡片收敛成 1 套 Dialog，几何收敛成 1 份令牌 |
| 唯一结构性前置 | `WFilesApp` 没包在 `Root` 里；Dialog / Notification / Sheet 依赖 `Root` 的 `dialog_layer` / `notification_layer`。不补这步，新对话框与通知**不会显示**（见 §8 批次 0） |
| 一个实测缺陷 | **19 个在用图标没打进二进制**（`Trash`/`Terminal`/`Command`/`Cloud`/`Download`/`Scissors`/`FolderPlus`/`SquarePen`/`X`…），现在渲染为空白。证据与修法见 §6.4 |

### 变更摘要

| 维度 | 现状 | 新设计 | 收益 |
|---|---|---|---|
| 组件复用 | 6 类（Button / Input / ListItem / Icon / v_virtual_list / scrollbar） | **20 类**（+ TitleBar / Sidebar / TabBar / Dialog / AlertDialog / Command / Breadcrumb / Kbd / Progress / StatusBar / Empty / Spinner / Skeleton / Tag·Badge / Popover·PopupMenu / Sheet / Resizable / Notification / ContextMenu / NativeMenu） | 少写 1500+ 行骨架代码，交互一致性由组件保证 |
| 顶部 | 自绘标签栏 38px，与系统窗口控件各管各的 | TitleBar 34px 承载标签（Segmented）+ 同排窗口控件 | macOS 三色灯与内容同一行，三平台统一 |
| 侧栏 | 固定 210px，不可折叠，`ListItem` 平铺 | Sidebar 255px（折叠 48px，200ms 动画）+ 分组（位置/磁盘/云盘/远程）+ Footer | 折叠腾出内容宽度；分组标题语义正确 |
| 工具栏 | 12 颗图标按钮平铺，按选中数置灰 | **5 常显 + 1「更多」下拉**；视图切换用 Segmented | 按钮数 12 → 6，认知负担与误点率下降 |
| 选中态 | 仅背景色 `muted` | 背景 + **左侧 2px primary 竖条** + `ring` 焦点环 | 键盘/鼠标双输入的焦点可见性（无障碍硬要求） |
| 对话框 | 手写遮罩卡片 ×6 | `open_dialog` / `open_alert_dialog` 一套 + 0.25s 进出动画 | 行为统一：Esc、点遮罩、初始焦点、Tab 环 |
| 详情板 | 固定 260px | 可拖拽 220–420px，可整体收起 | 窄屏可用；预览可放大 |
| 终端 | 固定 240px | `v_resizable` 可拖高（≥120px） | 看长输出不用全屏切走 |
| 传输 | 状态栏单行文本 | 状态栏摘要 + **传输中心 Sheet**（多任务列表） | 并发任务可见、可逐项暂停/重试 |
| 完成反馈 | 状态栏一行字 | **Notification**（右上角）+ 通知内"撤销"按钮 | "可撤销"卖点变成可见的一等公民 |
| 进度条 | 自绘两层 div | `Progress`（XSmall = 4px，与原尺寸一致） | 组件化，含不确定态 |
| 标记色 | 7 个硬编码 hex（`marks::color_hex`） | **6 个主题令牌**（`base.*`），深浅色自适应 | 深色模式下标记不再刺眼 |
| 几何 | 各文件顶部硬编码常量 | 一份令牌：密度三档 + 组件高度表（§2.2） | 改一处全局一致 |
| 字号 | 跟随主题默认 16px | 13 正文 / 12 元数据 / 11 徽标；mono 13 | 列表信息密度提升约 20% |
| 图标 | 45 个在用，19 个未打包（空白） | 收敛为 46 个用途明确的图标 + 打包清单显式化 + **CI 差集校验** | 图标不再"凭印象写名字" |

---

## 1. 设计原则

1. **文件是主角，界面是框**。chrome（标题栏+工具栏+状态栏）总高压到 **100px 以内**（现状 124px），屏幕越大越还给内容。
2. **层级靠色调与 1px 边线，不靠阴影**。三段表面：`sidebar.background`（#fafafa）→ `background`（#fff）→ `popover.background`（浮层，可带阴影）。阴影只允许出现在 Dialog / Popover / Menu / Notification 上。
3. **颜色只表达语义，不表达装饰**。danger 只给不可逆动作，warning 只给"需要你看一眼"（差异、半截结果、未保存），primary 只给"主操作/进度/选中"，success/info 只给状态反馈。标记色是唯一允许"为分类而用色"的地方。
4. **一切都能被打断、被撤销、被看见**。长任务必在传输中心可见；撤销入口出现在两个地方（`Cmd+Z` + 完成通知里的按钮），而非只藏在状态栏右端。
5. **三平台同形不等形**。菜单用原生（macOS/Windows 真原生，Linux 自绘），窗口控件位置随平台，其余视觉规范三平台同一套。

---

## 2. 设计令牌

### 2.1 颜色：界面角色 → 真实令牌

全部落在 `gpui-component` 已定义的 99 个令牌上，**不新增色值**；浅/深色由主题自动切换。

| 界面角色 | 令牌 | Light 实测值 | Dark 实测值 |
|---|---|---|---|
| 窗格底 / 列表底 | `background` / `list.background` | `#ffffff` | `#0a0a0a` |
| 侧栏 | `sidebar.background` / `sidebar.foreground` | `#fafafa` / `#171717` | `#0a0a0a` / `#f5f5f5` |
| 侧栏当前项 | `sidebar.accent.background` / `.foreground` | `#e5e5e5` / `#171717` | `#262626` / `#f5f5f5` |
| 侧栏/窗格分隔 | `sidebar.border` / `border` | `#e5e5e5` | `#262626` |
| 标签栏底 | `tab_bar.background` | `#f5f5f5` | `#171717` |
| 标签（活动） | `tab.active.background` / `.foreground` | `#ffffff` / `#171717` | `#0a0a0a` / `#fafafa` |
| 标题栏 | `title_bar.background` / `.border` | `#f8f8f8` / `#e5e5e5` | `#171717` / `#262626` |
| 状态栏 | `status_bar.background` / `.border` | `#f8f8f8` / `#e5e5e5` | `#171717` / `#262626` |
| 行悬停 | `list.hover.background` | `#f5f5f5` | 深色由主题补 |
| 行选中 | `list.active.background` + **`list.active.border` 左竖条** | `#bfdbfe33` + `#60a5fa` | `#1e40af33` + `#1d4ed8` |
| 表头 | `list.head.background` | `#fafafa` | `#17171766` |
| 多选高亮 | `selection.background` | `#55a0fc` | `#1d4ed8` |
| 焦点环 | `ring` | `#a3a3a3` | `#737373` |
| 主操作 / 进度 | `primary.background` / `progress_bar.background` | `#171717` | `#f5f5f5` |
| 危险（删除、覆盖、永久删除） | `danger.background` / `.foreground` | `#ef4444` / `#fafafa` | `#f87171` / `#dc2626` |
| 警告（差异、半截、未保存） | `warning.background` / `.foreground` | `#eab308` / `#fafafa` | `#facc15` / `#ca8a04` |
| 成功（完成、已同步） | `success.background` / `.foreground` | `#22c55e` / `#fafafa` | `#4ade80` / `#16a34a` |
| 信息（连接成功、离线提示） | `info.background` / `.foreground` | `#06b6d4` / `#fafafa` | `#22d3ee` / `#0891b2` |
| 遮罩 | `overlay` | `#0000000d` | `#00000033` |
| 拖拽悬停落点 | `drop_target.background` + `drag_border` | `#3b82f640` + `#3b82f6` | `#3b82f619` + `#3b82f6` |
| 滚动条 | `scrollbar.thumb.background` / `:hover` | `#a3a3a3e6` / `#a3a3a3` | `#525252e6` / `#525252` |
| 次要文字 | `muted.foreground` | `#737373` | `#a3a3a3` |
| 次级面（分组底、Kbd 底） | `muted.background` | `#f5f5f5` | `#262626` |
| 骨架屏 | `skeleton.background` | `#f5f5f5` | `#171717` |

**文件标记色：硬编码 7 色 → 6 个语义令牌**（`marks.rs::color_hex` 的迁移映射）

| 新标记 | 令牌（浅） | Light | 用途建议 | 旧 id 兼容 |
|---|---|---|---|---|
| 红 | `base.red` | `#ef4444` | 重要 / 待处理 | red |
| 黄 | `base.yellow` | `#eab308` | 待确认 | yellow、orange |
| 绿 | `base.green` | `#22c55e` | 已归档 | green |
| 青 | `base.cyan` | `#06b6d4` | 参考 | cyan、teal |
| 蓝 | `base.blue` | `#3b82f6` | 进行中 | blue |
| 紫 | `base.magenta` | `#d946ef` | 个人 | magenta、purple |

> 深色模式取各家族的 `-400` 档（如 `base.red.light` = `#f87171`），令牌层已存在。**旧数据迁移**：7 个旧 id → 6 个新 id 的映射写在 `settings` 迁移里，读旧值不报错、写回新值。

### 2.2 几何：密度三档 + 高度表

新增 `tokens::Density`（设置项，默认**标准**），所有行高、内边距、缩略图尺寸由它派生：

| 档位 | 列表行高 | 网格格 | 内边距 | 缩略图（行内/网格） | 适用 |
|---|---|---|---|---|---|
| 舒适 | 44px | 132×140 | 12px | 28 / 72 | 触控、演示 |
| **标准（默认）** | **34px** | **112×124** | **8px** | **20 / 64** | 日常（现状 40px → 收紧 15%） |
| 紧凑 | 26px | 96×104 | 4px | 16 / 48 | 大目录扫读 |

固定高度表（不再允许各文件自定义）：

| 部件 | 高度 | 来源 |
|---|---|---|
| 标题栏（含标签） | 34px | 组件常量 `TITLE_BAR_HEIGHT = 34` |
| 工具栏 | 40px | 本方案（现状 52 → 40） |
| 表头 | 30px | 本方案（与行高解耦，排序箭头不挤压行） |
| 详情板 | 宽 220–420（默认 280） | 本方案（现状固定 260） |
| 侧栏 | 宽 255（折叠 48） | 组件常量 `DEFAULT_WIDTH = 255` |
| 状态栏 | 28px | 本方案（现状 34） |
| 终端面板 | 120–480（默认 240），可拖 | 本方案（现状固定 240） |
| 命令面板 | 宽 600，结果区 max 380 | 沿用现状 |
| 对话框宽度 | 小 420 / 中 560 / 大 860 / 超宽 980 | 沿用现状（对齐组件默认 448 的中档） |
| 圆角 | 控件 `radius`=6，卡片 `radius_lg`=8，色点/徽标 pill | 主题字段默认值 |

### 2.3 字体

| 角色 | 字号 | 字重 | 颜色 | 用例 |
|---|---|---|---|---|
| 微标 | 11 | 500 | `muted.foreground` | Kbd、徽标计数、状态标签 |
| 元数据 | 12 | 400 | `muted.foreground` | 大小/时间/类型列、路径、说明 |
| 正文 | **13** | 400 | `foreground` | 文件名、列表主体、输入框、菜单项 |
| 强调 | 13 | 500 | `foreground` | 当前目录段、活动标签、面板标题 |
| 面板标题 | 15 | 600 | `foreground` | 对话框/面板标题 |
| 空态标题 | 17 | 600 | `foreground` | `Empty` 的 title |
| 等宽 | 13（mono） | 400 | `foreground` | 终端、diff、hex、校验和 |

> 落地方式：`Theme.font_size = 13`、`mono_font_size = 13`（组件默认 16 / 13，16 对文件管理器过大）。

### 2.4 动效

| 场景 | 时长/曲线 | 令牌 |
|---|---|---|
| 悬停、按钮态 | 120ms | `motion.fast` |
| 对话框/面板进出 | 180–250ms | `motion.normal` / Dialog 自带 250ms |
| 侧栏折叠、抽屉 | 200ms；标签指示器 280ms 弹簧 | `spring_control` / `spring_move` |
| Sheet 滑出 | 280ms | `motion.slow` |
| 通知进出 | 400 / 200ms | 组件常量 |

**禁止动画**：列表滚动、筛选重排、缩略图出现（直接淡入 ≤120ms 或不做）。

### 2.5 图标规范

| 项 | 规则 |
|---|---|
| 来源 | 只用 `assets/icons` 里的 1830 个（Lucide + GPUI Kit）。**禁止凭印象写名字**，落到代码前必须核对文件存在 |
| 尺寸 | **12**（徽标内、Kbd 旁）/ **16**（默认：按钮、列表行、侧栏、菜单）/ **20**（工具栏主操作、详情板标题）/ **32**（面板空态）/ **48**（预览占位） |
| 颜色 | 一律继承文字色（`svg` 用 `text_color`）。默认 `muted.foreground`（未激活）/ `foreground`（可点/激活）/ `primary.background` 反色（选中）/ `danger.background`（危险动作）。**唯一例外**：文件标记色点（`Tag` / `Badge`） |
| 描边 | 保持 Lucide 原始 `stroke-width="2"`，不改路径 |
| 交互 | 图标按钮必须有 `aria_label`；纯图标按钮必须挂 `Tooltip`（文案 + Kbd 徽标） |
| 打包 | 显式清单（§6.4）；新增图标**必须同时**加进清单，并由测试守住 |
| 命名 | `IconName` 变体 = svg 文件名去连字符后 PascalCase（`folder-open.svg` → `IconName::FolderOpen`） |

---

## 3. 布局骨架

```
┌─────────────────────────────────────────────────────────────────────┐
│ TitleBar 34  ⬤⬤⬤  │ [标签1 ×][标签2 ×][+]  │ 分栏 同步 搜索 命令   │  ← 组件 TitleBar + TabBar
├──────────┬──────────────────────────────────────────────────────────┤
│ Sidebar  │ Toolbar 40   ◀ ▶ ▲ │ …/文稿/WFiles │ 新建 改名 删 复制 粘贴 更多 │ ☰▦▥ │ 筛选 │
│  255     ├──────────────────────────────────────────────────────────┤
│ (折叠 48)│ 表头 30   名称 ⇅ │ Git │ 大小 ⇅ │ 修改时间 ⇅ │ 类型 │
│          ├──────────────────────────────────────────────────────────┤
│ 位置      │ 行 34      📁 项目        ⎇main   —      09:41    文件夹   │  ← v_virtual_list
│ 磁盘      │ 行 34  ▪   📄 设计稿.md   ⎇main   12 KB  昨天 17:02 文本  │  ← 选中：底色+左竖条
│ 云盘      │ 行 34      🖼 封面.png      —      820 KB 10-11    PNG    │
│ 远程      ├──────────────────────────────────────────────────────────┤
│          │ 终端面板 120–480（可拖高，刻意不跟主题）                    │  ← v_resizable
│ 本地文件  ├──────────────────────────────────────────────────────────┤
│ 系统      │ StatusBar 28  复制 3 项 42.7/128 MB 8.2 MB/s 剩 12s ▓▓▓░ │ ⏸ ✕ 2 个远程编辑中 撤销 │
└──────────┴──────────────────────────────────────────────────────────┘
                    浮层（同一时刻最多一层）：命令面板 / 对话框 / Sheet / 传输中心
```

**双栏模式**：`h_resizable("panes")` 两个 `resizable_panel()`，分隔条 3px 拖拽热区，焦点栏顶部 1px `primary` 线（沿用现状视觉，改由组件提供拖拽与 clamp）。

**层级规则（z 序，从下到上）**：内容 → 拖拽预览卡 → 传输中心 Sheet → 对话框（Dialog/AlertDialog）→ 命令面板 → 通知（右上角，不阻塞）。同一时刻**只允许一层模态**；通知可与模态并存。

---

## 4. 交互流程（6 条主干）

### 4.1 复制 / 移动（含冲突与撤销）

| 步骤 | 界面行为 |
|---|---|
| 1 | 列表选中 N 项（Shift 范围、Cmd 点选），选中态 = `list.active.background` + 左竖条；状态栏左侧显示「已选 N 项 · 合计 X MB」 |
| 2 | `Cmd+C` / `Cmd+X` → 行图标叠加 12px 徽标（`Copy` / `Scissors` 小图标，`muted.foreground`）；状态栏提示「已复制 N 项」 |
| 3 | 导航到目标目录（标签、侧栏、面包屑、新建标签皆可）→ `Cmd+V` |
| 4 | 若同名 → **AlertDialog「冲突」**：标题「3 个同名项目」；每个冲突行展示 **来源 → 目标**（大小、修改时间差异，`warning` 高亮不同项）；四策略按钮：`保留两者`(primary，默认焦点) / `替换`(danger) / `跳过`(secondary) / `取消`(ghost)；底部 `Switch`「应用到剩余全部项目」 |
| 5 | 执行 → 状态栏出现 Progress（4px）+ 名称 + 字节 + 速度 + 剩余时间；**传输中心 Sheet** 同步列出该任务及其他任务 |
| 6 | 完成 → 右上角 Notification：「已复制 3 项到 …」+ 按钮 **撤销**；状态栏 5s 后回到空闲文案 |
| 7 | `Cmd+Z` 或点通知里「撤销」→ 撤销确认框（危险动作二次确认）→ 走入回收站恢复链路 |

### 4.2 拖放

| 步骤 | 界面行为 |
|---|---|
| 1 | 拖起（阈值 4px）→ 跟随光标出现 **DragPreview 卡**：最左 3 个缩略图叠放 + 「N 项 · X MB」 |
| 2 | 悬停合法落点 → 该行/格子 `drop_target.background` 铺底 + 1px `drag_border` 描边 |
| 3 | 悬停文件夹 **700ms** → 弹簧展开进入（`spring_load` 现有逻辑保留）；展开中不重复计时 |
| 4 | 按住 `Alt` → 预览卡徽标在 `Copy` ↔ `Scissors` 间切换，落点脚下出现「复制到 / 移动到」文字 |
| 5 | 拖到标签上 = 放进那个标签的目录；拖到侧栏位置/磁盘/云盘/远程 = 放进该根 |
| 6 | 拖出窗口 → 交给系统（Finder / Explorer），预览卡不变 |
| 7 | 落点非法（跨只读卷、目标为自身子目录）→ 预览卡整体降至 40% 不透明 + `circle-slash` 图标，松手无动作 |

### 4.3 远程连接 → 浏览 → 编辑 → 回传

| 步骤 | 界面行为 |
|---|---|
| 1 | 侧栏「连接服务器…」或命令面板 → Dialog：协议四选一用 **Tab Segmented**（WebDAV / S3 / SFTP / FTP），表单按协议显隐字段（S3 地域、SFTP 私钥、WebDAV「坚果云」一键填址） |
| 2 | 点「连接」→ 按钮进入 `Spinner` + 禁用；**先试连后保存**（沿用现状）；失败原因显示在框内 `danger` 行，不弹二次窗 |
| 3 | 成功 → Dialog 关闭 + Notification「已连接 · 密码存入系统钥匙串」(info)；侧栏远程组出现条目：`Cloud` 图标 + 名称 + **状态点**（`success` 在线 / `muted` 未连 / `warning` 离线） |
| 4 | 浏览 → 列表骨架屏（`Skeleton` 3–5 行）→ 结果；断网时用离线缓存垫底，列表顶部一条 `info` 横幅「离线数据 · 最后同步 14:02」 |
| 5 | `Cmd+Alt+E` 编辑远程文件 → 状态栏右侧常驻「1 个远程文件编辑中（保存即回传）」；编辑完成回传 → Notification「已回传 xxx」+ 撤销 |
| 6 | 侧栏悬停条目 → 尾部出现 `EllipsisVertical` 菜单（重新连接 / 编辑配置 / 断开 / **删除**）；删除仍是**两步确认**（第一击变「确认删除」） |

### 4.4 搜索 → 跳转

| 步骤 | 界面行为 |
|---|---|
| 1 | `Cmd+Shift+F` → 面板（宽 720）；输入框下方一行**语法 chips**：`name:` `ext:` `type:` `size:` `date:` `content:` `tag:` `color:`，点 chip 即插入 |
| 2 | 输入 180ms 去抖后开搜；状态行显示「正在搜索 · 已扫描 1,204 / 命中 18」+ `Spinner` 12px；语法错 → 状态行 `danger` 文案 + 出错片段下方波浪线，不弹窗 |
| 3 | 结果虚拟列表（行 44）：图标 + 名称（命中片段加粗）+ 命中行内容（第二行 `muted`）+ 右侧相对路径 |
| 4 | `↑↓` 移动、`Enter` 跳到文件（先切目录再选中，滚动跟随）；`Esc` 关闭；点遮罩关闭 |
| 5 | 空结果 → `Empty`：`SearchX` 32px + 「没有匹配的文件」+ 一句建议（放宽条件/含子目录） |
| 6 | 「存为…」→ 命名输入行 → 存下后底部一排已存搜索小签（名 + 查询 + `×` 忘掉） |

### 4.5 比较 → 同步

| 步骤 | 界面行为 |
|---|---|
| 1 | `Cmd+Alt+B`（目录）→ 面板宽 860；顶部 `Spinner` + 扫描进度（流式批次） |
| 2 | 工具行：筛选 Segmented（全部差异 / 只看不同 / 只看单边）+ 口径切换（按时间 / 按内容 SHA-256，tooltip 说明代价） |
| 3 | 差异行：状态标签（左独有 / 右独有 / 不同·左新 / 分不出先后）用 `Tag` 变体着色——左独有 `Color(blue)`、右独有 `Color(cyan)`、不同 `Color(yellow)`、分不出先后 `Color(neutral)`；行 34 |
| 4 | 选同步模式（→右 / ←左 / 双向 / 镜像→右 / 镜像←左）→ **先出预演摘要**：「将复制 12 项、删除 3 项、跳过 4 项」+ 展开可看清单；确认后交给传输中心执行（**不自己写搬运循环**，沿用现状） |
| 5 | 文件变化 → 自动重比（订阅 OpsEvent）；面板关闭即中止扫描线程 |
| 6 | 差异高亮色与 §4.1 的冲突框一致：`warning` 系表示"需要你看一眼" |

### 4.6 大文件传输（暂停/限速/续传/重试）

| 状态 | 界面行为 |
|---|---|
| 运行 | 状态栏：名称 + `3/12` + `42.7 MB / 128 MB` + `8.2 MB/s` + `剩 12 秒` + Progress；右上角无通知（避免噪音） |
| 暂停 | 按 `⏸` → 速度与剩余时间**隐藏**（避免"还剩 3 分"僵死），显示「已暂停」badge；进度条保留位置 |
| 限速 | 状态栏「限速」下拉（`Select`）：不限 / 1 / 5 / 10 MB/s，立即生效（令牌桶） |
| 中止 | `✕` → 保留半截 + 续传账本；状态栏回到空闲；**不报失败** |
| 失败 | 状态栏右侧「重试 N 项」按钮 + `warning` 色条目；传输中心 Sheet 可**逐项重试**并显示失败原因 |
| 多任务 | 状态栏只露当前 1 条 + 「还有 N 个任务」；点它打开**传输中心 Sheet**：每行 = 名称 / 进度 / 速度 / 剩余 / 暂停 / 取消 / 重试 |
| 远程编辑 | 状态栏右侧常驻计数，点开也在传输中心里（编辑中的文件是一项） |

---

## 5. 逐界面规格（22 项）

约定：**形态** = 尺寸与结构；**复用** = 组件映射；**交互** = 键鼠行为；**状态** = 空/忙/错；**变更** = 与现状的差异。文案一律 i18n key，前缀 `nav.` / `ops.` / `panel.` / `dialog.` / `status.` / `empty.`。

### A. 应用外壳

#### 1. 标题栏 + 标签栏（原「标签栏」38px）

- **形态**：34px 单行。左：平台窗口控件（macOS 三色灯 / Windows·Linux 右侧按钮），中：标签区（Segmented pill，标签高 26，max 200，hover 显示 `×`），右：`分栏` `同步浏览`（仅分栏时）`搜索` `命令`。
- **复用**：`TitleBar` + `TitleBar::window_options()`；`TabBar` + `Tab::new().icon().suffix(Button(Close))`；尾部按钮用 `Button::ghost().small()` + `Tooltip`。
- **交互**：`Cmd+T` 新标签 / `Cmd+W` 关 / `Ctrl+Tab` 循环 / 中键关标签；点击另一栏的标签**只挪焦点不换位**（保留现状）；**拖文件到标签 = 放进该目录**（`Tab` 外层容器挂 drop target）；标签溢出时 `TabBar::menu(true)` 收进下拉。
- **状态**：最后一个标签关闭 → 回到主页（保留现状）；远程标签加载中 → 标签内 `Spinner` 12px。
- **变更**：自绘标签体 → `TabBar`（获得溢出下拉与滑动指示器）；与窗口控件合并到一行。

#### 2. 侧栏（255px）

- **形态**：`Sidebar` 255px / 折叠 48px；`SidebarHeader` 44px：主色圆角方块 + `FolderOpen` + 「WFiles」+ 副标题一行（折叠时只剩图标）；四组 `SidebarGroup`：**位置**（个人文件夹 / 桌面 / 文稿 / 下载）、**磁盘**（卷列表 + 用量细条 + `Eject` 弹出）、**云盘**（同步目录 + 同步状态点）、**远程**（连接 + 状态点 + 尾部悬停菜单）；`SidebarFooter`：「本地文件系统」提示块（`muted.background`）。
- **复用**：`Sidebar` + `SidebarMenu`/`SidebarMenuItem`/`SidebarGroup` + `SidebarToggleButton`；折叠用 `SidebarCollapsible::Icon`；底部提示用 `SidebarFooter`；行尾删除按钮 `Button::ghost().xsmall()` + `PopupMenu`。
- **交互**：单击导航当前焦点栏；**位置/磁盘/云盘/远程四类都是拖放目标**（拖到「磁盘」整组即落到该卷根）；`Cmd+B` 折叠/展开侧栏；无卷/无云盘时整组隐藏（保留现状）；远程条目删除**两步确认**（第一击变「确认删除」，`danger` 色）。
- **状态**：卷探测 3s TTL（保留）；远程在线/离线/编辑中用状态点区分。
- **无障碍**：每个条目 `aria_label` = 名称 + 状态（如「远程：坚果园，在线」）。
- **变更**：固定 `ListItem` 列表 → `Sidebar` 组件（获折叠动画、分组语义）；宽度可拖（外层 `h_resizable` 包一层）。

#### 3. 状态栏（28px）

- **形态**：三段。**左**：空闲时显示最近一次消息（错误 `danger` 色）；有任务时显示进度详情 + Progress 4px。**中**：常驻提醒（「2 个远程文件编辑中」）。**右**：情境按钮（暂停/继续、中止、重试 N 项、撤销：<动作>、限速下拉）。
- **复用**：`StatusBar::new().left(..).right(..)`；进度 `Progress::new("ops").value(..)`（`.loading(true)` 为不确定）；按钮 `Button::ghost().xsmall()`；限速 `Select`；文案区挂 `aria_label` 供读屏/测试（保留现状做法）。
- **交互**：点击进度区 → 打开传输中心 Sheet；`Esc` 无动作（不抢键）；撤销按钮点两次才执行（危险动作）。
- **状态**：无任务无消息 → 只留右侧按钮；任务完成 → 停留 5s 后回落。
- **变更**：自绘 `v_flex` + 两层 div 进度条 → `StatusBar` + `Progress`；高度 34 → 28。

#### 4. 命令面板（`Cmd+P`，宽 600）

- **形态**：顶部对齐（pt 90）的 Dialog；输入行 46px（`Search` 图标 + 输入 + 清除）；结果区 max 380 虚拟列表；底部提示行 30px「↑↓ 选择 · Enter 执行 · Esc 关闭」。
- **复用**：`Dialog`（overlay_closable + keyboard）包 `Command::new(&state)`；`CommandState` 自带虚拟列表；分组用 `CommandGroup`；**快捷键徽标用 `CommandItem::action(Box<dyn Action>)` 自动渲染 `Kbd`**（替换现存手写 `hint` 文本）；空态用 `Empty`。
- **交互**：候选项**打开时快照**（只列此刻可用的；现状逻辑保留）；打分规则不变（精确 1000 > 前缀 600 > 包含 300 > 关键词 200/100，多词 AND）；`↑↓` 循环、`Enter` 执行、`Esc`/点遮罩关闭；23 条改键入口与传输限速入口保留在同一面板。
- **状态**：无匹配 → `Empty` + 「没有匹配的命令」。
- **变更**：手写输入框+列表 → `Dialog` + `Command`；键盘处理从卡片 `on_key_down` 改为 `CommandState`（`on_query`/`on_confirm`/`on_cancel`）。

#### 5. 应用级对话框（Dialog / AlertDialog 家族）

| 对话框 | 宽度 | 形态与交互 |
|---|---|---|
| **确认**（删除/撤销前） | 420 | `AlertDialog::new(cx).confirm()`；正文一句 + 影响清单（超 5 项折叠「等 N 项」）；`取消`(secondary) / 主操作(primary) 或 `danger`；默认焦点在取消 |
| **冲突** | 560 | 见 §4.1 第 4 步；四策略 + 「应用到剩余全部」`Switch`；冲突项展示来源/大小/时间差（ROADMAP 信任与安全要求） |
| **输入**（新建/重命名） | 420 | `Dialog` + `Input`（预填名）+ 校验错误红字（框内不关闭）；`Enter` 提交（`submit_on_enter`），**失焦不关框**（保留现状理由：否则提交按钮点不到）；创建前过三平台共用校验器（Windows 保留名/非法字符） |
| **批量重命名** | 620 | 五行输入（模板 / 查找 / 替换 / 起始 / 步长）+ 预览列表（旧名 → 新名 + 状态标签 `Tag`：将执行(blue)/无变化(muted)/冲突(yellow)/错误(danger)）+ 统计 + `执行`(primary，无可执行项禁用)；整批一条撤销记录 |
| **连接远程** | 460 | 见 §4.3；协议用 Segmented Tab；错误显示在框内 |

- **复用**：`window.open_dialog(cx, ..)` / `window.open_alert_dialog(cx, ..)` + `DialogHeader/Title/Description/Footer` + `DialogButtonProps`；关闭 `close_dialog(cx)`。
- **变更**：删掉 `overlay_card` 手写骨架与各面板重复的 Esc/遮罩处理，**统一交给 `Root` 的 dialog_layer**（前置见 §8 批次 0）。层级优先级（命令面板 > 连接 > 冲突 > 批量 > 输入 > 确认）改为**同一时刻只开一层**的调度规则。

#### 6. 通知（新增）

- **形态**：右上角（`NotificationSettings.placement` 默认 TopRight），宽 382，最多同时 10 条；图标 + 标题 + 正文 + 可选动作按钮。
- **复用**：`window.push_notification(Notification::new().success(msg).., cx)`；`NotificationDelivery::InApp`（默认）；系统通知留作 P5「平台通知」选项。
- **用途映射**：操作完成（带**撤销**按钮，`action`）/ 失败（带**重试**）/ 连接成功 / 远程回传完成 / 限速已更改（`id::<T>()` 复用同一张卡，避免刷屏）。
- **交互**：4s 自动隐藏；悬停暂停计时；点击或点动作后关闭。
- **变更**：新增能力。状态栏仍保留最近一条消息（持久、可回溯），二者不重复：**瞬时反馈给通知，持久状态给状态栏**。

### B. 窗格

#### 7. 工具栏（40px）

- **形态**：左：`◀ ▶ ▲`（前进/后退/上一级，ghost 20px）+ 面包屑；右：**常显 5 颗**（新建文件夹 / 重命名 / 删除 / 复制 / 粘贴）+ **`Ellipsis` 更多**（`PopupMenu`：剪切、复制路径、复制文件名、批量重命名、压缩包、标记、打开终端、分析、编辑远程文件）+ **视图 Segmented**（列表/网格/分列）+ 筛选输入（宽 200，`ListFilter` 前缀）。
- **复用**：`Button::ghost().small().icon()` + `Tooltip`；`PopupMenu`（`Button::dropdown_menu`）；视图切换用 `TabBar` + `TabVariant::Segmented`（或 `button_group`）；筛选 `Input`。
- **交互**：不可用项 `disabled`（不再"灰着但可点"）；`更多` 菜单项按频率排序、带 `Kbd` 徽标；筛选即时过滤（保留现状）；`Cmd+1/2/3` 切视图。
- **无障碍**：每颗按钮 `aria_label` + `Tooltip` 含快捷键。
- **变更**：12 颗平铺 → 5 + 1 下拉；视图三个独立 Button → Segmented；高度 52 → 40。

#### 8. 面包屑

- **形态**：从右往左、尾部（当前目录）加粗、`/` 分隔的紧凑路径；宽度不足时中间折叠成 `…`（`Ellipsis` 按钮点开列出中间层级）。
- **复用**：`Breadcrumb::new().children(items)`（分隔符固定 ChevronRight，与现状一致）；折叠按钮 `Button::ghost().xsmall()` + `PopupMenu`。
- **交互**：点任一段跳转；拖文件到某一段 = 落到该层；`Cmd+.` 或点击路径空白切「路径输入模式」（可直接粘贴路径，`Input`）。
- **变更**：手写 div 序列 → `Breadcrumb`；新增路径输入模式（现状只能靠导航到达）。

#### 9. 列表视图（默认）

- **形态**：表头 30px（名称 ⇅ / 元数据列 / 大小 ⇅ / 修改时间 ⇅ / 类型）+ 行 34px（标准密度）。行内构成：缩略图或图标 20px → 标记色点 8px（`Badge::dot`）→ 名称（截断，hover 出 Tooltip 显示全名）→ 元数据 → 大小 → 修改时间 → 类型。选中：底色 + **左侧 2px `list.active.border` 竖条**；多选高亮用 `selection.background`。
- **复用**：`v_virtual_list` + `VirtualListScrollHandle` + `scrollbar`（**保留**，10 万项基线）；行用 `ListItem` 外观 + 外层同高 div 挂拖放（保留现状原因：`ListItem` 挂不上 drop target）；标记点 `Badge::new().dot().color()`；图标 `Icon`。
- **交互**：`↑↓`（Shift 扩选）、`Home/End`、`PageUp/Down`、`Enter`/`Cmd+↓` 打开、`Cmd+A` 全选、`Esc` 清选（有浮层先关浮层）、`Backspace` 上一级；双击仅"干净双击"有效（带修饰键不触发，保留）；列头点击排序（活动列显示箭头，第三次点击回默认序）；拖放见 §4.2；`notify` 120ms 去抖自动刷新（保留）。
- **状态**：加载中 `Skeleton` 5 行；空目录 `Empty`（`FolderOpen` 32px + 「这个文件夹是空的」+ 主操作「新建文件夹」）；错误（权限/离线）→ 行内 `danger` 提示 + 重试按钮。
- **变更**：行高 40 → 34（密度令牌）；选中态加左竖条；表头与行高解耦；空态用 `Empty`。

#### 10. 网格视图

- **形态**：格子 112×124（标准），缩略图 64 + 两行截断名 + 一行 `muted` 说明；格间距 12；列数随宽度自适应（现状宽度探针逻辑保留）。
- **复用**：`v_virtual_list` 按"行"渲染（保留）；缩略图 `img` + `Skeleton` 占位；选中态同列表（底色 + 1px `list.active.border` 描边）。
- **交互**：与列表一致（方向键按行列移动）；`Cmd+2` 切换。
- **状态**：缩略图生成中 → `Skeleton` + `ShimmerText`「正在生成预览」。
- **变更**：仅几何令牌化 + 选中描边。

#### 11. 分列视图（Miller columns）

- **形态**：列宽 220 + 1px `border` 分隔；每列自带 30px 小标题（列名 + 条目数）；当前路径所在列的行高亮为「通往下一层」(`list.active.border` 左边线)。
- **复用**：`h_virtual_list` / 横向滚动 `overflow_x_scroll` + `horizontal_scrollbar`（保留）；**组件库无 Miller 语义的现成件，保持自绘**。
- **交互**：点文件夹右扩一列、点文件选中；`←` 回上一列；`Cmd+3` 切换。
- **变更**：列宽 200 → 220（可容下长名）；列标题语义化。

#### 12. 详情板（280px，可拖 220–420）

- **形态**：顶部 40px 标题栏（「详细信息」+ `PanelRight` 收起按钮）；预览块 120px（缩略图/图标 + 类型徽标）；信息行：类型 / 大小 / 修改 / 创建 / Git 状态 / 云盘状态 / 标记 / 位置（路径可点跳转、可复制）。多选时列出前 8 个名字 + 「等 N 项」，并显示合计大小。
- **复用**：`h_resizable` + `resizable_panel()`；信息行 `description_list`（label + 值两列）；标记行 `Tag`；路径行 `Button::ghost()` 复制。
- **交互**：拖左边缘改宽；`Cmd+Alt+I`（或标题栏按钮）收起/展开；`Cmd+C` 选中路径时复制路径（保留现状判定）。
- **状态**：无选中 → 一句话占位（不放大图标，避免视觉噪音）；缩略图未就绪 → `Skeleton`。
- **变更**：固定 260 → 可拖；信息行从手写 div → `description_list`。

#### 13. 空态与忙碌态（横向规范）

| 场景 | 表现 |
|---|---|
| 空目录 / 无搜索结果 / 无连接 | `Empty`：`EmptyMedia(variant: Icon)` 32px + title（17/600）+ description（12，`muted`）+ 一个主操作按钮 |
| 首屏加载 | `Skeleton` 3–5 行（列表）/ 网格 6 格 |
| 后台任务进行中 | `Spinner` 12px（工具栏/标签内）/ `Progress.loading(true)`（状态栏） |
| 缩略图/预览生成 | `Skeleton` + `ShimmerText`（一次性，不循环） |
| 半截结果（分析中止） | 结果保留 + 顶部 `warning` 横幅「这是半截的数 · 继续 / 清除」 |

### C. 浮层（Dialog / Sheet 化）

#### 14. Quick Look（`Space`，720）

- **形态**：Dialog 720×max 560；标题 + 副标题（类型 / 行数 / 尺寸）+ 内容区（文本等宽可滚 / 二进制 hex / 图片 / 目录与不可预览占位）+ 底部「关闭（Esc）」。
- **复用**：`Dialog`（`width(px(720.))`）；内容用 `Input` 只读或自绘（文本）；图片 `img` + `Skeleton` 占位。
- **交互**：`Space` 再按关闭、`Esc` 关闭、按钮关闭；**刻意不点遮罩关闭**（防选中文字误关，保留）；`↑↓` 在预览里翻页（文本视图）。
- **变更**：手写遮罩卡片 → `Dialog`（获得进出动画与焦点管理）。

#### 15. 压缩包面板（560）

- **形态**：Dialog 560；标题（包名）+ 统计副标题（N 项 / 原始 X → 压缩 Y）；等宽条目列表（文件夹 `Folder` / 文件 `File` 图标 + 包内路径 + 大小，可滚）；底部「解压到〈stem〉」(primary) / 「关闭」(ghost)。
- **复用**：`Dialog`；列表 `ListState` + `list(...)`（条目数通常 <1 万，不需要虚拟化，但换成 `ListState` 以获得滚动条一致性）。
- **交互**：`Esc` 关闭；解压/打包后台执行、busy 时按钮禁用 + `Spinner`；打包结果可撤销（保留）。
- **状态**：读取失败 → 面板内 `danger` 文案（不弹窗，保留）。

#### 16. 标记面板（`Cmd+Alt+M`，420）

- **形态**：Dialog 420；文件名；提示「点一下标色，再点一下去掉」；**6 个 26px 圆点**（§2.1 令牌色；选中加 2px `ring` 描边）；标签输入框（`Input`，历史标签 chips 可点）；底部「清除标记」(ghost) / 「完成」(primary)。
- **复用**：`Dialog` + `Input`；色点用 `Badge::new().dot().color(theme.base_red)` 或自绘 26px 圆（按钮语义需要更大点击区，自绘 div + `aria_label`）；历史标签用 `Tag`。
- **交互**：`Enter` 完成、`Esc` 取消（**只吃这两个键，其余放行给输入框**——保留现状的 `stop_propagation` 教训）；点遮罩关闭。
- **变更**：色值 7 个硬编码 hex → 6 个令牌；色点数量与语义收敛。

#### 17. 改键捕获浮层（420）

- **形态**：Dialog 420；标题「给〈命令〉按个键」；当前键位用 `Kbd` 展示（大号）；提示「退格 = 恢复出厂，Esc = 取消」；底部「恢复出厂」(ghost) / 「取消」(ghost)。
- **复用**：`Dialog` + `Kbd::new(stroke)`；捕获仍需 `capture_key_down` 自己收键（组件不提供）。
- **交互**：捕获阶段收键并 `stop_propagation`（防默认绑定趁机执行，保留）；冲突时框内提示「已被〈命令〉占用，仍要覆盖？」。
- **变更**：手写键位文本 → `Kbd`（平台化符号 ⌘⌥⇧⌃ / Ctrl+Alt）。

#### 18. 搜索面板（720）

见 §4.4。**复用**：`Dialog` + `Input` + `v_virtual_list` + `scrollbar` + `Empty` + `Tag`（语法 chips）；面板内键盘闸门逻辑保留（吃 ↑↓/Enter/Esc，放行打字与 Cmd 系）。

#### 19. 分析面板（720，三页签）

- **形态**：Dialog 720；页签 Segmented（文件夹大小 / 重复文件 / 校验和）；状态行（统计 + `复制结果` + `中止/重算`）；结果虚拟列表（混合行高：目录头 44 / 文件行 30 / 校验和行 86——**因行高不一，保持 `v_virtual_list` 传 `row_sizes`，不用 `DataTable`**）；底部（↑↓ 选择 · Enter 跳转 · Esc 关闭）。
- **复用**：`Dialog` + `TabBar`(Segmented) + `v_virtual_list` + `Progress.loading(true)`（扫描中）+ `Spinner`。
- **交互**：切页签 = 换任务（旧趟 `Control::abort()`，保留）；`Drop` 即中止（保留）；面板内再按开关键 = 收起（保留）；`复制结果` 抄全文（保留）。
- **变更**：仅视觉令牌化；页签从独立 Button → Segmented。

#### 20. 目录比较面板（860）

见 §4.5。**复用**：`Dialog` + `TabBar`(Segmented 筛选) + `Select`(口径) + `Button`(5 种同步模式，xsmall) + `v_virtual_list` + `Tag`(状态标签)。

#### 21. 文件内容比较面板（980）

- **形态**：Dialog 980；页签「按行 / 细到字符 / 十六进制」（二进制只留十六进制）；状态行（增删改行数或首处差异字节偏移）；并排虚拟列表，行 26（等宽 12px）：类型标记 18px + 左行号 + 左内容 + 右行号 + 右内容；hex 模式含偏移 60px + 字节串 + ASCII 80px；差异用 `warning` 加粗（行内段级）。
- **复用**：`Dialog` + `TabBar`(Segmented) + `v_virtual_list` + mono 字体；差异高亮沿用自绘 span 拆段。
- **交互**：`↑↓` 翻动滚动跟随、`Enter` 跳到左侧文件、`Esc` 关闭；单文件上限 8MB（超出截断并在状态行说明，保留）。
- **变更**：仅令牌化 + 字号 12（原文 13，等宽并排更省宽）。

#### 22. 内嵌终端（120–480，可拖）

- **形态**：底部面板；顶部 32px 标题行（程序名 · 状态 · 已上翻 N 行 · 全屏程序 + 重启/关闭 + **拖拽把手 `GripVertical`**）；字符网格区（等宽 13 / 行距 2）；尺寸探针自适应（保留）。
- **复用**：`v_resizable("bottom")` + `resizable_panel()`（**推荐**，而非 Dock——Dock 对"单终端"过重）；字符渲染、选区、光标、PTY 全部**保持自绘**（组件不提供终端语义）。
- **交互**：`Ctrl+Alt+T` 开关（保留）；终端聚焦时**除开关键外全部收下**（保留）；`Cmd+C` 复制选区 / `Cmd+Shift+C` 全屏 / `Cmd+V` 粘贴；`Shift+PageUp/Down` 回滚；收起即杀 shell（保留）。
- **配色**：**刻意不跟主题**（保留），但收敛为命名常量：`term.surface = 0x181818`、`term.ink = 0xd4d4d4`、ANSI 16 色 + 256 色立方。理由写进注释：终端是"屏幕"，跟随主题会让 `ls --color` 之类的输出不可读。
- **变更**：固定 240 → 可拖（唯一实质变化）。

### D. 支撑层

| 模块 | UI 落点变化 |
|---|---|
| `ops_entity` | 状态栏 + 对话框数据源不变；新增「传输中心」条目视图模型（任务列表，供 Sheet 消费） |
| `remotes` | 侧栏远程组（状态点）+ 连接 Dialog + 通知（连接/回传成功） |
| `palette` | 打分纯逻辑不变；候选项改为 `CommandItem`（获得自动 `Kbd` 徽标） |
| `settings` | 新增 `density`（三档）、`terminal_height`、`details_width`；删除硬编码常量来源 |
| `marks` | 7 色 hex → 6 令牌；新增旧 id 迁移 |
| `meta` / `thumbnails` | 元数据列与缩略图展示层令牌化 |
| `keymap` | 不变（仍可配置）；新增命令需同步进 `Command` 面板 |
| `volumes` / `cloud` | 侧栏行的状态点与用量条数据来源不变 |

---

## 6. 图标规范

### 6.1 规则（重申，可执行）

1. 只用 `assets/icons` 存在的图标，落到代码前核对：文件名 → 变体名 PascalCase。
2. 尺寸只用 §2.5 的五档；颜色继承文字色，**禁止硬编码**（标记色点除外）。
3. 每个图标按钮：`aria_label` + `Tooltip`（含 Kbd 徽标）。
4. 新增图标必须同时进打包清单（§6.4），并由测试守住（§6.5）。

### 6.2 界面 → 图标映射表

| 用途 | IconName | svg | 打包 |
|---|---|---|---|
| 后退 / 前进 | `ChevronLeft` / `ChevronRight` | chevron-left / chevron-right | ✔ |
| 上一级 | `ArrowUp` | arrow-up | ✔ |
| 主目录 | `House` | house | **✘ 补** |
| 刷新 | `RefreshCw` | refresh-cw | ✔ |
| 双栏同步浏览 | `ArrowLeftRight` | arrow-left-right | **✘ 补** |
| 视图：列表 / 网格 / 分列 | `List` / `LayoutGrid` / `Columns3` | 同名 | ✔（ViewIcons） |
| 新建文件夹 | `FolderPlus` | folder-plus | **✘ 补** |
| 重命名 | `SquarePen` | square-pen | **✘ 补** |
| 复制 / 剪切 / 粘贴 | `Copy` / `Scissors` / `ClipboardPaste` | copy / scissors / clipboard-paste | 前 ✔，后二 **✘ 补** |
| 删除到回收站 | `Trash` | trash | **✘ 补** |
| 批量重命名 | `TextCursorInput` | text-cursor-input | **✘ 补** |
| 压缩包 | `PackageOpen` | package-open | **✘ 补** |
| 打开终端 | `SquareTerminal` | square-terminal | ✔ |
| 快速查看 | `Eye` | eye | ✔ |
| 标记 | `Tags` | tags | **✘ 补** |
| 筛选 | `ListFilter` | list-filter | **✘ 补** |
| 搜索 | `Search` | search | ✔ |
| 搜索无结果 | `SearchX` | search-x | **✘ 补** |
| 清空输入 | `X` | x | **✘ 补** |
| 溢出菜单 | `Ellipsis` / `EllipsisVertical` | ellipsis / ellipsis-vertical | **✘ 补** |
| 撤销 / 重做 | `Undo2` / `Redo2` | undo-2 / redo-2 | ✔ / **✘ 补** |
| 命令面板 | `Command` | command | **✘ 补** |
| 完成 / 失败 / 提示 | `CircleCheck` / `TriangleAlert` / `Info` | circle-check / triangle-alert / info | ✔ |
| 进行中 | `LoaderCircle` | loader-circle | ✔ |
| 文件夹（关/开/默认） | `FolderClosed` / `FolderOpen` / `Folder` | 同名 | ✔ |
| 文件 / 文本 / 代码 | `File` / `FileText` / `FileCode` | file / file-text / file-code | ✔ |
| 图片 / 音频 / 视频 / 压缩 / 归档 | `Image` / `Music` / `Video` / `FileArchive` | image / music / video / file-archive | 前三 ✔，`FileArchive` **✘ 补** |
| 磁盘 / 用量 / 弹出 | `HardDrive` / `Gauge` / `Eject`(用 `ArrowUpFromLine`) | hard-drive / gauge / — | 前二 ✔ |
| 可移动设备 | `Usb` / `Disc` | usb / disc | ✔ |
| 云盘 / 离线 | `Cloud` / `CloudOff` / `WifiOff` | cloud / cloud-off / wifi-off | **✘ 补** |
| 远程服务器 | `Server` / `ServerCog` | server / server-cog | 前 ✔ |
| 连接 / 加密 | `Plug` / `Lock` / `KeyRound` | plug / lock / key-round | ✔ |
| 下载 / 上传 | `Download` / `Upload` | download / upload | **✘ 补** / ✔ |
| 暂停 / 继续 | `Pause` / `Play` | pause / play | ✔ |
| 限速 | `Gauge` | gauge | ✔ |
| 元数据列：Git | `GitBranch` | git-branch | ✔ |
| 元数据列：尺寸/时长 | `Ruler` / `Timer` | ruler / timer | ✔ |
| 元数据列：时间/类型/校验 | `Clock` / `FileType` / `Hash` | clock / file-type / hash | ✔ |
| 目录比较 / 内容比较 | `Columns2` / `FileDiff` | columns-2 / file-diff | **✘ 补** |
| 分析（文件夹大小） | `ChartPie` | chart-pie | ✔ |
| 面板收起（右/下/右栏） | `PanelRight` / `PanelBottom` | panel-right / panel-bottom | ✔ |
| 拖拽把手 | `GripVertical` | grip-vertical | **✘ 补** |
| 设置 / 快捷键 / 外观 | `Settings` / `Keyboard` / `SunMoon` | settings / keyboard / sun-moon | ✔ |
| 加号 / 关闭 | `Plus` / `Close` | plus / close | ✔ |

> 「**✘ 补**」= 现在没打进二进制，**必须**加入打包清单才能显示（清单见 §6.4）。
> `Eject` **不存在**于图标库（已核对），弹出卷用 `ArrowUpFromLine`。

### 6.3 图标在界面中的分布（检查一致性用）

| 区域 | 尺寸 | 是否有背景 | 颜色 |
|---|---|---|---|
| 窗口控件区 | 组件自带 | 无 | `foreground` |
| 标签页内 | 14 | 无 | 活动 `foreground` / 非活动 `muted` |
| 侧栏条目 | 16 | 无 | 活动 `sidebar.accent.foreground` |
| 工具栏 | 16（常显） | hover 时 `muted.background` | `foreground`；disabled 40% |
| 列表行（类型图标） | 16（缩略图 20） | 无 | `muted.foreground` |
| 列表行（状态徽标，复制/剪切） | 12 | 无 | `info` / `warning` |
| 表头排序箭头 | 12 | 无 | 活动列 `foreground`，其余透明（不显示） |
| 空态 | 32 | 无 | `muted.foreground` |
| 对话框按钮 | 16 | 无 | 随按钮前景 |
| 状态栏 | 12 | 无 | `muted.foreground` |
| 通知 | 20 | 无 | 由类型决定（success/danger/info） |

### 6.4 打包清单（现状 + 需补）

现状（`src/main.rs:55`）：

```rust
assets::icon_assets!(ViewIcons, [List, LayoutGrid, Columns3]);
```

问题：`Assets` 只打包 `default-icons.txt` 里的 **106** 个（已核对构建产物 `target/debug/build/gpui-kit-assets-*/out/default_assets.rs`，共 106 条 `icons/` 记录）。代码在用的 **45** 个图标里有 **19** 个不在其中，`load()` 返回 `Err`，界面渲染为**空白**。

修法（`embedded` 常量对全部 1830 个图标都存在，所以 `icon_assets!` 可列任意图标，无需 `AllAssets` 的 7MB）：

```rust
assets::icon_assets!(ExtraIcons, [
    // 现有代码缺的 19 个
    ClipboardPaste, Cloud, Columns2, Command, Download, FileArchive, FileDiff, FolderPlus,
    House, Link, Monitor, Scissors, SearchX, Signature, SquarePen, Terminal, TextCursorInput,
    Trash, X,
    // 本次重设计新增的
    ArrowLeftRight, ArrowUpFromLine, Ellipsis, EllipsisVertical, GripVertical, ListFilter,
    PackageOpen, Redo2, Tags, TextSearch,
]);
```

### 6.5 让规范落地：一条图标测试（建议）

```rust
// 扫描 src/ 里所有 `IconName::X`，断言每个都能被资源源加载到
// 断言钉在真实数据源上：直接调资产管理器的 load(path)，不看回调
#[gpui_kit::test]
fn every_referenced_icon_is_packed(cx: &mut TestAppContext) { /* … */ }
```

> 加进测试前先在**修复前**跑一遍——应当**红**（19 个失败）。全绿说明它没钉住任何东西（沿用项目的测试纪律）。

---

## 7. 无障碍与 i18n（P5 同步项）

| 项 | 规范 |
|---|---|
| 焦点可见 | 所有可键盘到达的元素必须有 `ring` 焦点环（键盘焦点与鼠标悬停必须可区分） |
| 读屏标签 | 图标按钮 `aria_label`；列表行 = 「名称, 类型, 大小, 修改时间」；进度 = 「正在复制, 42%, 剩 12 秒」（状态栏文案已攒纯文本，保留该做法） |
| 键盘可达 | 每一层浮层都能纯键盘开/关/选择；Tab 环不逃出模态 |
| 对比度 | 正文与 `muted.foreground` 在浅/深色下都需 ≥4.5:1 与 ≥3:1（`muted.foreground` 用在中性 500 档，深浅色均由主题保证） |
| 动效 | 提供「减少动态效果」遵循系统设置（侧栏折叠/抽屉改为即时） |
| i18n | 所有文案走 key（`nav.` / `ops.` / `panel.` / `dialog.` / `status.` / `empty.`）；**图标旁文字与 Tooltip 都要 i18n**，不能只译正文 |
| 数字/时间 | 大小与时间走 `format.rs`，单位与日期格式随语言 |

---

## 8. 实施批次（每批独立可验收）

### 批次 0 — 前置（必须先做，否则后面全白干）

1. `WFilesApp` 包进 `Root`（`Root::new(view, window, cx)`），使 `dialog_layer` / `notification_layer` 生效。
2. 补打包清单（§6.4）+ 图标测试（§6.5）。
3. 新增 `src/tokens.rs`：几何常量 + 密度三档；`Theme.font_size = 13` / `mono_font_size = 13` / `radius = 6`。
4. 标记色迁移（7 → 6 令牌 + 旧值映射）。

**验收**：`window.open_dialog` 能弹出；45 个在用图标全部渲染（肉眼 + 新测试）；侧栏/列表/状态栏在标准密度下与设计稿一致；`cargo test --all` 全绿；`df` 检查系统盘（外置卷编译）。

### 批次 1 — 外壳

TitleBar + TabBar、Sidebar（含折叠）、StatusBar + Progress、Notification（含撤销按钮）、命令面板（Dialog + Command）。

**验收**：三平台窗口控件不重叠（**必须实机看 Windows/Linux**，macOS 通过不能代表）；`Cmd+P` 结果行的 Kbd 徽标正确；完成一次复制后通知出现且撤销可用。

### 批次 2 — 窗格

工具栏收纳、Breadcrumb、列表选中/悬停/焦点环、Empty/Skeleton、详情板可拖。

**验收**：10 万项目录滚动不掉帧（沿用性能基线）；12 颗按钮收纳后没有命令找不到（逐一对照命令面板清单）；键盘焦点在列表与详情板之间可辨。

### 批次 3 — 浮层

6 个对话框统一到 Dialog/AlertDialog（替换 `overlay_card`）、Quick Look/压缩包/标记/改键、搜索/分析/比较/差异面板令牌化。

**验收**：不再存在手写遮罩卡片（全局搜索 `overlay_card` 应为 0 处）；每个对话框 `Esc`、点遮罩、初始焦点行为一致；冲突框四种策略 + 「应用到剩余全部」全覆盖测试。

### 批次 4 — 进阶

终端 `v_resizable`、传输中心 Sheet、`NativeMenu`（macOS/Windows 真原生，Linux `AppMenuBar` 自绘）、右键 `ContextMenu`（列表/侧栏/标签）。

**验收**：终端拖高不抖动、不丢选区；传输中心多任务并行时状态栏摘要与面板一致；原生菜单在 macOS、Windows 上真出现（Linux 明确为自绘，需在文档里写明差异）。

---

## 9. 非目标（明确不做）

| 不做 | 原因 |
|---|---|
| 用 `DataTable` 替换文件列表 | 组件要求**统一行高**，而我们需要缩略图行/网格/分列三种异构渲染；且内部虚拟化与现有 `v_virtual_list` 是同一个 `uniform_list`，性能无收益、约束更多 |
| 自定义列（脚本列） | ROADMAP「明确不做」；且插件系统已被否，没有承载脚本列的东西 |
| 磁盘 treemap、压缩包内搜索 | ROADMAP「明确不做」 |
| 引入 Dock 布局系统 | 为多处可停靠面板设计，对"一个底部终端"过重；用 `v_resizable` |
| 终端跟随主题 / 提供浅色终端主题 | 桌面惯例是终端保持自身配色；跟随主题会让带颜色的命令输出不可读 |
| 列表滚动/筛选加动画 | 直接违反性能基线（10 万项不掉帧） |
| Figma 稿导入 | 目标文件 `Untitled`（jAVJ3zknYZdxvh7DXqOb6l）为空：0 节点、0 组件、0 样式；且 Figma 集成是只读的，不向画布写。若后续在 Figma 里画了稿，可再走 `figma tokens` / `figma component` 导入对齐 |

---

## 10. 附录：本方案的证据来源

| 结论 | 验证方式 |
|---|---|
| 组件 API（Dialog/Command/Sidebar/TabBar/Progress/StatusBar/Empty/Sheet/Resizable/Notification/NativeMenu…） | 通读 `gpui-component 0.7.1` 源码逐条抄签名 |
| 组件复用现状（6 类） | `grep use gpui_kit/component` 全仓 |
| 主题令牌与默认值 | `src/theme/default-theme.json`、`default-colors.json`、`Theme` 字段 |
| 图标总数 1830 / 默认打包 106 | `ls assets/icons | wc -l`；构建产物 `target/**/gpui-kit-assets-*/out/default_assets.rs` 计数 |
| 19 个在用图标未打包 | 生成物中 `grep '"icons/trash.svg"'` 等**无匹配**；`embedded` 常量对 1830 个全存在（故可补） |
| `Root` 是 Dialog/Notification 前置 | `gpui-component/src/root.rs` 的 `dialog_layer` / `notification_layer`；`main.rs` 未包 `Root` |
| 现有界面 22 项与几何常量 | 通读 `app.rs` / `pane.rs` / 各 `*_entity.rs` |
