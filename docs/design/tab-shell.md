# Tab Shell 重构设计

状态：设计中。本文定义 Latte Lens 从"单投影 + 单内容"外壳迁移到"一切皆 tab"外壳的
目标模型、数据归属、渲染与输入路由、快捷键迁移、分阶段实施与测试门禁。终端 PTY、
Agent 发送链路、浏览器/阅读视图属于后续独立设计，不在本文范围内。

## 1. 概述

当前外壳是"左树右内容"的单投影结构：`TreeScope`（AllFiles / GitChanges / Agents）
切换左窗格投影，右窗格永远只有一份内容状态。目标外壳是 **tab 化的情境容器**：

- 一个 tab = [投影列表 | 内容窗格] 的完整情境，拥有自己的选择、展开、滚动与内容状态。
- tab 栏常驻顶部，「+」菜单是唯一的 tab 工厂，⌘P 风格面板是 tab 切换与文件打开的主路径。
- 投影类型即 tab 类型：Files（工作区树）、Review（Git 变更）、Search（查询结果）、
  Chat（Agent 会话观察，特性门控）。Terminal / Browser 不在本期。
- 全局共享数据集与后台 runtime 不上 tab；tab 只持有"视图状态 + 内容状态"。

设计原则：tab 是情境的唯一组织维度；凡是想"切换"的视图，都必须能以 tab 形式并行留着。

## 2. 不可破坏的产品边界

1. 只读产品边界不变：tab 化本身不引入任何写操作。Terminal tab 与 Agent 发送链路是
   写/执行面，必须各自走独立设计与边界评审，不得搭车进入本重构。
2. 后台 runtime 模型不变：文件系统/Git/Preview 的 `WorkerRuntime`、`SearchRuntime`、
   `NavigationRuntime`、`AgentRuntime` 保持全局单例；tab 通过 generation/epoch 路由
   请求与拒绝过期结果，不新建每 tab 线程。
3. 有界遍历与非阻塞语义不变：50,000 条目上限、两层初始扫描、懒展开、搜索取消、
   stale generation 拒绝，全部保持。
4. 渲染函数不读文件、不查 PATH、不启动进程、不等锁；前景色样式约定不变。
5. 质量门禁只升不降：UT 93%（Q1 直接责任面）、生产 PTY E2E 85%、Agent Core 80%；
   Files 与 Git Changes 用户旅程保持阻塞级生产二进制 E2E 覆盖（迁移为 tab 流程）。
6. canonical keymap 以 `docs/design/keyboard-shortcuts.md` 为唯一事实来源；任何键位
   变更必须同 PR 更新该文档、README controls 表与 footer help text。
7. 不要求 Nerd Font；tab 标题与图标使用普通 Unicode/ASCII 并做宽度兜底。

## 3. 当前实现基线

重构必须从下列真实 seam 接入，而不是另建一套并行状态机。

### 3.1 App 是约 100 个字段的单体（`src/app.rs:679-789`）

- 内容状态是一个扁平字段簇：`content_lines` / `content_highlights` / `content_scroll` /
  `content_horizontal_scroll` / `content_selection` / `content_mode` / `content_provider` /
  `content_preview_kind` / `content_source_target` / `content_show_line_numbers` /
  `content_diff_lines` / `content_identity` / `content_fold_source` / `content_fold_regions` /
  `content_structure` / `content_collapsed_folds` / `content_cursor_line` /
  `content_successful` / `content_projection_width`（`app.rs:689-709`）。
  该簇已由 `reset_content`（`app.rs:6546-6570`）整体清空、由
  `apply_content_completion`（`app.rs:6340-6419`）整体填充——天然的结构体边界。
- `fold_cache: VecDeque<(ContentIdentity, HashSet<FoldAnchor>)>`（`app.rs:710`）已按
  文档身份做键，保持全局共享。
- 每投影状态已手工存在两份：`all_files_selection` / `git_changes_selection`
  （`app.rs:723-724`）、`all_files_expansion` / `git_changes_expansion`
  （`app.rs:729,733`）、`visible_rows` / `git_rows` 等渲染数据集（`app.rs:737-742`）。
  tab 化是把这套"按字段名区分投影"的模式一般化。
- `SearchState.restore`（`app.rs:256-295`）已经是打开搜索时捕获的
  树+内容+导航完整快照；App 另有 suspend-resume 快照（`app.rs:1775, 2720`）。
  tab 的挂起/恢复复用同一快照模式。
- popup 是 `Option` 字段，层级靠前缀存在性判定：`navigation_picker`（`app.rs:786`，
  最高）、`preview_find`（`app.rs:753`）、`search`（`app.rs:750`）。
  「+」菜单与 ⌘P 面板是这一模式的两个新全局 popup。
- 全局共享数据集保持全局：`all_entries` / `changed_entries`（`app.rs:682-683`）、
  `repo_graph`（`app.rs:722`）、`repo` / `branch` / 计数（`app.rs:681, 711-716`）、
  `tree_epoch`（`app.rs:732`）、四个 runtime（`app.rs:744, 749, 775, 780`）、
  `ui_regions`（`app.rs:718`）。

### 3.2 投影切换机制已存在

`TreeScope`（`app.rs:108-115`）+ `set_tree_scope`（`app.rs:3418-3464`）+
`apply_tree_scope`（`app.rs:3466-3496`）：保存外发投影选择 → 重置 `tree_state` →
重建可见行 → 恢复内发投影选择 → 进入 GitChanges 时强制 refresh。tab 切换吸收这套
逻辑，`TreeScope` 与 scope tabs 渲染（`ui.rs:453-489`）随之删除。

### 3.3 输入与渲染路由

- 键分派顺序（`handle_key`，`app.rs:1589-1723`）：navigation picker → 复制键 →
  preview find → search popup → 全局 `q`/`Esc` → 全局快捷键 → 按 `focused_pane`
  分派（ScopeTabs/Tree/Content，`app.rs:3537-3618`）。
- 渲染（`ui::draw`，`ui.rs:120-180`）：header(2) / body / footer(1)；body 横向切
  左树窗格 + 1 列分隔 + 右内容；命中盒由 `regions()`（`ui.rs:204-297`）每帧重建，
  存 `UiRegions`（`app.rs:597-629`）。
- 鼠标（`handle_mouse`，`app.rs:2804-2960`）与键同构：popup 优先，然后基础层按
  region 命中。

### 3.4 Agent 观察层是只读的，且已有现成投影行模型

`AgentViewState`（`agent/state.rs:134-142`）持有
`BoundedVec<AgentViewSession, MAX_VIEW_SESSIONS=256>`；`AgentViewSession`
（`state.rs:60-86`）携带会话键、subject、生命周期、活动状态、freshness、
changes/artifacts/turns 计数——可直接作为 Chat tab 的投影行。
`ObservationProvider`（`agent/provider.rs:178-215`）**有意只提供
discover/probe/snapshot/next_event**，没有 start/send/focus/resume；发送链路不存在，
本期不建。

## 4. 目标数据模型

### 4.1 Tab 身份与类型

```rust
pub(crate) struct TabId(u64);

pub(crate) enum TabKind {
    Files,
    Review,
    Search,
    Chat, // feature = "agent-observability"
}
```

`TabId` 由 App 单调分配，不复用。tab 标题按类型与情境自动生成
（`files`、`review · main`、`search · <query>`、`chat · <session subject>`），
用户不可重命名（本期）。

### 4.2 Tab 结构

```rust
pub(crate) struct Tab {
    id: TabId,
    kind: TabKind,
    title: String,
    // 投影（左窗格）
    tree_state: ListState,
    projection: ProjectionState,
    panel_width: u16,
    // 内容（右窗格）
    content: ContentState,
    preview_find: Option<PreviewFindState>,
    // 每 tab 模态
    navigation_picker: Option<NavigationPickerState>,
}

pub(crate) enum ProjectionState {
    Files {
        selection: Option<PathBuf>,
        expansion: HashMap<PathBuf, bool>,
        visible_rows: Vec<FileEntry>,
        truncated: bool,
    },
    Review {
        selection: Option<GitRowIdentity>,
        expansion: HashMap<GitRowIdentity, bool>,
        visible_rows: Vec<GitTreeRow>,
    },
    Search {
        query: String,
        mode: SearchMode,
        results: Vec<SearchResult>,
        list_state: ListState,
        generation: u64,
    },
    Chat {
        selection: Option<SessionKey>,
    },
}
```

`ContentState` 是 §3.1 内容字段簇的整体提取，构造一个 `ContentState::default()`
等价于今天的 `reset_content`。`navigation_caret` / `navigation_hover_highlight` /
`navigation_target_highlight`（`app.rs:788-790`）随内容渲染走，归入 `ContentState`。

### 4.3 状态归属总表

| 状态 | 归属 | 理由 |
| --- | --- | --- |
| 内容字段簇、preview find、每 tab 导航高亮 | `Tab.content` | 右窗格是 tab 的内容 |
| 树选择/展开/可见行/`ListState`/面板宽度 | `Tab.projection` | 左窗格是 tab 的投影 |
| navigation picker | `Tab` | 预览与跳转都作用于本 tab 内容 |
| `fold_cache` | 全局 | 已按 `ContentIdentity` 键控，跨 tab 共享 |
| `all_entries` / `changed_entries` / `repo_graph` / repo 状态 / `tree_epoch` | 全局 | 规范扫描结果，所有 tab 共用 |
| 四个 runtime 与 generation 槽 | 全局 | 线程模型不变（边界 2） |
| 导航历史 back/forward | 全局 | 按内容身份键控，跨 tab 连续 |
| LSP source / document 版本 | 全局 | LSP 会话按工作区共享 |
| Agent runtime/state/view | 全局 | Chat tab 是全局观察状态的一个视图 |
| 搜索 runtime | 全局；Search tab 持有 query/结果/generation | worker 单例不变 |
| `ui_regions` / 主题 / 配置 / 剪贴板状态 / quit 确认 | 全局 | 外壳级状态 |
| 「+」菜单 / ⌘P 面板 | 全局 popup | 作用于 tab 集合本身 |

### 4.4 App 新外壳字段

```rust
tabs: Vec<Tab>,
active_tab: TabId,
next_tab_id: u64,
new_tab_menu: Option<NewTabMenuState>,   // 「+」下拉
tab_palette: Option<TabPaletteState>,    // ⌘P 面板
```

软上限 `MAX_OPEN_TABS = 16`：到达上限时「+」菜单仍可打开但提交被拒并提示关闭
或用 ⌘P 切换；不静默丢弃。

## 5. 渲染与输入路由

### 5.1 布局

```
tab_bar(1)   ← tab 标题序列 + 「+」
header(1)    ← 现有 repo/branch/refresh 行（draw_header 收窄为 1 行）
body         ← [投影窗格 | 1 列分隔 | 内容窗格]，全部取自 active tab
footer(1)    ← 现有状态/帮助行
```

总 chrome 行数与今天一致（3 行）。tab 栏复用 `draw_scope_tabs`（`ui.rs:453-489`）
的 span 行 + 每 tab `Rect` 命中盒模式；不引入 `ratatui::widgets::Tabs`。
投影/内容窗格的绘制函数（`draw_tree` `ui.rs:1062`、`draw_content` `ui.rs:1631`）
改为接收 `&Tab`，函数体不变。

### 5.2 模态层级（新）

1. `new_tab_menu`（全局，锚定「+」下方的小 popup，5 行模板列表 + 快捷键提示）
2. `tab_palette`（全局，居中，复用 navigation picker 的 Clear+边框+列表布局）
3. 复制键
4. active tab 的 `navigation_picker`
5. active tab 的 `preview_find`
6. 基础层（tab 栏 / 投影 / 内容）

渲染顺序与之一致：基础层 → tab 模态 → 全局模态，各自 `dim_underlay`
（`ui.rs:182-190`）。`UiRegions` 增加 `tab_bar: Vec<Rect>`、`new_tab_button: Rect`、
`new_tab_menu`、`tab_palette` 字段。

### 5.3 键分派（新）

`handle_key` 新顺序：

1. `new_tab_menu` → 方向键选择 / 字母助记 / Esc 关闭 / Enter 提交
2. `tab_palette` → 输入过滤 / 上下 / Enter（切换或打开文件）/ Esc
3. 复制键（现状不变）
4. active tab 的 `navigation_picker` / `preview_find`（现状不变）
5. 全局 tab 命令（见 §6）
6. 按 active tab 的 `focused_pane` 分派到现有 `handle_tree_key` /
   `handle_content_key`（参数化 `&mut Tab`）

### 5.4 ⌘P 面板的数据源

面板条目 = 打开的 tab（切换）+ 工作区文件（在 active Files tab 打开，无 Files tab
时新建一个）。文件侧复用现有 `recent_files`（`app.rs:756`）与文件搜索索引
（`rebuild_file_search_results`，`app.rs:2344-2418`）。面板状态机复用
`NavigationPickerState` 的分组/可见行/预览模式。

## 6. 快捷键迁移

canonical keymap 变更（同 PR 更新 `docs/design/keyboard-shortcuts.md`）：

| 键 | 今天 | 迁移后 |
| --- | --- | --- |
| `Ctrl+P` | 文件搜索 popup | ⌘P 面板（tab 切换 + 文件打开） |
| `Ctrl+T` / `Ctrl+Shift+F` | 工作区文本搜索 popup | 新建/聚焦 Search tab |
| `Ctrl+F` | Preview find | 不变（每 tab） |
| `Tab` / `Shift+Tab` | 循环 scope | 循环 tab |
| `1` / `2` / `3` | 选 scope | 切到第 N 个 tab（`1`-`9`） |
| `Ctrl+W` | （无） | 关闭 active tab（最后一个 tab 不可关） |
| `Alt+1..9` | （无） | 跳第 N 个 tab（与 `1-9` 等价，供已习惯 IDE 者） |

删除项：居中的文件/文本搜索 popup（`draw_search_popup` `ui.rs:503-683` 及其
`SearchState` 会话 machinery）；其快照/恢复模式由 tab 挂起/恢复继承。
`Ctrl+W` 需在目标终端矩阵验证上报可靠性；若不稳定，fallback 为 tab 栏聚焦时的 `x`。

## 7. 分阶段实施

### Phase 1 — 内容状态提取与单 tab 外壳（纯重构，行为不变）

1. 把 §3.1 内容字段簇提取为 `ContentState`，`reset_content` /
   `apply_content_completion` / `request_content_with_review_path` 改为操作
   `&mut ContentState`。机械移动，编译与现有测试兜底。
2. 引入 `Tab` / `TabId` / `TabKind`，App 持有 `tabs: Vec<Tab>` 与 `active_tab`；
   启动时恰好创建一个 Files tab。投影侧先只迁移 Files 所需字段。
3. tab 栏渲染（一个 tab + 「+」按钮，「+」暂不响应或仅响应 Files 模板）。
4. 验收：`make ci` 全绿；`app_tui_integration.rs`（4622 行）不改一行通过；
   视觉与今天逐像素一致（TestBackend 断言）。

### Phase 2 — 多 tab 与投影迁移

1. 「+」菜单（Files / Review / Search / Chat 模板）与 `MAX_OPEN_TABS` 软上限。
2. Review tab：吸收 GitChanges 投影；删除 `TreeScope` / `set_tree_scope` /
   `apply_tree_scope` / scope tabs 渲染与 `1`/`2` 键；进入 Review tab 时保留
   "每次进入强制 refresh"语义（`app.rs:3447-3449`）。
3. Chat tab（特性门控）：投影行用 `AgentViewSession`，内容窗格渲染选中会话的
   observation 流；只读。
4. ⌘P 面板（tab 切换 + 文件打开）与 `Ctrl+W` 关 tab。
5. Search tab：文本搜索迁入；文件搜索并入 ⌘P；删除居中搜索 popup。
6. 同 PR 更新 keymap 文档、README controls、footer help；suspend-resume 快照
   覆盖 tabs。
7. 验收：`make ci` + `make coverage` 全绿且门禁不降；Files / Git Changes 阻塞级
   PTY E2E 旅程迁移为 tab 流程并保持阻塞级。

### Phase 3 — 后续独立设计（本文不展开）

- Terminal tab（PTY）：写/执行面，需要独立的只读边界评审与设计文档。
- Chat 发送链路：`ObservationProvider` 有意无 send；新建协议是独立设计。
- Browser / Reader tab。

## 8. 测试门禁

- Phase 1：现有测试零修改通过；新增 `ContentState` 提取的单元测试
  （reset/apply 等价性）。
- Phase 2：新增 TestBackend 集成测试——tab 栏渲染与命中盒、「+」菜单开合与
  模板提交、多 tab 开关切换、**tab 间状态隔离**（切走再切回，选择/展开/滚动/
  内容/折叠全部保留）、软上限拒绝、⌘P 过滤/切换/打开文件、Search tab 查询流。
- 既有"scope 切换"测试改写为"tab 切换"测试，断言语义一一对应。
- 覆盖率门：93 / 85 / 80 不降（边界 5）；Files 与 Git Changes 旅程保持阻塞级 E2E。
- 不削弱、不删除任何时序/安全测试来让改动通过。

## 9. 开放问题

1. **导航历史归属**：本设计取全局（按内容身份键控，跨 tab 连续）。若评审认为
   "回退"应只在本 tab 内生效，改为每 tab 一个 `VecDeque`，Phase 2 前定。
2. **`Ctrl+W` 终端兼容性**：需在 Linux/macOS/Windows 终端矩阵验证；fallback
   是 tab 栏聚焦时的 `x` 单键。
3. **tab 标题溢出**：窄终端下 tab 栏截断策略（优先保活动 tab + 「+」，其余按
   最近使用压缩），Phase 2 实现时定细节。
4. **Search tab 与全局搜索 runtime 的 generation 路由**：多个 Search tab 并存时
   每个 tab 自持 generation，worker 单槽合并语义不变，过期结果按 tab generation
   拒绝——实现时需验证与现有 `search_generation`（`app.rs:754`）的衔接。
