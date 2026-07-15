# 代码跳转设计

状态：实现前设计，尚未落地。

本文定义 Latte Lens 的只读代码导航能力。目标不是在终端里重写一个语言分析器，
而是在用户显式配置并信任语言服务器时复用 LSP 的语义结果；没有已配置且可用的语言
服务器时，Definition、References 和 Implementations 都明确提示并保持原位。Tree-sitter
与 pulldown-cmark 只负责折叠和文档符号，不承担代码语义解析。

首期范围包括：跳转定义、查找引用、跳转实现、文档符号、多个结果选择、返回和前进。
重命名、重构、编辑、补全、Hover、诊断 UI、Code Action、Workspace Symbol 和 SCIP
不在本文范围内。

## 1. 不可破坏的产品边界

1. Latte Lens 不下载、不安装、不升级语言服务器。
2. 语言服务器必须同时满足“用户级配置中存在该 family、`enabled=true`、可执行文件通过
   trust validation”；PATH 中恰好存在同名程序不构成授权，也不会被自动发现或启动。
3. 已授权的语言服务器只在用户触发语义导航后懒启动；仅打开工作区不会启动外部程序。
4. 不把“工作区中唯一同名符号”当成语义定义。Workspace Symbol 不是 Definition，代码
   Definition 没有 AST fallback。
5. 折叠和本地文档结构继续由现有 Tree-sitter / pulldown-cmark 解析产生，不等待 LSP。
6. 语义导航只对内置 `text` Preview 生效；Diff、Info、第三方 `PreviewProvider` 不参与。
7. 只接受选中工作区内部、通过既有 no-follow 普通文件检查的 `file:` URI；依赖源码等
   工作区外目标不扩大当前内容读取 authority。
8. 渲染函数不读取文件、不查 PATH、不启动进程、不等待锁或协议响应。
9. LSP 是用户显式选择信任的外部进程。Latte Lens 会拒绝服务端编辑请求，但无法从应用层
   保证任意语言服务器自身不修改文件；因此绝不从仓库内读取可执行命令配置，也不通过
   shell、`.cmd` 或 `.bat` 间接执行命令。
10. 现有 `PreviewProvider` 公共契约保持不变。

## 2. 当前实现基线

实现必须从下列真实 seam 接入，而不是另建一条同步 UI 路径。

- [`src/main.rs`](../../src/main.rs) 的 `Cli` 当前只有工作区路径；`main` 通过
  `App::new` 启动应用。
- [`src/app.rs`](../../src/app.rs) 的 `App::handle_key` 先处理 Find/Search，再分派到
  `handle_scope_tabs_key`、`handle_tree_key` 和 `handle_content_key`。`FocusPane::Content`
  是编辑器快捷键的作用域。
- `App` 使用 `RequestGeneration` 管理 Preview 请求，并在 `poll_background` 中合并
  `WorkerRuntime::take_completions`；`apply_content_completion` 是内容身份、折叠、搜索目标
  和滚动位置的唯一 reducer。
- [`src/runtime.rs`](../../src/runtime.rs) 的 `WorkerRuntime` 是单独的 `latte-lens-io`
  线程，`RequestSlot` 保留最新请求，`RequestGeneration::accept` 拒绝过时 completion。
  长生命周期 LSP 不能占用这个串行文件/Git/Preview worker。
- `ContentIdentity` 是规范化的工作区相对路径；`ContentSnapshot` 已携带
  `identity`、`fold_source` 和 `fold_regions`。导航文档身份在此基础上扩展，不能退回
  UI label 或只用文件名。
- [`src/folding.rs`](../../src/folding.rs) 已用 Tree-sitter 解析 Rust、TypeScript/TSX、
  JavaScript/JSX、Python 和 Go，用 pulldown-cmark 解析 Markdown。`fold_regions` 有
  节点、事件和区域上限，并在后台 Preview 路径调用。
- `ContentVisualRow` 是原始逻辑行到换行/折叠显示行的投影。`content_visual_rows`、
  `reveal_folded_line` 和 `scroll_to_logical_line` 已保证隐藏行展开后按原始行与字节定位；
  导航必须复用它们。
- Search 使用独立 `SearchState`、居中 popup、`ListState`、恢复快照和 generation；
  Preview Find 使用 `PreviewFindState`，命中后调用 `reveal_folded_line`。导航结果 picker
  复用其布局和输入约定，但不能塞进 Search query/session。
- [`src/ui.rs`](../../src/ui.rs) 的 `draw` 先画基础布局，Search 激活时再 dim underlay
  和绘制 popup；`draw_footer` 有明确的退出、错误、loading、clipboard 和帮助优先级。
- `begin_content_selection` / `content_point_bounds` 已能把鼠标列映射成原始
  `ContentPoint { line, byte }`；精确导航 caret 复用同一映射。
- [`tests/app_tui_integration.rs`](../../tests/app_tui_integration.rs) 使用 Ratatui
  `TestBackend` 验证焦点、hitbox、换行、折叠、搜索和复制。
- [`scripts/e2e/fixtures.py`](../../scripts/e2e/fixtures.py) 隔离 HOME/XDG/Git 配置，
  [`scripts/e2e/scenarios.py`](../../scripts/e2e/scenarios.py) 驱动 production binary，
  `ReadOnlyOracle` 验证仓库与宿主配置未改变。

## 3. 总体架构

```text
App / input reducer
  │ NavigationRequest(generation, document_version, operation, origin)
  ▼
NavigationRuntime manager thread
  ├─ provider policy + configured executable trust boundary
  └─ LspSession[(server_root, language_family)]
       ├─ independent bounded stdin writer thread
       ├─ framed stdout reader thread
       └─ bounded stderr drainer thread
  │ NavigationCompletion(generation, document_version, result/state)
  ▼
App::poll_background
  ├─ stale rejection
  ├─ zero / one / many result reduction
  ├─ same-file reveal or cross-file Preview request
  └─ history commit only after target Preview succeeds
```

新增 `NavigationRuntime`，不把 LSP 接入现有 `WorkerRuntime`。原因是语言服务器是长生命周期
双向协议；其 initialize、notification 和 stdout 读取不能阻塞 Refresh、目录加载或 Preview。
`NavigationRuntime` 只通过有界 channel 与 `App` 通信，UI 每轮 `poll_background` 非阻塞地
drain completion。

每个 `(server_root, language_family)` 最多一个 LSP session。首期同时保留最多 8 个 session；
创建第 9 个时关闭最久未使用且无请求的 session。如果 8 个都忙，当前请求返回非阻塞状态，
不无限扩容。

## 4. 内部模型

以下均为 crate-private，除配置入口外不扩大公共 API。

```rust
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SourcePosition {
    /// 0-based logical line in the normalized Preview text.
    line: usize,
    /// 0-based UTF-8 byte offset within that logical line.
    byte: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceRange {
    start: SourcePosition,
    end: SourcePosition, // end-exclusive
}

#[derive(Clone, Debug)]
struct NavigationDocument {
    identity: ContentIdentity,
    absolute_path: PathBuf,
    disk_raw_len: u64,
    server_root: PathBuf,
    language: LanguageDescriptor,
    version: DocumentVersion,
    text: Arc<str>,
    line_index: Arc<LineIndex>,
    structure: Arc<StructureSnapshot>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct DocumentVersion(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NavigationOperation {
    Definition,
    References,
    Implementations,
    DocumentSymbols,
}

#[derive(Clone, Debug)]
struct NavigationInvocation {
    generation: u64,
    operation: NavigationOperation,
    source_identity: ContentIdentity,
    source_version: DocumentVersion,
    origin: SourcePosition,
    origin_token: SourceRange,
    restore: NavigationRestore,
    history_intent: HistoryIntent,
}

#[derive(Clone, Debug)]
struct NavigationProtocolRequest {
    key: NavigationRequestKey,
    operation: NavigationOperation,
    origin: SourcePosition,
    document: Arc<NavigationDocument>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NavigationTargetRange {
    Source(SourceRange),
    Utf16(lsp_types::Range),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NavigationTarget {
    document: ContentIdentity,
    range: NavigationTargetRange,
}

#[derive(Clone, Debug)]
enum NavigationResult {
    Locations { items: Vec<NavigationTarget> },
    Symbols { items: Vec<StructureSymbol> },
    Unavailable { reason: UnavailableReason },
    Failed { message: String },
    Cancelled,
}
```

`NavigationInvocation` 从按键起到 picker、stage 和 history commit 全程由 `App` 持有；runtime
只接收 `NavigationProtocolRequest` 并回传 request key/result，不持有 UI restore、picker 或
history。completion 必须匹配 invocation generation、源 `ContentIdentity` 和 `DocumentVersion`。
文件切换、刷新、再次发起导航、关闭 popup 和退出都会使旧 generation 失效；
`$/cancelRequest` 不能替代 reducer 的 stale check。

LSP location 使用 `Utf16` variant，本地 Document Symbol 使用 `Source` variant。该 enum 原样
贯穿 result、picker、pending transition 和 history；只有目标完整 Preview 的 `LineIndex` 可将
`Utf16` 精确规范化成 `Source`，不得提前把 UTF-16 character 当成 byte。

### 4.1 结构快照与折叠解耦

现有 `fold_regions` 泛化为后台 Preview 路径中的一次解析、三个独立 projection；按键和鼠标
foreground reducer 只消费快照，绝不重新 parse：

```rust
struct StructureSnapshot {
    source: StructureSource,
    folds: Vec<FoldRegion>,
    symbols: Vec<StructureSymbol>,
    symbols_complete: bool,
    recognizable_tokens: RecognizableTokenIndex,
}

#[derive(Clone, Debug)]
struct RecognizableTokenIndex {
    ranges: Vec<SourceRange>,
    complete: bool,
}

#[derive(Clone, Debug)]
struct StructureSymbol {
    id: SymbolId,
    name: String,
    kind: SymbolKind,
    range: SourceRange,
    selection_range: SourceRange,
    parent: Option<SymbolId>,
    detail: Option<String>,
    container: Option<String>,
}

```

`FoldRegion` 和 `FoldAnchor` 保持现有语义；symbol 不复用 fold anchor，symbol 排序或名称变化
也不能清空折叠状态。现有 fold pipeline 的解析、预算、normalize 和 all-or-nothing 返回语义
保持原样：代码复用同一棵 Tree-sitter tree，但 fold traversal 与 symbol traversal 使用独立
`WorkBudget`；Markdown 为 symbol 单独跑一个有界 event pass。symbol 提取失败、遇到无法识别
的 name shape、超过上限或预算耗尽时统一返回 `symbols=[]`、`symbols_complete=false`，不得
返回部分 symbol，也不得改变本来会得到的 folds。

fold 继续沿用当前 100,000 节点、50,000 Markdown event、4,096 region 上限和 fail-closed
行为；symbol 独立限制为 100,000 AST 节点或 50,000 Markdown event、最多 4,096 项、最大
层级 64。recognizable-token pass 也有完全独立的预算：最多访问 100,000 个 Tree-sitter node，
最多收录 65,536 个有效、非空、end-exclusive 的 allowlisted named leaf range。成功结果按
`(start, end)` 排序、去重，并验证互不重叠；成功但没有 token 时返回
`ranges=[]、complete=true`。任一候选 range 非法、UTF-8/行列转换失败、range/count 溢出、
allowlisted node 为 ERROR/MISSING、出现重叠或 traversal 预算耗尽，均 fail whole 为
`ranges=[]、complete=false`，不能保留合法 prefix。Tree-sitter parse 本身失败时 token index
同样为空且不完整；parse 成功后 fold、symbol、
token 三个 projection 中任一个失败都不能反向清空或改变另外两个 projection。结构快照不包含
reference edge、同名索引或 Definition proof。

#### 4.1.1 精确 symbol 映射

所有代码 symbol 都要求 declaration node 和它的 `name` field 非 ERROR/MISSING，name source
是非空 UTF-8；`range` 是 declaration node 的完整 end-exclusive byte range，
`selection_range` 是 name node 的完整 end-exclusive byte range。单行 declaration 也生成
symbol。没有下表所列 name field 的匿名 function/class/arrow/lambda/impl 一律不生成 symbol；
不能从赋值左侧、文本或同名节点推测名称。

| 语言 | declaration node → name field | `SymbolKind` | hierarchy |
| --- | --- | --- | --- |
| Rust | `function_item → name` | ancestor 为 `impl_item`/`trait_item` 时 Method，否则 Function | 最近的已收录 `trait_item`/`mod_item` |
| Rust | `struct_item/enum_item/union_item/trait_item/type_item → name` | Type | 最近的已收录 `mod_item` |
| Rust | `mod_item → name` | Module | 最近的已收录 `mod_item` |
| Rust | `impl_item` 无声明 name，不收录；其 fold 保持现状 | — | 内部 method 找不到已收录 parent 时为 root symbol |
| TypeScript | `function_declaration/generator_function_declaration → name` | Function | 最近已收录 Module/Type |
| TypeScript | `method_definition/abstract_method_signature/method_signature → name` | Method | 最近已收录 class/interface/module |
| TypeScript | `class_declaration/abstract_class_declaration/interface_declaration/enum_declaration/type_alias_declaration → name` | Type | 最近已收录 Module/Type |
| TypeScript | `internal_module → name` | Module | 最近已收录 Module；`ambient_declaration` 无统一 name field，明确不收录 |
| JavaScript | `function_declaration/generator_function_declaration → name` | Function | 最近已收录 class/function |
| JavaScript | `method_definition → name` | Method | 最近已收录 class |
| JavaScript | `class_declaration → name` | Type | 最近已收录 class/function |
| Python | `function_definition → name` | 最近已收录 class 的直接/透明 descendant 为 Method，否则 Function | 最近已收录 class/function；`decorated_definition` 透明 |
| Python | `class_definition → name` | Type | 最近已收录 class/function |
| Go | `function_declaration → name` | Function | root |
| Go | `method_declaration → name` | Method | root；首期不从 receiver 文本合成 parent |
| Go | `type_spec → name` | Type | root；fold 仍来自现有 `type_declaration` |

TypeScript 表同时用于 TSX；JavaScript 表同时用于 JSX/MJS/CJS。现有 fold 支持的
`function_expression`、`arrow_function`、`generator_function`、匿名 `class`、Python lambda
继续可以折叠，但不进入 Document Symbols。parent 只取 AST ancestor 中最近的已收录 symbol；
跨越未收录匿名 node 不制造 synthetic parent。

Markdown 使用独立 heading stack：`Event::Start(Tag::Heading)` 建候选，直到对应
`TagEnd::Heading` 为止；名称按顺序拼接 `Text` 和 `Code`，SoftBreak/HardBreak 转一个空格，
其他 inline event 不产生文本，最终 collapse whitespace 并 trim。没有非空文本的 heading
不收录。`selection_range` 从第一个贡献名称的 Text/Code event start 到最后一个 event end；
`range` 与该 heading section 的 fold 范围一致，即从 heading 行首到下一个同级或更高级
heading 行首之前，文件末尾用最后一行末尾。parent 是 heading-rank stack 中最近的更高级
heading。fenced code block 只生成 fold，不生成 symbol。

任何 declaration 的 byte range 不能精确转换成 `SourceRange`、parent 深度超过 64、symbol
数超过 4,096，或 symbol traversal 超预算，都使本文件的 symbols 整体为空且
`symbols_complete=false`；folds 保持独立结果。

### 4.2 Navigation caret

现有 `content_cursor_line` 只表示折叠/viewport 的逻辑行，不足以发出 LSP column。新增：

```rust
struct NavigationCaret {
    point: SourcePosition,
    preferred_display_column: usize,
}
```

- Preview 成功加载时，caret 位于第一行第一个非空白 grapheme；空文件为 `(0, 0)`。
- 普通鼠标点击通过现有 `content_point_bounds` 更新 caret，拖选和复制语义不变。
- Ctrl+单击或 Super+单击先更新 caret，再触发 Definition。
- Preview Find 命中、Document Symbol 选择和导航 target 会把 caret 移到 range start。
- `j/k` 仍按当前行为滚动一个 visual row；`sync_content_cursor_to_scroll` 同时把 caret 移到
  新顶部逻辑行的第一个非空白 grapheme。它不改变 `←/→` 的 pane focus 语义。
- Content focus 且无非空 selection 时，对 caret 所在 grapheme 加下划线；它不是文本编辑
  光标，不接受字符输入。F12 只查询后台产出的 `RecognizableTokenIndex`；index 不完整、空白、
  行尾或没有命中时显示状态且不请求 LSP。
- selection、copy 和 navigation target highlight 是三个独立状态；跳转高亮不能伪装成
  selection，否则会改变 Ctrl+C 的现有退出/复制分支。

## 5. UTF-8、UTF-16 与 URI

LSP 默认 position encoding 是 UTF-16。client initialize 只声明 `utf-16`；如果 server
明确选择 UTF-8 或 UTF-32，session 标记为不兼容，不猜测转换方式。

`LineIndex` 从发给 LSP 的完整 `NavigationDocument.text` 构建：

- 以 `\n` 切分逻辑行；CRLF 的 `\r` 不计入可定位行内容。
- internal byte 必须在 UTF-8 scalar boundary，且不超过该行内容末尾。
- internal → LSP：对 byte 前缀执行 `chars().map(char::len_utf16).sum()`。
- LSP → internal：逐 scalar 累加 UTF-16 code unit；落在 surrogate pair 中间、越过行尾或
  超出行数都返回错误，不 clamp。
- combining mark 按各自 UTF-16 unit 计算；终端 display width 不参与协议 column。
- 对 emoji、CJK、combining mark、CRLF、空行、行尾和非法半个 surrogate 建确定性单测。

内置 text Preview 只有在没有触发 `PREVIEW_MAX_BYTES` / `PREVIEW_MAX_LINES` 截断时才构造
`NavigationDocument`。后台 `execute_content` 使用 Preview 的完整 `lines` 以 `\n` 重建
只读 document；这只规范化换行，不改变四种首期语言的行/column 语义，并避免按键时重新读盘
以及 Preview/LSP 内容竞态。截断 Preview 显示
`Navigation unavailable: preview is truncated.`，不把截断文本冒充完整 didOpen。

LSP URI 边界使用 `url::Url::from_file_path` / `to_file_path`，再转换成 `lsp_types::Uri`。
只接受无 query/fragment 的 `file:` URI。服务端 location 进入 App 前必须：

1. 转为平台路径；
2. 通过 `inspect_content_path(Some(app.root), path)` 的 no-follow 普通文件检查；
3. 确认位于用户选中的 `App::root`；
4. 转换为 `ContentIdentity`。

非 file URI、工作区外路径、symlink/reparse point、目录、缺失文件和不可表示的 Windows URI
全部丢弃并计数。若响应原本非空但过滤后为空，显示
`Definition is outside the opened workspace or is not a safe file.`，不走 AST fallback。

## 6. 语言、workspace root 与 server 配置

### 6.1 语言映射

| family | 扩展名 | LSP `languageId` | 常见显式配置（不自动采用） |
| --- | --- | --- | --- |
| Rust | `.rs` | `rust` | `rust-analyzer` |
| TypeScript / JavaScript | `.ts/.mts/.cts` → `typescript`; `.tsx` → `typescriptreact`; `.js/.mjs/.cjs` → `javascript`; `.jsx` → `javascriptreact` | 按左列 | `typescript-language-server --stdio` |
| Python | `.py/.pyi` | `python` | `pyright-langserver --stdio` |
| Go | `.go` | `go` | `gopls serve` |

Markdown 只有本地 Document Symbols，不启动 Markdown LSP。不认识的扩展名首期没有 LSP，
也没有代码 definition fallback。

### 6.2 server root

对目标文件，在最新 `RepoGraph::repositories()` 中选择 `worktree` 位于 `App::root` 内、包含
文件且路径最深的已初始化仓库。没有候选时使用 `App::root`。如果工作区只是外层 Git 仓库的
子目录，不能把外层 worktree 作为 LSP root，因为那会扩大用户选中的读取范围。

session key 是规范化的 `(server_root, language_family)`。同一 nested repo 的 TS/JS 共用
一个 typescript session；不同 nested repo 不共享。Refresh 更新 RepoGraph 后，已有 session
不迁移；下一次导航若计算出不同 key，则打开新 session，旧 session 按 LRU 关闭。

### 6.3 配置文件与优先级

当前项目没有用户配置层。首期只新增一个用户级 JSON 文件，不扫描仓库内配置，也不因 PATH
中存在常见命令就生成隐式配置：

- `LATTELENS_LSP_CONFIG`：显式配置文件，必须为绝对路径；相对路径拒绝。
- macOS 默认：`$HOME/Library/Application Support/latte-lens/lsp.json`
- Windows 默认：`%APPDATA%\\latte-lens\\lsp.json`
- 其他 Unix：绝对 `$XDG_CONFIG_HOME/latte-lens/lsp.json`，否则
  `$HOME/.config/latte-lens/lsp.json`

显式路径不存在或 JSON 非法时，LSP 整体不可用并显示状态；默认文件不存在不是错误，但此时
LSP 保持 Disabled。只有全局 `enabled=true` 且对应 family 显式 entry 的 `enabled=true`、
`program` 非空时才获得启动授权。配置 schema 固定为：

```json
{
  "enabled": true,
  "servers": {
    "rust": {
      "enabled": true,
      "program": "/opt/bin/rust-analyzer",
      "args": []
    },
    "typescript": {
      "enabled": true,
      "program": "typescript-language-server",
      "args": ["--stdio"]
    },
    "python": { "enabled": false },
    "go": {
      "enabled": true,
      "program": "gopls",
      "args": ["serve"]
    }
  }
}
```

- 配置读取也使用 no-follow/reparse-safe 的普通文件边界，且独立于 `App::root`；先读取最多
  65,537 bytes，只有在 EOF 且长度不超过 64 KiB 时才解析。内容必须是严格 UTF-8，不接受 BOM。
- parser 使用自定义 serde visitor，在所有 object 层拒绝 duplicate key，并对 top-level、family
  map 和 server entry 全部执行 `deny_unknown_fields`：top-level 只允许 `enabled/servers`，
  `servers` 只允许 `rust/typescript/python/go`，entry 只允许 `enabled/program/args`。最多 4 个
  server entry；缺失的 global/family `enabled` 都按 `false`，不能用宽松默认值获得启动授权。
- `program` 长度必须为 1..=4,096 bytes 且不含 NUL；`args` 最多 16 项，每项最多 4,096 bytes、
  累计最多 16 KiB，且均不含 NUL。任何文件大小、UTF-8、schema、duplicate、字符串或数组上限
  失败都使整个配置 disabled，产生最多 240 个清理后字符的 warning，并且绝不进入 PATH 解析。
- `enabled=false` 全局关闭；family 的 `enabled=false` 只关闭该 family。缺失 family 等同
  `enabled=false`，没有 default/discover fallback。
- `program` 只允许绝对路径或无路径分隔符的 basename。拒绝 `./server`、`../server` 和其他
  相对路径，避免 child cwd 指向仓库后执行仓库文件。
- basename 只在该显式 entry 已授权时由 Latte Lens 遍历 `PATH` 解析；PATH 不用于发现
  family。仅遍历绝对 PATH entry，忽略空项、`.` 和相对项，并始终解析成绝对 executable。
- 绝对 program 和 basename 解析结果使用同一 trust validation：逐级 `symlink_metadata`
  拒绝 symlink；Windows 逐级拒绝 reparse point；最终必须是 regular file。随后 canonicalize，
  再次确认仍为同一普通文件且不位于 `App::root` 内。位于工作区内的 executable 即使用户写进
  用户配置也拒绝，避免打开仓库后执行仓库内容。
- Unix/macOS 还要求最终文件任一 execute bit 已设置。Windows basename 只尝试原名和
  `.exe`，绝不采用当前目录；绝对路径只接受 `.exe` 或无扩展名的 native executable，显式
  拒绝大小写不敏感的 `.cmd`、`.bat`、`.com`、`.ps1` 以及任何由 shell/interpreter 关联启动
  的文件。Windows reparse point 校验使用现有 `windows-sys` 文件属性。
- 绝不通过 shell 拼接命令。`program` 传给 `Command::new`，`args` 逐项传给 `args`。
- 首期不接受配置中的环境变量、workspace-specific command、initializationOptions 或任意
  server command。子进程继承 Latte Lens 环境，cwd 固定为 `server_root`。
- 通过 trust validation 也只代表可启动；首次 F12 / Shift+F12 / Ctrl+F12 才 spawn。

为保持 embedding 和测试确定性，增加 additive
`App::with_options(path, registry, AppOptions)`：

```rust
pub struct AppOptions {
    pub navigation: NavigationSettings,
    pub navigation_config_warning: Option<String>,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            navigation: NavigationSettings::disabled(),
            navigation_config_warning: None,
        }
    }
}
```

`App::new` 与 `App::with_preview_registry` 均使用 `AppOptions::default()`，不得读取 HOME、XDG、
APPDATA 或 PATH，也永远默认 navigation disabled。production `main` 先 canonicalize CLI
workspace，显式调用 `NavigationSettings::load_user_config(&workspace_root)` 读取用户配置、解析
basename 并执行上述 trust validation，再调用 public additive `App::with_options`。
`App::with_options` 只接收已解析成绝对路径的 trusted settings，并在 canonical App root 已知后
再次执行不含 PATH 的防御性校验。配置缺失/非法返回 disabled settings 加 warning，不能阻止
TUI 启动。测试和 embedder 显式注入 `NavigationSettings::disabled()` 或 fake settings，不接触
宿主配置。现有构造函数和 `PreviewProvider` 签名不变。

## 7. Provider 优先级

### 7.1 Definition

顺序必须固定：

1. 若对应 family 没有显式 enabled/trusted 配置，显示
   `No configured Rust language server.` 并保持原位。
2. 已配置 session 在 Backoff/Failed 或 initialize 未声明 Definition capability 时显示精确状态，
   不请求、不降级。
3. 已配置 session 为 Starting 时只保留最新 generation，Ready 后请求
   `textDocument/definition`。
4. LSP 返回合法 null/空结果时显示 `No definition found.`；合法一个/多个结果走 direct/picker。
5. 工作区外/不安全结果、协议错误、超时、崩溃或 malformed response 都显示对应状态并保持
   原位。

Tree-sitter、pulldown-cmark、Search、Document Symbols 和同名 workspace symbol 在任何状态下
都不是 Definition provider。没有 `LocalDefinitionProof`、same-file name lookup 或伪语义索引。

### 7.2 References 与 Implementations

只使用显式配置且 Ready 的 LSP。server 未配置、未声明 capability 或请求失败时保持原位并
显示状态；不使用 AST 和 workspace 搜索降级。

### 7.3 Document Symbols

Document Symbols 属于本地结构导航，不是 semantic definition：

1. `StructureSnapshot.symbols_complete=true` 时立即使用 Tree-sitter / pulldown-cmark symbols；
2. 本地语言不支持或结构预算不完整，且对应 family 已显式配置、LSP Ready 并支持
   `documentSymbol` 时请求 LSP；
3. 两者都不可用时显示状态。

Tree-sitter symbols 优先，保证 `@` 不因 LSP 启动变慢；LSP `documentSymbol` 是结构解析不可用
时的 fallback。fold 始终只消费 `StructureSnapshot.folds`，从不消费 LSP symbol ranges。

## 8. LSP stdio 协议

### 8.1 framing 与 JSON-RPC

使用小型自有 transport，而不是引入 async runtime 或完整 editor framework：

- stdout reader 按 ASCII case-insensitive header name 接受恰好一个
  `Content-Length: N\r\n`，空行 `\r\n` 结束 header。值 trim 后必须是无符号十进制且
  `1..=4 MiB`；缺失、重复（即使值相同）、负数、溢出或非十进制都使 session 失败。
- header 连同终止空行最大 8 KiB，单个 JSON body 最大 4 MiB。可选 `Content-Type` 最多出现一次，
  media type 必须为 ASCII case-insensitive 的 `application/vscode-jsonrpc`；允许没有 charset，或
  恰好一个 ASCII case-insensitive 的 `charset=utf-8` / `charset=utf8` 参数。重复 Content-Type、
  重复 charset、其他 charset、其他参数或 malformed 参数都使 session 失败；其他未知 header
  可以忽略。
- 支持一次 read 的半个 frame、多个 frame、header/body 分片；不得假设 read 边界等于消息边界。
- JSON-RPC id 只规范化为 `RpcId::Signed(i32) | RpcId::String(String)`。inbound server request
  只接受 signed `i32` integer 或 string；负整数和 string id 均在 response 中原样 echo。
  fractional number 或超出 signed `i32` 的 server-request id 回复 `-32600 Invalid Request`、
  `id:null`；`null` id 只用于 Parse error / Invalid Request，不能匹配 client pending request。
- client allocator 从 0 开始，只生成单调的 `0..=i32::MAX`，其中 `i32::MAX` 可以且只能分配一次；
  下一次需要 id 时产生 `RequestIdExhausted`，fail pending 后进入 forced cleanup/restart，新的
  session/epoch 把 allocator 重置为 0，绝不 wrap 或在旧 session 复用 id。transport event 必须携带
  session epoch；旧 epoch 的迟到 response 在匹配前被拒绝并计数，绝不能命中新 session 的重用 id。
  当前 epoch 已 cancel/timeout 的 id 保留在最多 64 项的 bounded retired-id set，迟到 response 只会
  命中并移除 tombstone；插入第 65 项时 forced restart，不能用无界 stale-id 集合掩盖 response 错误。
- 每个 envelope 必须是单个 JSON object 且 `jsonrpc == "2.0"`。server request 必须有 string
  `method` 和 string/integer id；notification 不含 id；response 必须有匹配 pending 的 id，且
  `result` 与 `error` 恰有一个。当前 epoch 中 id 为 fraction、越过 signed `i32`、`null`、shape
  非法或没有匹配 pending request 的 response 都是 invalid response，立即 fail session；只有明确
  属于旧 epoch 的 response 才作为 stale 拒绝而不污染新 session。
- body 不是合法 JSON 时，尽力写一次 `-32700 Parse error`、`id:null`，随后立即 fail session；
  合法 JSON 但 server request/notification envelope 非法时回复 `-32600 Invalid Request`，连续
  3 个此类非法 inbound envelope 后 fail session。response 只要违反上一条 response contract 就
  立即 fail session，不适用三次容错，也不向 response 再回 error。已知 server request 的 params
  缺失或 shape 错误回复 `-32602 Invalid params`，不会 panic 或执行默认动作。
- stdout 只能是 LSP frame。协议外 stdout 视为 malformed；stderr 由独立线程持续 drain，
  每个 session 只保留清理控制字符后的最后 16 KiB，防止 child 因 pipe 填满而死锁。

每个 child 的 stdin 由独立 bounded writer thread 唯一持有；manager 只序列化完整 frame 后
`try_send`，绝不在 manager thread 上 write/flush。writer 使用 `sync_channel(16)`，该 session
所有排队 frame 同时受 1 MiB writer cap 和下述 payload permits 约束；单帧无法取得 permit、
channel 满、write/flush 失败都回传一个有界 control event并触发 forced cleanup。成功 write+flush
后 writer 释放 permit 并回传 ack；manager 不等待同步 ack，因而一个阻塞的 child stdin 不会阻塞
其他 session、timeout、cancel 或 App input。writer→manager control event 使用每 session
`sync_channel(16)`；event channel 满也视为 session 消费失速并 forced cleanup，不无限积压 ack。

所有 payload allocation 同时取得 checked 的每 session 24 MiB permit 与全 runtime 128 MiB
global permit；checked add/multiply、capacity 增长或任一 permit 获取失败都 fail whole，绝不交付
prefix 或 `truncated=true`。记账覆盖 stdout reader 正在构造的 frame、stdout channel 的 2 个 queued
frame、manager 当前 frame、parser allocation、normalized result、App completion queue、writer frame、
16 KiB stderr tail，以及 `NavigationDocument` 的 text/`LineIndex`/`RecognizableTokenIndex` 等
line/token allocation。`Arc` 共享内容只记一次，但 permit owner 必须活到最后一个 owner drop。

单 session 的可审计峰值 state component 公式是
`4 MiB + 8 MiB + 4 MiB + 2 MiB + 1 MiB = 19 MiB`：依次对应 in-progress reader、2 个 queued
frame、manager frame、parser data、单个 normalized
result；余下 5 MiB 才可由上述 completion、writer、stderr、document/line/token 等 permit-bound
allocation 竞争使用。8 个 session 可以存在，但 128 MiB global permit 会在总 payload 到顶前拒绝
新的增长，不能按 `8 × 24 MiB` 实际分配。location 最多 2,000，document symbol 最多 4,096，
symbol 的 name/detail/container 内容累计最多 512 KiB，单个 normalized result 最多 1 MiB。

permit token 随 buffer/result 的所有权在 reader → queue → manager → parser/result → completion/App
之间 transfer，旧 owner 在 transfer 后不得保留重复 charge 或未记账 clone；drop/cancel/stale/failure
在该 owner 释放实际 allocation 时同步 release permit。envelope parser 只借用 manager frame，并用
自定义 bounded serde visitors 直接构造受限类型；禁止先建 unrestricted `serde_json::Value` 或无界
map/string/vector。parser 的任何 shape/count/byte/accounting 失败都 fail whole response/session。

最多 8 个 session；App→manager command `sync_channel(32)`；每 session pending JSON-RPC request 64；
App completion `VecDeque(16)`。stdout frame channel 满表示协议消费者失速，直接 fail session，不能
丢 frame。App command channel 满时，仅 `NavigationRequest` 可放入一个 “latest wins” overflow slot
并覆盖更旧 generation；Shutdown/Cancel 使用独立 atomic wake flag，不能丢。completion 入队前先删
最低 generation 的 stale completion 并释放其 permit；如果当前 generation 仍无法同时满足 16 项和
session/global permits，全量结果改为小型 `Failed("navigation completion queue is full")`，App 必须
清除 loading。pending map 将插入第 65 项时先失败当前请求并重启 session。任何 overflow 路径都
不得交付 partial result。

### 8.2 initialize 与能力

spawn 后 5 秒内完成：

1. `initialize`：带 `processId`、`clientInfo = latte-lens`、`rootUri`、单个
   `workspaceFolders`，并只声明 Definition、References、Implementation、DocumentSymbol、
   workspaceFolders、UTF-16 和 cancellation 所需 capability；所有 dynamicRegistration=false。
2. 验证 `InitializeResult.capabilities.position_encoding` 为缺省/UTF-16。
3. 归一化 capability：

```rust
struct NavigationCapabilities {
    definition: bool,
    references: bool,
    implementations: bool,
    document_symbols: bool,
    text_document_sync: TextSyncCapability,
}
```

4. 发送 `initialized` notification 后，才按 sync matrix 在需要时发送 didOpen，随后发送用户请求。

`ServerState` 明确区分：

```rust
enum ServerState {
    Disabled { reason: String },
    Unavailable { reason: String },
    Starting { since: Instant },
    Ready { capabilities: NavigationCapabilities },
    Backoff { attempt: u8, retry_at: Instant, error: String },
    Failed { error: String },
    StoppingShutdown,
    StoppingForced,
}
```

用户在 Starting 时的新请求覆盖旧 pending 请求；Ready 后只执行最新 generation。

### 8.3 document sync

Latte Lens 是只读 viewer，不产生 didChange：

- 将 `TextDocumentSyncCapability` 归一化成独立的 `open_close: bool` 与
  `change: None | Full | Incremental`，矩阵没有其他默认值：

  | server capability shape | `open_close` | `change` |
  | --- | --- | --- |
  | capability 缺失或 kind 简写 `None` | `false` | `None` |
  | kind 简写 `Full` | `true` | `Full` |
  | kind 简写 `Incremental` | `true` | `Incremental` |
  | options object | `open_close.unwrap_or(false)` | `change.unwrap_or(None)` |

- Latte Lens 在所有矩阵分支都绝不发送 didChange。`open_close=false` 时也不发送 didOpen/didClose，
  直接对 file URI 请求；`change=None` 与 `open_close` 仍是两个独立值。
- `open_close=true` 时，对当前需要请求的 document 发送一次 `textDocument/didOpen`，内容
  来自完整 `NavigationDocument.text`。切换 document 或 `DocumentVersion` 改变时先 didClose
  旧 URI，再 didOpen 新快照。
- `change=Full` 或 `Incremental` 都与只读 snapshot 兼容：Latte Lens 没有编辑，所以不会发送
  didChange，不能把 Incremental 错判成不兼容。
- 如果 `change=None` **或** `open_close=false`，则每次发送 definition/references/implementation/
  documentSymbol 前，把源文件 revalidation 用 `try_send` 投递到与 manager/Preview worker 隔离的
  disk-snapshot lane。该 lane 在整个 navigation runtime 内恰好允许 1 个 active + 1 个 queued job；
  不等待 queue，满时本地失败、显示状态且不发送 LSP request。每个 job deadline 是
  `min(500 ms, request remaining deadline)`；deadline/cancel/generation failure 都在本地完成，迟到结果
  只按 job generation 丢弃，绝不能随后补发 LSP request。
- disk-snapshot job 复用 Preview 的 `inspect_content_path` no-follow/reparse-safe 普通文件边界，
  在 open 前以及每次至多 64 KiB 的 read 前检查 cancel、generation 和 deadline；重新打开同一绝对
  路径，最多读取 `min(PREVIEW_MAX_BYTES + 1, NavigationDocument.disk_raw_len + 1)` bytes，并要求
  到达 EOF、raw length 仍等于 `disk_raw_len`、仍是同一 `ContentIdentity`、严格 UTF-8 且未触发
  Preview byte/line cap。随后执行与 `execute_content` 完全相同的 BOM 与 CRLF/LF 规范化，再与
  `NavigationDocument.text` byte-for-byte 比较。任一不一致返回 `StaleDocument`，不发送 LSP
  request，并显示 `File changed on disk; refresh Preview.`。测试必须注入 opener/reader/clock/cancel，
  不用真实 sleep 才能验证 active/queued、queue-full、500 ms cap、分块 cancel 和 late-result rejection。
  `open_close=true` 且 change 支持 Full/Incremental、已经 didOpen 精确 snapshot 时不额外重读当前
  document；服务端返回的每个目标仍执行 URI/no-follow safety 校验。
- didOpen version 使用 checked `i32`。到达 `i32::MAX` 后先 didClose 并 orderly restart
  session，新 session 从 1 开始；绝不 wrap 或复用旧 session version。
- 不把 Preview 截断内容发送给 server。

### 8.4 请求与响应

- Definition：`textDocument/definition` 的 `result` 接受 `null`、`Location`、
  `Location[]` 或 `LocationLink[]`；`LocationLink` 优先 `targetSelectionRange`。
- References：`textDocument/references` 使用 `includeDeclaration=true`，`result` 只接受
  `null` 或 `Location[]`，非空结果始终进 picker。
- Implementations：`textDocument/implementation` 接受与 Definition 完全相同的 nullable shape。
- Document Symbols：`textDocument/documentSymbol` 接受 `null`、nested `DocumentSymbol[]` 或
  flat `SymbolInformation[]`。`null`/空数组都归一化为空 symbol；非空数组必须先判定且全程保持
  单一 variant，不能逐项猜测。
- `DocumentSymbol[]` 隐含属于当前请求 document：按 depth-first preorder 展平，真实 nested parent
  写入 `parent`；`name.trim()` 必须非空，`detail` 只作 picker display metadata。`range` 与
  `selectionRange` 都必须经当前 `NavigationDocument.line_index` 从 UTF-16 完整转换成 end-exclusive
  `SourceRange`，且 selection 必须包含于 range。nested 最大深度 64；未知/未映射的 LSP
  `SymbolKind` 统一为内部 `Other`，不根据名称推断。
- `SymbolInformation[]` 保持 flat：每项 `parent=None`，`containerName` 只作 display metadata，不能
  合成 hierarchy。每个 location URI 经规范化后必须等于当前请求 document，并通过同一
  URI/no-follow safety boundary；name 和 UTF-16 range 使用与 nested variant 相同的非空、边界和
  Source 转换规则。任一 cross-file URI 都使整个 symbol response 失败。
- 单个 name/detail/container 最多 4 KiB，三者内容累计最多 512 KiB。混合 variant、invalid item、
  非法/越界 range、selection containment 失败、cross-file、超过深度/count/1 MiB byte cap 都使
  整个 request 失败；不保留合法 prefix，也不构造 synthetic parent。
- 结果先过滤安全路径，再按 workspace-relative path、start line、start character、end 排序并
  去重。最多接受 2,000 个 location 或 4,096 个 symbol；超过任一 count/byte cap 都返回小型
  `Failed`，不设置 truncated、不返回部分结果。

initialize timeout 5 秒，普通导航 3 秒，shutdown 1 秒。新请求、Esc、内容 generation 变化或
App 退出时，对仍 pending 的用户 request 发送 `$/cancelRequest` 并立即以本地 generation
完成 `Cancelled`；当前 epoch 的 id 进入 bounded retired-id set，迟到 response 只能命中 tombstone
后被拒绝，其他 unmatched response 仍按 invalid response fail session。导航 timeout 同样先 cancel，再返回
Failed，但 session 可继续；initialize timeout 直接进入 cleanup/backoff。收到 server 的
`RequestCancelled` 映射为 `Cancelled`，其他 JSON-RPC error 保留清理后最长 240 字符的 message。
任一 stdin write 或 flush 失败由 writer event 立即 fail 所有 pending，并进入 forced cleanup。

### 8.5 server 发给 client 的消息

必须回应的 server request：

| method | response |
| --- | --- |
| `workspace/configuration` | 与 items 等长的 `null` 数组 |
| `workspace/workspaceFolders` | 当前唯一 folder |
| `client/registerCapability` / `client/unregisterCapability` | `null`，但不改变静态 capability |
| `window/workDoneProgress/create` | `null` |
| `workspace/applyEdit` | `{ "applied": false, "failureReason": "Latte Lens is read-only" }` |
| `window/showMessageRequest` | `null` |
| 未知 request | JSON-RPC `-32601 Method not found` |

`window/logMessage`、`window/showMessage` 可转成清理后的 debug/status；`$/progress`、telemetry
和 `textDocument/publishDiagnostics` 有界解析后丢弃。首期不渲染 diagnostics，也不执行
`workspace/executeCommand`、Command、CodeAction 或 edit。

### 8.6 crash、backoff 与清理

- initialize 或运行期退出会失败当前请求并进入指数 backoff：1、2、4、8、30 秒。
- 连续 5 次启动/崩溃后进入 `Failed`，本进程内不再自动启动；状态提示用户检查配置并重启
  Latte Lens。稳定 Ready 60 秒后清零连续失败计数。
- backoff 期间按导航键只显示剩余时间，不密集重启。
- spawner 返回 `OwnedProcessTree`，而不是裸 `Child`。Unix 在 spawn 时建立新的 process group，
  cleanup 对整个 process group 发 TERM/KILL 并单独 reap direct child；Windows 用现有
  `windows-sys` features 以 suspended 状态创建 child，先把它分配给启用
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object，再 resume，cleanup 通过终止/关闭 Job Object
  杀死整个 tree 并单独等待 direct child。所有留在 parent 一侧的 stdin/stdout/stderr pipe end
  在 spawn 后明确设为 non-inheritable，避免后续 child 继承 Latte Lens 持有的端点。
- 每个 stdin writer、stdout reader、stderr drainer 在退出前先关闭自己唯一持有的 pipe handle，
  然后通过独立有界 lifecycle channel 发一次 `IoThreadDone(kind, session_epoch)`。manager 只有收到
  对应 epoch/kind 的 `IoThreadDone` 后才 join 那个 handle；deadline 本身绝不能触发无条件 join。
  lifecycle channel 为三个 terminal event 保留容量，不与可丢/可满的 writer ack channel 共用。
- stdout EOF 在正常 `StoppingShutdown` 之前发生时视为 crash：fail pending、关闭 bounded writer
  sender并设置 cancel，然后进入 forced cleanup；stderr EOF 只记录对应 `IoThreadDone`，不单独推进
  session state。header/body 中途 EOF、stdin write/flush failure 和 malformed frame 走同一个
  forced-cleanup transition。
- 只有 `Ready → StoppingShutdown` 可走 orderly cleanup：manager 把 `shutdown` request 送入 bounded
  writer，收到对应 response（且 write ack）或到 1 秒 deadline 后再尝试 `exit`；随后关闭 writer
  sender并通知 cancel。若 direct child 在 250 ms 内没有退出，或任一 orderly deadline 到期，立即
  升级为 forced cleanup；不能在 deadline 上直接 join pipe thread。
- `Starting`、initialize failure、Backoff/Failed 转换、crash、malformed frame、`RequestIdExhausted`、
  writer queue/permit/write/flush failure 都走 `StoppingForced`，不得发送 shutdown/exit：先关闭
  bounded writer sender并设置 cancel，随后由 `OwnedProcessTree` 终止**整个 process tree**，确认
  tree termination 并 reap direct child，之后才等待 pipe EOF/`IoThreadDone`。收到各 done event 后
  分别 join 已结束 thread；不得先等 EOF、不得只 kill direct child，也不得在共同 deadline 后盲 join。
- 只有 process tree 已终止、direct child 已 reap、三个 pipe owner 都已报告 `IoThreadDone` 且对应
  handle 均已 join，才能释放 `OwnedProcessTree` 并进入 `Backoff` 或 `Failed`；这两个状态绝不携带
  live descendant、child、pipe 或 I/O thread。tree termination/reap 失败时保持 cleanup state 并报告
  fatal lifecycle error，不能把未完成清理伪装成 Backoff。
- `NavigationRuntime::drop` 对所有 session 并发应用同一状态机：Ready session 尝试 orderly，其他
  session forced；任一超时升级为 tree termination，但 join 仍逐个等待 matching `IoThreadDone`，
  不能按 session 串行 cleanup，也不能留下 zombie、descendant、I/O thread 或持有 App channel。

## 9. 交互设计

### 9.1 快捷键

只在 Content focus、`ContentMode::Preview`、无 Search/Find/navigation picker 时生效：

| 按键 | 行为 |
| --- | --- |
| `F12` + modifiers 恰为 `NONE` | Definition |
| `F12` + modifiers 恰为 `SHIFT` | References |
| `F12` + modifiers 恰为 `CONTROL` | Implementations |
| `Left` / `Right` + modifiers 恰为 `ALT` | Back / Forward |
| `Char('@')` + modifiers 为 `NONE` 或 `SHIFT` | Document Symbols；兼容终端是否保留 Shift |
| 左键 + modifiers 恰为 `CONTROL` 或恰为 `SUPER` | 在精确鼠标 point 请求 Definition |
| 任何额外 `ALT/SUPER/CONTROL/SHIFT` 组合 | 忽略，不做近似匹配 |

不引入 `g d` chord，因为 Content 中 `g` 当前是 Home，Tree 中 `g` 当前选择首行；首期不改变
既有单键语义。README 的 controls 表在实现阶段同步更新。

如果焦点不在 Preview，F12 不切换 pane，只显示 `Focus Preview to navigate.`。Diff 中完全不发
请求。Navigation loading 时 UI 继续接收滚动、焦点和刷新输入；再次请求会取消前一个。

三种 semantic operation 只有 caret/mouse byte 命中后台 `StructureSnapshot` 已收录的“最小 named
leaf、end-exclusive range”才创建 `NavigationInvocation`；不从 whitespace、行尾或 token end 向
相邻 token 吸附。首期 allowlist 固定为：

| family | Tree-sitter leaf kind |
| --- | --- |
| Rust | `identifier`, `type_identifier`, `field_identifier` |
| TypeScript / JavaScript | `identifier`, `property_identifier`, `private_property_identifier`, `type_identifier`, `shorthand_property_identifier_pattern` |
| Python | `identifier` |
| Go | `identifier`, `field_identifier`, `type_identifier`, `package_identifier` |

F12 与 Ctrl/Super click 都只能在已排序、去重、互不重叠的 ranges 上使用 `partition_point`/二分查找：
取最后一个满足 `range.start <= caret` 的候选后，只以 `range.start <= caret < range.end` 判定命中；
禁止在 foreground 重新 Tree-sitter parse、线性扫描 AST/ranges 或向相邻 token 吸附。index
`complete=false` 时显示 `Navigation token index is incomplete; refresh Preview.`；完整 index 无命中
（包括成功的零 token、Markdown、whitespace、行尾和 token end）时显示
`No navigable token at caret.`。两种情况都不创建 invocation、不启动 session、不发送 LSP request。
该 allowlist 只决定用户是否点在一个源码 token 上，不产生 definition edge、同名查找或任何 AST
semantic fallback。`@` Document Symbols 不要求 caret token。Ctrl/Super click 使用鼠标命中的精确
source byte 和同一 end-exclusive 规则。

Esc 在 `handle_key` 顶层按固定顺序只消费一个状态：navigation picker → Preview Find/Search
（沿用各自 close/restore）→ 当前 generation 的 pending navigation cancel/restore → 现有双击
退出确认。其他按键仍由 Find/Search 优先消费。

鼠标 Down 先处理 popup/Find，再确认位于 `content_inner`。Preview fold gutter hit-test 必须先于
modified-click；即使带 Ctrl/Super，点击 `▾/▸` 也只折叠。非 gutter 且位于真实（非 synthetic）
Preview text row 时，恰好 Ctrl 或恰好 Super 的左键更新 caret、清除零长度 selection 并发起
Definition，不进入 `begin_content_selection`；其余左键最后才走现有 selection/drag 路径。

### 9.2 zero、one、many

- Definition / Implementations：0 个显示状态，1 个直接跳，多个打开 picker。
- References：1 个或多个都打开 picker，避免一个 reference 意外把用户带走。
- Document Symbols：始终打开 picker，保持父子缩进；没有 symbol 显示状态。

picker 使用 Search popup 的 `search_popup_area`、dim underlay、`ListState` 和 mouse hitbox 模式，
但状态独立：

```rust
struct NavigationPickerState {
    title: String,
    invocation: NavigationInvocation,
    results: Vec<NavigationPickerItem>,
    list_state: ListState,
    return_focus: FocusPane,
}
```

每行固定显示 `path:(start.line + 1):(start UTF-16 character + 1)`，line/column 都是 1-based。
`NavigationTargetRange::Utf16` 直接使用协议 character；`Source` 必须用该 target document 已加载的
`LineIndex` 从 source byte 转回 UTF-16 后再构造 picker item，绝不显示 UTF-8 byte offset 或终端
display width。LSP 能给 container/name 时显示第二行，不能为了 snippet 同步读盘。
`↑/↓/PageUp/PageDown` 移动，Enter 接受，Esc 关闭，单击选择、400ms 内双击接受。移动 selection
不预览文件，只有 Enter/双击才改变内容。

### 9.3 target reveal 与历史

```rust
struct NavigationHistoryEntry {
    target: NavigationTarget,
    viewport: ContentViewportRestore,
}

struct NavigationHistory {
    back: VecDeque<NavigationHistoryEntry>,
    forward: VecDeque<NavigationHistoryEntry>,
    pending: Option<PendingHistoryTransition>,
}

struct PendingHistoryTransition {
    invocation: NavigationInvocation,
    staged_content_generation: Option<u64>,
    direction: HistoryDirection,
    target: NavigationTarget,
    proposed_back: VecDeque<NavigationHistoryEntry>,
    proposed_forward: VecDeque<NavigationHistoryEntry>,
}
```

`NavigationRestore` 属于 App-owned `NavigationInvocation`，在命令 invocation 时立即捕获，而
不是收到 LSP 结果后才捕获。它包含：
tree scope、`TreeState` selection/offset、All Files/Git Changes identity、pending tree path、focus；
content lines/highlights/mode/provider/identity/navigation document、logical viewport、horizontal
scroll、selection、clipboard status、Preview Find、diff annotation；fold source/regions/collapsed set、
content cursor、NavigationCaret、target highlight、content success/loading；navigation status、back/
forward stacks。logical viewport 精确记录当前顶部 `ContentVisualRow` 的 `(line_index,
byte_range.start, synthetic, effective_scroll)`；history origin 记录 invocation-time caret range 和
该 viewport，而不是请求完成时可能已滚动的位置。

back/forward 各最多 128 项。接受 target 时：

1. picker 和 pending 都持有完整的 App-owned invocation，并保存“成功后才采用”的 proposed
   back/forward；此时不
   pop/push stack。picker item、pending target 与 history entry 都保留
   `NavigationTargetRange`；成功 commit 后新 history target 存成 `Source` variant。
2. 同文件：调用 `reveal_folded_line`、`scroll_to_logical_line(line, byte)`，设置 caret 和独立
   navigation highlight；同一 reducer turn 校验 generation 后才替换 stacks，否则从 restore
   原子还原。
3. 跨文件不得调用会立即 `reset_content` 的普通 `request_content`。给 `ContentRequest` 增加
   `ContentPurpose::NavigationStage`：后台照常生成 `ContentSnapshot`，App 只把 completion 放在
   pending transaction，不改变当前 tree/content/fold。
4. staged snapshot 成功后，先校验 navigation/content generation、URI safety、未截断
   NavigationDocument 和 UTF-16 range；全部通过才在一个 reducer turn 内提交 target content、
   `apply_tree_scope(AllFiles)`/tree identity、ancestor reveal、fold reveal、caret/highlight、viewport
   与 proposed history。目录尚未扫描完时只在 commit 后发 directory requests。
5. Preview error、cancel、timeout、非法/越界 range、安全过滤或仍由当前 transaction 持有的
   stale completion，都清除 picker/pending/status 并从 `NavigationRestore` 原子恢复全部状态，
   不留下 Loading 文本、tree selection、fold 或 stack 的半次修改。
6. 如果 completion stale 是因为更新的 navigation generation 已接管，旧 completion 仅丢弃，
   不用旧 restore 覆盖新 transaction；新 transaction 在创建时负责先 cancel/restore 前一个。

目标文件可能尚未加载到浅层 tree；成功 commit 后沿用 `reveal_all_files_selection` 的 ancestor
expansion 和 directory request，最终目录 completion 把 selection 对齐目标。stage 失败前不改
tree expansion。

Back/Forward 使用同一 pending transition：proposed stacks 描述成功后的 pop/push，但 commit
前真实 stack 不变。目标已删除或不安全时恢复 invocation snapshot，stack 不弹出。新普通跳转
只有成功后才清空 forward。

目标 range 用 `HighlightKind::NavigationTarget` 渲染，保持到下一次普通点击、内容请求或导航
请求；不写进 `content_selection`，复制仍基于原始 source bytes。折叠展开只移除包含目标行的
ancestor anchors，不展开无关 nested folds。换行后仍通过 `ContentVisualRow` 映射到含 target
byte 的 visual row，行号保持原始逻辑行号。

### 9.4 footer 状态

新增独立 `NavigationStatus { level, message, expires_at }`，不复用 `last_error`。footer 优先级：

1. active Find/Search/navigation picker 的专用 help；
2. quit confirmation；
3. `last_error`；
4. Refresh/Directory/Content loading；
5. navigation Starting/Requesting/status；
6. clipboard；
7. repository partial/error；
8. 普通 help。

固定消息示例：

- `Starting rust-analyzer…`
- `Finding definition…`
- `No configured Rust language server.`
- `rust-analyzer does not provide implementations.`
- `No definition found.`
- `Definition target is outside the opened workspace.`
- `Navigation unavailable: preview is truncated.`
- `rust-analyzer timed out while finding references.`
- `rust-analyzer stopped unexpectedly; retrying in 4s.`

success/info 4 秒后消失；Unavailable/Failed 8 秒后消失。任一新导航请求先清除旧状态。消息在
渲染前移除 C0 control 和 ANSI escape，并按 footer width 截断。

## 10. 依赖与 MSRV

采用 std thread/channel/process + 小型 framing transport，不引入 Tokio、tower-lsp 或
async-lsp。新增直接依赖固定为：

```toml
lsp-types = "=0.97.0"
serde = { version = "=1.0.228", features = ["derive"] }
serde_json = "=1.0.150"
url = "=2.5.8"
```

`lsp-types` 只提供协议 data type，不提供进程生命周期；`url` 专门承担跨平台 file URI。
上述精确组合已用 `cargo +1.88.0 check` 做最小编译探针，保持 `rust-version = 1.88` 与 Rust
2024 edition。实现后 `Cargo.lock` 必须锁定 transitive 版本，并在实际仓库再次执行
`cargo +1.88.0 check --locked`。若 lock resolver 选出高于 1.88 的 transitive crate，必须降级
该 crate，不能提高项目 MSRV。

## 11. 文件与接口改动

按以下边界实现，避免把协议代码塞入 `App`：

| 文件 | 改动 |
| --- | --- |
| `Cargo.toml` / `Cargo.lock` | 加入上节四个精确依赖 |
| `src/navigation.rs` | invocation/Source-or-Utf16 target、LineIndex、语言映射、严格有界 trusted 配置、provider policy、history、picker item、NavigationDocument payload permit ownership |
| `src/lsp.rs` | 4 MiB stdio framing、`RpcId::Signed(i32)`/string、borrowed bounded visitors、24 MiB/session + 128 MiB/global permits、精确 sync matrix 与 1+1 disk revalidation lane、独立 bounded stdin writer、session actor、`OwnedProcessTree`/`IoThreadDone` cleanup、URI 安全转换 |
| `src/folding.rs` | 增加互相独立的 symbol 与 `RecognizableTokenIndex` pass；保留现有 fold/anchor budget 与三路 all-or-nothing 隔离 |
| `src/runtime.rs` | `ContentSnapshot` 增加后台产生的 crate-private navigation document/structure/token index；增加非破坏 `NavigationStage` purpose；计算受限 server root |
| `src/app.rs` | App-owned NavigationInvocation、generation、token `partition_point`、caret、picker、status、完整 restore/staged history、精确 key/mouse/Esc reducer；不 foreground parse |
| `src/ui.rs` | picker、caret/target overlay、footer 状态和 hitbox；无 I/O |
| `src/main.rs` | 加载用户 LSP 配置并传 `AppOptions` |
| `src/lib.rs` | 注册模块；只公开构造所需 options/settings，其他保持 crate-private |
| `tests/app_tui_integration.rs` | reducer、TestBackend、焦点、popup、折叠/换行/复制兼容测试 |
| `tests/support/mock_lsp.py` | POSIX 真实 stdio mock server，支持脚本化 capability/response/crash |
| `tests/lsp_process_integration.rs` | Unix process-group / Windows Job Object 与继承 pipe descendant 的真实进程 cleanup；不依赖 PTY |
| `scripts/e2e/fixtures.py` / `scenarios.py` | hermetic PATH mock、production-binary 代码跳转 journey |
| `README.md` / `docs/README.md` | 实现后补快捷键、配置、外部进程 trust 和本文索引 |

现有 `PreviewContent`、`PreviewProvider::preview`、`PreviewRegistry` 和
`App::with_preview_registry` 不删字段、不改签名。`AppOptions` 是 additive constructor。

## 12. 实现顺序

1. 增加依赖与 `navigation.rs` 的位置、UTF 转换、language/config/URI boundary 和 payload permit
   ownership 单测。
2. 泛化 `folding.rs` 为独立 fold/symbol/recognizable-token projections，逐语言完成本文映射，并先
   证明三份预算与 all-or-nothing 结果互不影响；`runtime.rs` 随 Preview 在后台发布完整 index。
3. 实现 borrowed JSON-RPC frame/envelope parser、`RpcId::Signed(i32)` allocator/epoch、24 MiB/session
   与 128 MiB/global accounting、LSP state machine、stdin writer 和可注入 transport；所有 timeout/
   backoff 测试用显式 `now` 驱动，避免真实 sleep。
4. 实现精确 document-sync matrix 与隔离的 1 active + 1 queued disk-snapshot lane，先用注入的
   opener/reader/clock/cancel 验证 500 ms/deadline/generation，再连接 LSP request dispatch。
5. 实现 production `OwnedProcessTree` spawner、non-inheritable parent pipe ends、独立 bounded stdin
   writer、stdout/stderr drain、`IoThreadDone` handshake 与 tree-first forced cleanup。
6. 在 `runtime.rs` 后台构造完整、未截断且已计账的 NavigationDocument；实现 deepest safe repo root。
7. 接入 App generation/provider policy 和 token `partition_point`；先完成 same-file direct reveal，再完成 cross-file
   NavigationStage、完整 restore 与 history 成功后原子提交。
8. 接入 picker、caret、mouse、快捷键和 footer；验证 Search/Find 优先级和 foreground 零 parse/I/O。
9. 加 TestBackend、permit 并发耗尽、mock-LSP integration 和 inherited-pipe descendant 真实进程
   cleanup，再加 production PTY journey。
10. 更新 README/docs index，跑完整门禁和 MSRV。

任何阶段都不能为了先展示 UI 而在 render 路径同步 spawn/read。

## 13. 测试与验收门禁

### 13.1 确定性单元测试

- `LineIndex`：ASCII、中文、emoji surrogate pair、combining mark、CRLF、空行、行尾、非法
  UTF-16 column 和非法 UTF-8 byte boundary。
- framing/JSON-RPC：拆分/多 frame、header casing、缺/重复/非法 Content-Length、Content-Type 缺省
  charset、单个大小写不敏感 `charset=utf-8`/`charset=utf8`、重复/其他/malformed charset、8 KiB/4 MiB
  边界；`RpcId::Signed(i32)` 的 negative/zero/max 与 string server id、fractional/越过 signed `i32`
  server request → `-32600 id:null`、invalid response → session fail、`i32::MAX` 只分配一次、下一次
  `RequestIdExhausted` forced restart/reset 0、old-epoch response rejection、retired-id 第 65 项 forced
  restart、invalid params/JSON/EOF/write/flush failure。stdout cap2、writer cap16/1 MiB、completion
  cap16、result 1 MiB、pending overflow、
  latest-wins coalescing都验证 fail-whole、无 partial item。
- payload accounting：逐项覆盖 `4 + 8 + 4 + 2 + 1 MiB` state components 和剩余 permit-bound
  writer/stderr/completion/NavigationDocument/line/token allocation；验证 ownership transfer/release、
  checked overflow、borrowed visitor 不构造 unrestricted Value，以及多个 session 并发耗尽 24 MiB
  session/128 MiB global permit 时 fail whole、释放后可恢复。
- config/trust：64 KiB/64 KiB+1、strict UTF-8/BOM、所有层 duplicate/unknown field、entry/arg/string
  count/byte cap、平台用户配置、missing/invalid、global/family disable、未配置 family 不查 PATH、
  绝对 program、显式 basename PATH、相对 PATH、symlink/reparse、workspace executable、Unix
  execute bit、Windows `.exe` 以及 `.cmd/.bat/.com/.ps1` 拒绝。
- URI：空格/Unicode、percent encoding、Windows drive/UNC（Windows cfg）、non-file、query、
  outside root、symlink/reparse point 和 missing target。
- structure：本文每个 declaration/name field、匿名排除、Markdown 名称/rank hierarchy、
  full/selection range；symbol overflow/failure 返回空且 fold 输出/预算完全不变；四种 family token
  allowlist、100,000 visited-node / 65,536 range 边界、排序/去重/无重叠、成功零 token complete、
  invalid/overflow/exhaustion 空且 incomplete，并证明 token/fold/symbol 三路失败互不传播。
- LSP symbols：nested preorder/parent/detail、flat SymbolInformation 无 synthetic hierarchy、unknown
  kind→Other、empty/null、mixed variant、selection containment、cross-file、depth/count/string/result byte
  cap，任一非法项都 fail whole。
- policy：每个 ServerState × operation；无配置时三种 semantic operation 均不跳，所有结果
  都无 AST/same-name fallback。
- sync：capability missing、shorthand None/Full/Incremental 和 options 的精确 `open_close`/`change`
  矩阵，所有分支 didChange 永不发送；None/openClose=false 时注入 no-follow disk revalidation 的
  相同/增长/缩短/identity change/换行规范化/stale，1 active + 1 queued、`try_send` queue-full、
  `min(500 ms, request deadline)`、open 前/每个 64 KiB read 前 cancel/generation/deadline，以及本地
  timeout 不发 LSP、late completion 丢弃。
- lifecycle：version/`RequestIdExhausted`、三类 timeout、cancel/stale epoch、blocked stdin 不阻塞其他
  session、stdout/stderr EOF、crash/backoff、LRU、Ready-only shutdown/exit 与 Starting/失败 forced
  cleanup；Unix process group、Windows suspended→Job Object→resume、non-inheritable parent pipe、
  tree-first termination、direct child reap、每个 `IoThreadDone` 后才 join、Backoff/Failed gate、runtime
  drop 并发规则。真实 helper descendant 继承 stdout/stderr 并在 direct child 退出后继续持有 pipe，
  断言整个 tree 被终止、EOF/done/join 完成且无残留进程；禁止 deadline 上无条件 join。
- history：invocation viewport/caret、NavigationStage、每种失败/cancel/stale/safety 原子恢复、
  picker/pending 的 App-owned invocation、Source/Utf16 target 延迟转换、back/forward success-only commit、
  new jump 清 forward、128 上限。

### 13.2 mock-LSP 集成

抽象 `LspTransportFactory` 允许 Rust 测试注入内存 transport，覆盖完整 actor/reducer 而不 spawn。
POSIX 另用 `tests/support/mock_lsp.py` 验证真实 Content-Length stdio：

- initialize → initialized → didOpen → definition；
- Location、Location[]、LocationLink[]；
- references 多结果、implementation capability 缺失；
- nested DocumentSymbol、flat SymbolInformation、negative/string server request id reply、diagnostics ignore；
- UTF-16 emoji target、工作区外 target 过滤；
- delayed stale response、`$/cancelRequest`、malformed/oversize frame、stdin stall、stderr flood、crash/backoff；
- sync capability 四种 shape 与注入 disk lane 的 queue/deadline/cancel/late result；
- concurrent permit exhaustion、ownership transfer/release 和 `RequestIdExhausted` restart 后旧 epoch response；
- shutdown/exit 后 process tree 退出，无 pipe deadlock；另由 `tests/lsp_process_integration.rs` 用真实
  inherited-pipe descendant 验证 direct child 退出也不能让 cleanup 提前完成。

Windows 运行 frame/state-machine/in-memory integration；真实 process/URI/cleanup 用 Windows Rust
integration 覆盖，不依赖 Python 或 PTY。

### 13.3 Ratatui TestBackend

- F12 的 NONE/SHIFT/CONTROL 精确矩阵、Alt 左右、@ 的 NONE/SHIFT、额外 modifier 忽略；
  fold-gutter 优先于 Ctrl/Super click，modified click 优先于 selection。
- 四种 family `RecognizableTokenIndex` allowlist；只用 `partition_point` 验证 start 命中、end-exclusive、
  whitespace/行尾/token-end 不吸附，incomplete/no-hit 都显示状态、不创建 invocation/不发 LSP，并
  断言 key/mouse foreground 不 parse；picker 统一显示 1-based line/UTF-16 column（emoji/CJK/combining
  mark 不使用 byte/display width）。
- picker → Find/Search → pending cancel → quit 的 Esc 顺序，以及 Starting/loading/failure/footer。
- 1-result direct、multi-result picker、键盘/鼠标/double-click、Esc restore focus。
- target 展开 ancestor fold，保留无关 nested fold；wrap 后滚到含 byte 的 visual row。
- caret/target highlight 不改变 line number、selection 或 copied source text。
- same-file 和 staged cross-file back/forward；目标 Preview 失败、cancel、stale、非法或截断时
  完整 tree/content/caret/viewport/history 恢复。
- stale navigation/content completion 在 refresh、文件切换和连续 F12 后被拒绝。
- `App::new`/`with_preview_registry` 不读 HOME/PATH 且 disabled；production options 与 fake
  settings 注入确定性。

### 13.4 production PTY

在现有 hermetic Sandbox 内创建两份 Rust 文件、mock `rust-analyzer.exe`/native executable 和
用户级 `lsp.json`，显式 `enabled=true`；若配置 basename，再把只含绝对 sandbox bin 的目录
prepend 到 scenario PATH。journey 至少覆盖：

1. 打开 caller Preview，鼠标点击/按 F12；
2. 等待 `Starting rust-analyzer…` 与 definition target；
3. Shift+F12 打开多结果 picker，选择另一个文件；
4. Alt+Left/Alt+Right 返回/前进；
5. 无配置时明确显示 `No configured Rust language server.` 且绝不 spawn；非法 workspace 内
   executable、symlink/reparse 或 Windows shell-backed command 被拒绝；TUI 仍可滚动和退出；
6. `ReadOnlyOracle` 证明 Git status、Git metadata 和 host config 未改变；PTY 持续 drain 到 child
   exit，mock server 无残留进程。

### 13.5 交付命令

```sh
cargo +1.88.0 check --locked
make ci
make coverage
```

`make ci` 仍是交付主门禁，`make coverage` 必须保持 80% line floor；navigation、framing、UTF
转换和 policy 的新生产逻辑不能只依赖 PTY 覆盖。若修改 package 内容再额外运行
`make package-smoke`，本方案本身不要求改变发行包布局。

## 14. 明确不做

- 不自动安装或提示一键安装语言服务器。
- 不根据 PATH 自动发现、授权或启动语言服务器；basename 解析只服务于显式 enabled 配置。
- 不扫描 `.vscode`、package scripts、workspace 文件或仓库配置来获得 executable command。
- 不做同名 workspace symbol → definition 的推测。
- 不给 Definition、References 或 Implementations 做任何 AST fallback。
- 不在 Diff 中发 LSP position；Diff 行不是工作树源码坐标。
- 不打开工作区外依赖源码，不跟随 symlink/reparse point。
- 不处理 rename、format、code action、completion、hover、signature help、diagnostics UI。
- 不发送 didChange，不接受 workspace edit，不执行 server Command。
- 不引入 SCIP/LSIF 索引，也不后台扫描整个 workspace 建自定义语义索引。

只有在以上边界、generation/stale proof、UTF-16 转换、mock server、TestBackend、PTY 和完整
门禁同时通过后，代码导航才算完成。
