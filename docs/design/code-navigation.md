# 代码跳转设计

状态：已实现。结构快照、跨平台 LSP 进程边界、协议 actor、App 原子跳转、canonical keymap、
Alt hover/click、preview + grouped-list results popup 与历史导航均已落地；真实语言服务器、
Windows process-tree 和 PTY 端到端行为由第 13 节交付门禁持续确认。

本文定义 Latte Lens 的只读代码导航能力。目标不是在终端里重写一个语言分析器，
而是通过内置默认命令发现并信任语言服务器后复用 LSP 的语义结果；没有可用的语言
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
- `ContentIdentity` 区分规范化的工作区相对路径和受控 package root 下的 dependency 相对路径；
  `ContentSnapshot` 已携带 `identity`、`fold_source` 和 `fold_regions`。导航文档身份在此基础上扩展，
  不能退回 UI label 或只用文件名。
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
  ├─ provider policy + resolved executable trust boundary
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

每个 `(server_root, language_family)` 在一个 Latte Lens 进程内恰好复用一个 LSP session。
session 数量没有固定上限；同一 language family 在不同 nested repo/server root 启动并保留独立
session，不因已经存在若干其他 key 而拒绝或驱逐。每 session 与全 runtime 的 payload budget、
有界 channel 和请求上限仍独立生效。

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

`NavigationInvocation` 从按键起到 results popup、stage 和 history commit 全程由 `App` 持有；runtime
只接收 `NavigationProtocolRequest` 并回传 request key/result，不持有 UI restore、results popup 或
history。completion 必须匹配 invocation generation、源 `ContentIdentity` 和 `DocumentVersion`。
文件切换、刷新、再次发起导航、关闭 popup 和退出都会使旧 generation 失效；
`$/cancelRequest` 不能替代 reducer 的 stale check。

LSP location 使用 `Utf16` variant，本地 Document Symbol 使用 `Source` variant。该 enum 原样
贯穿 result、results popup、pending transition 和 history；只有目标完整 Preview 的 `LineIndex` 可将
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
- Alt+单击先更新 caret，再触发 Definition；Alt+hover 只更新独立 hover highlight。
- Preview Find 命中、Document Symbol 选择和导航 target 会把 caret 移到 range start。
- `j/k` 仍按当前行为滚动一个 visual row；`sync_content_cursor_to_scroll` 同时把 caret 移到
  新顶部逻辑行的第一个非空白 grapheme。它不改变 `←/→` 的 pane focus 语义。
- caret 不是文本编辑光标，不接受字符输入，也不常驻显示下划线。只有 Alt+hover 精确命中的完整 token
  使用独立下划线提示；semantic shortcut 只查询后台产出的 `RecognizableTokenIndex`，index 不完整、
  空白、行尾或没有命中时显示状态且不请求 LSP。
- selection、copy、navigation hover 和 navigation target highlight 是四个独立状态；跳转高亮不能伪装成
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
只接受无 query/fragment 的 `file:` URI。服务端 location 进入 App 前必须先转为平台路径，再走下列两条
互斥路径：

1. **workspace source**：通过 `inspect_content_path(Some(app.root), path)` 的 no-follow 普通文件检查，
   确认位于用户选中的 `App::root`，再转换为工作区相对 `ContentIdentity`。
2. **dependency source**：工作区外时，从文件父目录向上最多检查 32 层；每一层和目标文件都必须通过以
   filesystem root 为边界的 no-follow 普通文件检查，且其中一层必须含自己的普通文件 manifest：
   `go.mod`、`Cargo.toml`、`package.json`、`pyproject.toml` 或 `setup.py`。该 manifest 所在目录是受控
   content root，结果转换为 `ContentIdentity::Dependency { root, relative, server_root }`。`root` 仅用于
   Preview 安全边界，`server_root` 仍是发出请求的 workspace/repository root；不能因为打开依赖而扩大
   LSP session 或 Tree/Git 的工作区范围。

非 file URI、工作区外且不属于上述安全包根的路径、symlink/reparse point、目录、缺失文件和不可表示的
Windows URI 全部丢弃并计数。若响应原本非空但过滤后为空，显示
`Navigation target is outside the opened workspace or unsafe.`，不走 AST fallback。

## 6. 语言、workspace root 与 server 配置

### 6.1 语言映射

| family | 扩展名 | LSP `languageId` | 内置默认命令 |
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
不迁移；下一次导航若计算出不同 key，则打开新 session，旧 key 的 session 保留并在再次命中时
复用，直到进程退出或该 session 进入既有的失败/清理状态。

### 6.3 配置文件与优先级

Latte 产品统一把用户配置放在 `~/.latte`。代码跳转默认开启，Latte Lens 使用内置命令名在
`PATH` 中发现对应语言的 server；它只读取用户级产品配置，不扫描仓库内配置：

- Linux、macOS、Windows 默认：`~/.latte/latte-lens.jsonc`；Windows 的 `~` 来自
  `%USERPROFILE%`，其他平台来自 `$HOME`。
- `LATTELENS_CONFIG`：可覆盖为另一个绝对产品配置路径；相对路径拒绝。

显式路径不存在或 JSONC 非法时，代码跳转整体不可用并显示状态；默认文件不存在不是错误，
而是使用代码中的默认值。`code_navigation` 是功能配置域，language server 是
`engine.type` 所选择的实现。配置按字段覆盖内置默认：缺失字段继承、`enabled=false` 关闭、
显式 `engine.command` 替换对应语言的默认命令。配置 schema 固定为：

```jsonc
{
  "code_navigation": {
    "languages": {
      "rust": {
        "engine": {
          "type": "language_server",
          "command": ["/opt/bin/rust-analyzer"],
        },
      },
      "python": { "enabled": false },
    }
  }
}
```

- 配置读取也使用 no-follow/reparse-safe 的普通文件边界，且独立于 `App::root`；先读取最多
  65,537 bytes，只有在 EOF 且长度不超过 64 KiB 时才解析。内容必须是严格 UTF-8，不接受 BOM。
- parser 接受 JSONC 行注释、块注释与尾逗号，在所有 object 层拒绝 duplicate key，并对
  top-level、feature、family map、language entry 和 engine 全部执行 `deny_unknown_fields`。
  engine `type` 必须为 `language_server`。最多 4 个 language entry。
- `command[0]` 长度必须为 1..=4,096 bytes 且不含 NUL；其余 argv 最多 16 项，每项最多 4,096 bytes、
  累计最多 16 KiB，且均不含 NUL。任何文件大小、UTF-8、schema、duplicate、字符串或数组上限
  失败都使整个配置 disabled，产生最多 240 个清理后字符的 warning，并且绝不进入 PATH 解析。
- feature 的 `enabled=false` 全局关闭；family 的 `enabled=false` 只关闭该 family。缺失 feature、
  family、`enabled` 或 `engine` 均继承内置默认。默认命令分别为 `rust-analyzer`、
  `typescript-language-server --stdio`、`pyright-langserver --stdio` 和 `gopls serve`。
- `command[0]` 只允许绝对路径或无路径分隔符的 basename。拒绝 `./server`、`../server` 和其他
  相对路径，避免 child cwd 指向仓库后执行仓库文件。
- 默认或自定义 basename 由 Latte Lens 遍历 `PATH` 解析。仅遍历绝对 PATH entry，忽略空项、
  `.` 和相对项，并始终解析成绝对 executable。某个默认命令不存在或未通过校验时，只让该
  family 的语义跳转不可用，不影响其他 family；显式自定义命令非法仍使整份配置 fail closed。
- 绝对 program 和 basename 解析结果使用同一 trust validation：先 canonicalize 用户配置或
  package-manager 提供的入口，因此允许入口是指向主机工具的 symlink（Windows 为可 canonicalize
  的 link/reparse entry）；broken link 和 cycle 失败。设置中只保留 canonical target，入口随后被
  retarget 不能改变当前授权。canonical target 的每一级再用 `symlink_metadata` 拒绝残留 symlink，
  Windows 逐级拒绝残留 reparse point；最终必须是 regular file 且不位于 `App::root` 内。Unix 记录 canonical file 的
  `(st_dev, st_ino, mode)`，Windows 通过 no-follow handle 记录 volume serial + file id + regular/
  reparse attributes；这些 identity 只用于检查变化，不声称把执行绑定到已打开 handle。位于工作区
  内的 executable 即使用户写进用户配置也拒绝，避免打开仓库后执行仓库内容。
- Unix/macOS 还要求最终文件任一 execute bit 已设置。Windows basename 只尝试原名和
  `.exe`，绝不采用当前目录；绝对路径只接受大小写不敏感的 `.exe`，显式
  拒绝大小写不敏感的 `.cmd`、`.bat`、`.com`、`.ps1` 以及任何由 shell/interpreter 关联启动
  的文件。Windows reparse point 校验使用现有 `windows-sys` 文件属性。
- `NavigationSettings` 只保存 canonical absolute program、受限 args 和初次 identity，不保存原始
  entry link。**每一次**
  spawn 都在 process API 调用前的同一函数内重新执行逐级 no-follow/reparse、regular、execute/
  native `.exe`、workspace exclusion、canonical path 和 identity 检查；变化即返回
  `Configured language server changed since validation.`，不 spawn、不自动重新授权。Unix 把该
  absolute path 传给 `Command::new`；Windows 把它作为非空 `CreateProcessW.lpApplicationName`。
  两端都逐 argument 编码，绝不通过 shell 拼接。
- 这条边界抵御的是**已打开工作区内的对手**：仓库内容不能提供、替换或经 symlink/reparse
  重定向 server。最终检查与 OS 按路径装载之间仍存在很小的路径替换窗口；`execve`/
  `CreateProcessW` 都没有本方案可统一使用的“从已验证 handle 执行”语义。因此，能够修改用户在
  workspace 外显式授权的 executable 或其父目录的 actor 属于用户已经信任的主机/工具链边界，
  不在 workspace-adversary guarantee 内。实现、README 和 footer 不得声称 executable identity
  被跨 spawn 原子锁定，也不能把这一限制表述成 Windows 不支持。
- 首期不接受配置中的环境变量、workspace-specific command、initializationOptions 或任意
  server command。子进程继承 Latte Lens 环境，cwd 固定为 `server_root`。
- 通过 trust validation 也只代表可启动；首次 semantic navigation 请求才 spawn。

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

`App::new` 与 `App::with_preview_registry` 均使用 `AppOptions::default()`，不得读取 HOME、
USERPROFILE 或 PATH，也永远默认 navigation disabled。production `main` 先 canonicalize CLI
workspace，显式调用 `NavigationSettings::load_user_config(&workspace_root)` 合并内置默认与用户
配置、解析 basename 并执行上述 trust validation，再调用 public additive `App::with_options`。
`App::with_options` 只接收已解析成绝对路径的 trusted settings，并在 canonical App root 已知后
再次执行不含 PATH 的防御性校验。默认配置缺失时返回可发现的默认 settings；配置非法返回
disabled settings 加 warning，不能阻止 TUI 启动。测试和 embedder 显式注入
`NavigationSettings::disabled()` 或 fake settings，不接触
宿主配置。现有构造函数和 `PreviewProvider` 签名不变。

## 7. Provider 优先级

### 7.1 Definition

顺序必须固定：

1. 若对应 family 没有可用的 trusted engine，显示
   `Code navigation is unavailable for Rust: no language server was found.` 并保持原位。
2. 已配置 session 在 Backoff/Failed 或 initialize 未声明 Definition capability 时显示精确状态，
   不请求、不降级。
3. 已配置 session 为 Starting 时只保留最新 generation，Ready 后请求
   `textDocument/definition`。
4. LSP 返回合法 null/空结果时显示 `No definition found.`；合法一个结果 direct，多个结果进入
   results popup。
5. 工作区外/不安全结果、协议错误、超时、崩溃或 malformed response 都显示对应状态并保持
   原位。

Tree-sitter、pulldown-cmark、Search、Document Symbols 和同名 workspace symbol 在任何状态下
都不是 Definition provider。没有 `LocalDefinitionProof`、same-file name lookup 或伪语义索引。

### 7.2 References 与 Implementations

只使用已解析且 Ready 的 LSP。server 不可用、未声明 capability 或请求失败时保持原位并
显示状态；不使用 AST 和 workspace 搜索降级。合法空结果显示状态；References 或 Implementations
只要有一个或多个结果就进入 results popup，不自动选择唯一结果。

### 7.3 Document Symbols

Document Symbols 属于本地结构导航，不是 semantic definition：

1. `StructureSnapshot.symbols_complete=true` 时立即使用 Tree-sitter / pulldown-cmark symbols；
2. 本地语言不支持或结构预算不完整，且对应 family 的 LSP Ready 并支持
   `documentSymbol` 时请求 LSP；
3. 两者都不可用时显示状态。

Tree-sitter symbols 优先，保证 `Ctrl+S` 不因 LSP 启动变慢；LSP `documentSymbol` 是结构解析不可用
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

所有 payload allocation 同时取得 checked 的每 session **32 MiB** permit 与全 runtime
**192 MiB** global permit；checked add/multiply、capacity 增长或任一 permit 获取失败都 fail whole，
绝不交付 prefix 或 `truncated=true`。记账覆盖 stdout reader 正在构造的 frame、stdout channel 的
2 个 queued frame、manager 当前 frame、JSON scratch **reservation**、normalized result/App
completion、writer frame、16 KiB stderr tail、disk revalidation buffer，以及
`NavigationDocument` 的 text/`LineIndex`/`RecognizableTokenIndex` allocation。`Arc` 共享内容只记
一次，但 permit owner 必须活到最后一个 owner drop。

单 session 的可审计最坏并发 payload 公式固定为
`R4 + Q8 + M4 + S4 + O1 + W1 + D3 + V0.501 + E0.016 = 25.517 MiB`：`R/Q/M` 分别是
reader 正在读的一个 body、channel 中两个 body、manager 正在解析的一个 body；`S` 是与当前 body
长度相等且最大 4 MiB 的 serde scratch reservation；`O` 是至多 1 MiB 的 normalized result（移入
completion 后仍是同一 allocation）；`W` 是 writer 队列合计 1 MiB；`D` 是每 session 同时保留的
navigation text（512 KiB）、至多 2,001 个 line offset、65,536 个 `SourceRange` token capacity 与
metadata 的合计 3 MiB hard sub-cap；`V` 是 `PREVIEW_MAX_BYTES + 1` 的 revalidation buffer；`E`
是 stderr tail。余下至少 6.483 MiB 仍受同一 permits 约束，用于 frame headers、受限 Vec/String
capacity 与固定 channel envelope；它不是可以绕过记账的自由区。session 数量没有固定上限；
192 MiB global permit 仍限制所有 session 同时准入的 payload 总量，因此任意数量的 session 都不可能
同时达到 32 MiB 峰值，增长失败时只返回小型错误而不交付部分结果。
location 最多 2,000，document symbol 最多 4,096，symbol 的 name/detail/container 内容累计最多
512 KiB，单个 normalized result 最多 1 MiB。

permit token 随 buffer/result 的所有权在 reader → queue → manager → parser/result → completion/App
之间 transfer，旧 owner 在 transfer 后不得保留重复 charge 或未记账 clone；drop/cancel/stale/failure
在该 owner 释放 allocation 时同步 release permit。进入 `serde_json` 前先执行不分配输出的
`JsonPreflight`：字符串/escape aware 地扫描单个 top-level value，拒绝 nesting 超过 128 和任一
JSON number token 超过 128 encoded bytes；它只做资源前检，不代替 JSON grammar validation。

在调用 `serde_json::Deserializer::from_slice(manager_body)` **之前**，parser 必须取得恰好
`manager_body.len()` bytes 的 `ScratchPermit`，并一直持有到 `Deserialize` 与 `Deserializer::end()`
都返回。`serde_json 1.0.150` 自己拥有 `Deserializer.scratch: Vec<u8>`，escaped string 和某些数字
路径会增长它；custom visitor 无法注入、限制或观察该 Vec，所以本文只把 body-length reservation
作为其输入有界时的最坏 scratch charge，绝不声称 visitor 控制 serde 内部分配。allocator 的 capacity
rounding 也不是精确 RSS hard limit；32/192 MiB 是 Latte Lens 的逻辑 payload admission limit。
README、status/footer 和错误消息只能称其为 `navigation payload budget`；不得写成“进程/会话物理
内存被硬限制为 32/192 MiB”、RSS ceiling 或 allocator hard cap。
scratch permit 失败时禁止调用 serde。envelope parser 借用 manager frame，并用 bounded visitors
直接构造受限类型；visitor 在复制 decoded string、Vec 或 map **之前**取得 output permit，禁止先建
unrestricted `serde_json::Value` 或无界容器。接近 4 MiB 的 escaped string 要么在目标字段 cap 内
完整成功，要么 fail whole；接近 4 MiB 的 number 在 preflight 阶段因 128-byte token cap 拒绝。

session 按规范化 key 唯一复用且数量无固定上限；App→manager command `sync_channel(32)`；每 session
pending JSON-RPC request 64；
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
- disk lane 只有一个长期 thread，**绝不**因为 active read 超时而再 spawn replacement。deadline 只让
  request generation 失败并禁止后续 LSP request；若底层普通文件位于异常网络/虚拟文件系统且一次
  OS `read` 永不返回，该 lane 标记 `WedgedRead`，丢弃唯一 queued job，后续需要 revalidation 的
  navigation 立即返回 `File revalidation worker is unavailable.`。这把最坏情况限制为一个 blocked
  thread 和一个已计账的 512 KiB+1 buffer，而不是随请求累积线程。
- `NavigationRuntime::drop` 设置 lane cancel、drop queued sender，并最多等待 100 ms 的
  `DiskLaneDone`。如果注入 reader 或 OS read 仍未返回，Rust/std 没有跨平台安全取消任意 blocking
  file read 的能力；drop 会丢弃 `JoinHandle`（detach），不会做无界 join。该 thread 只能持有自己的
  file handle、已计账 buffer、`Arc<AtomicBool>` cancel 和一个接收端已被 drop 的 one-shot sender；
  不能持有 `App`、session、process/job、runtime command/completion sender 或 workspace mutable state。
  read 最终返回时它先看到 cancel，释放 file/buffer/permits，发送失败后退出。测试用永不返回的
  injected reader 证明 drop 有界，并明确断言这是 teardown degradation，而不是“所有 thread 已 join”
  的虚假保证。
- didOpen version 使用 checked `i32`。到达 `i32::MAX` 后先 didClose 并 orderly restart
  session，新 session 从 1 开始；绝不 wrap 或复用旧 session version。
- 不把 Preview 截断内容发送给 server。

### 8.4 请求与响应

- Definition：`textDocument/definition` 的 `result` 接受 `null`、`Location`、
  `Location[]` 或 `LocationLink[]`；`LocationLink` 优先 `targetSelectionRange`。
- References：`textDocument/references` 使用 `includeDeclaration=true`，`result` 只接受
  `null` 或 `Location[]`，非空结果始终进 results popup。
- Implementations：`textDocument/implementation` 接受与 Definition 完全相同的 nullable shape，
  非空结果始终进 results popup。
- Document Symbols：`textDocument/documentSymbol` 接受 `null`、nested `DocumentSymbol[]` 或
  flat `SymbolInformation[]`。`null`/空数组都归一化为空 symbol；非空数组必须先判定且全程保持
  单一 variant，不能逐项猜测。
- `DocumentSymbol[]` 隐含属于当前请求 document：按 depth-first preorder 展平，真实 nested parent
  写入 `parent`；`name.trim()` 必须非空，`detail` 只作 result display metadata。`range` 与
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
spawner 返回 `OwnedProcessTree`，而不是裸 `Child`，并在进入平台分支前执行 6.3 的最终
pre-spawn revalidation。

#### 8.6.1 Unix/macOS process group

- 用绝对 program、逐项 args、`current_dir(server_root)`、piped stdio 和
  `std::os::unix::process::CommandExt::process_group(0)` spawn；spawn 后要求
  `getpgid(pid) == pid`，否则终止/reap 尚未交付的 direct child。parent pipe ends 保持 `CLOEXEC`，
  不使用 shell 或 user `pre_exec` closure。
- forced cleanup 在 direct child 尚未 reap 时先 `kill(-pgid, SIGTERM)`，250 ms 后 group 仍存在则
  `kill(-pgid, SIGKILL)`，再 wait/reap direct child；最后 `kill(-pgid, 0) == ESRCH` 才确认 group
  已空。descendant 继承 group，所以 direct child 先退出且 descendant 持有 pipe 时仍能 tree-first
  cleanup。显式信任的 server 若主动 `setsid`/`setpgid` 逃逸，属于 trusted-server 边界外，文档
  不得声称能约束恶意 server。

#### 8.6.2 Windows x86_64 direct `CreateProcessW`

现有发布范围的 Windows x86_64 必须完整实现，不提供“Windows unavailable”分支；baseline 为
Windows 10 / Windows Server 2016 及以上。使用 `windows-sys = 0.61.2` 的
`Win32_Foundation`、`Win32_Storage_FileSystem`、`Win32_Security`、`Win32_System_Environment`、
`Win32_System_IO`、`Win32_System_JobObjects`、`Win32_System_Memory`、`Win32_System_Pipes`、
`Win32_System_Threading`。

spawn 顺序固定如下，任一步失败都逆序 RAII cleanup，不交付半初始化 handle：

1. 用 inheritable `SECURITY_ATTRIBUTES` 三次 `CreatePipe`；立即对 parent stdin-write、stdout-read、
   stderr-read 调用 `SetHandleInformation(HANDLE_FLAG_INHERIT, 0)`，child stdin-read、stdout-write、
   stderr-write 保持 inheritable。
2. 第一次 `InitializeProcThreadAttributeList(null, 2, 0, &size)` 取得大小，用 `GetProcessHeap` +
   `HeapAlloc(HEAP_ZERO_MEMORY)` 分配后以相同的 attribute count `2` 再次 initialize；列表的两项分别是
   `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 和 `PROC_THREAD_ATTRIBUTE_JOB_LIST`。前者写入恰好三个 child
   handles，后者写入第 6 步创建的 Job；handle array 和 Job handle storage 都活到 create
   返回，所有分支 `DeleteProcThreadAttributeList` 后 `HeapFree`。
3. `STARTUPINFOEXW.StartupInfo.cb = size_of::<STARTUPINFOEXW>()`、`dwFlags =
   STARTF_USESTDHANDLES`，stdio 指向 child ends；`PROCESS_INFORMATION` 清零。
4. `lpApplicationName` 是最终校验的绝对 `.exe`；mutable command line 是 NUL-terminated UTF-16。
   argv0/每个 arg 总是加双引号：quote 前 `n` 个 backslash 写成 `2n+1` 个再写 quote，结束 quote 前
   `n` 个写成 `2n` 个，其余原样；参数间一个空格。NUL 或含终止 NUL 超过 32,767 UTF-16 units
   拒绝。这固定为 CRT/`CommandLineToArgvW` argv contract，不假装适配自解析 raw command line 的
   非标准 server。
5. `GetEnvironmentStringsW` 的 block 原样传 `lpEnvironment`，create 返回后必定
   `FreeEnvironmentStringsW`；flags 为 `CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED |
   CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT`，cwd 为绝对 `server_root`，security attrs 为 null，
   `bInheritHandles=TRUE`；handle list 隔离并发 spawn 的其他 inheritable handles。
6. create 前 `CreateJobObjectW(null, null)`，以
   `JOBOBJECT_EXTENDED_LIMIT_INFORMATION.BasicLimitInformation.LimitFlags =
   JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 调用
   `SetInformationJobObject(JobObjectExtendedLimitInformation)`。扩展 attribute list 的数量为 2：除
   `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 外，同时通过 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 传入这个 Job，
   使 child 在 `CreateProcessW` 返回前已经属于 Job，不存在 suspended child 尚未被 owner containment
   接管的窗口。`CreateProcessW` 成功后立即构造 process/job owner，再关闭 parent 的 child-end copies
   并销毁 attribute list；child 仍 suspended。
7. owner 构造完成后才 `ResumeThread`，再关闭 primary thread handle，process/job 与 parent pipe ends
   转入 owners。`ResumeThread == u32::MAX` 或其他 post-create failure 必须先保存原始错误；只有
   `TerminateJobObject` 成功、`WaitForSingleObject(process, ...) == WAIT_OBJECT_0` 且
   `ActiveProcesses == 0` 三项同时成立，才可按已清理失败返回。任一项失败都返回原始错误、secondary
   cleanup error 和仍持有 process/job handles 的结构化 owner，由已知 `SessionKey` 的 manager
   quarantine 保留；不得静默 drop 或用 cleanup error 覆盖原始错误。

若 Latte Lens 自身在 supervisor Job 中，不请求 breakaway；Windows 8+ nested job 允许 creation-time
加入 Latte Lens 的空 child job。outer job policy 拒绝 `CreateProcessW` 的 Job list 时不会创建 child，
在 resume 前保留原始创建错误安全失败，不是关闭 Windows 实现。绝不设置
`BREAKAWAY_OK`/`SILENT_BREAKAWAY_OK`，所以正常 descendant 留在 job，且按 server 的 spawn 规则
继承其 stdout/stderr child handle。

forced cleanup 先 `TerminateJobObject`，再用有界 `WaitForSingleObject(process, ...)` reap direct
child，并用 `QueryInformationJobObject(JobObjectBasicAccountingInformation).ActiveProcesses == 0`
确认 tree 已空；超时保持 cleanup/fatal，不能伪装成 Backoff。最后关闭 process/job handle；
`KILL_ON_JOB_CLOSE` 是第二道保证。`GetExitCodeProcess` 只在 process signaled 后读取。

#### 8.6.3 pipe threads

- 每个 stdin writer、stdout reader、stderr drainer 在退出前先关闭自己唯一持有的 pipe handle，
  然后通过独立有界 lifecycle channel 发一次 `IoThreadDone(kind, session_epoch)`。manager 只有收到
  对应 epoch/kind 的 `IoThreadDone` 后才 join 那个 handle；deadline 本身绝不能触发无条件 join。
  lifecycle channel 为三个 terminal event 保留容量，不与可丢/可满的 writer ack channel 共用。
- stdout EOF 在正常 `StoppingShutdown` 之前发生时视为 crash：fail pending、关闭 bounded writer
  sender并设置 cancel，然后进入 forced cleanup；stderr EOF 只记录对应 `IoThreadDone`，不单独推进
  session state。header/body 中途 EOF、stdin write/flush failure 和 malformed frame 走同一个
  forced-cleanup transition。
- 只有 `Ready → StoppingShutdown` 可走 orderly cleanup：manager 先向所有 active Ready session
  非阻塞发送 `shutdown`，再用同一个 absolute 750 ms deadline 轮询对应 response；收到者发送 `exit`。
  phase 结束后统一关闭 writer sender，再用共享 750 ms clean tree-exit deadline 轮询所有 owner；
  不能为每个 session 单独重启 deadline，也不能在 deadline 上直接 join pipe thread。
- `Starting`、initialize failure、Backoff/Failed 转换、crash、malformed frame、`RequestIdExhausted`、
  writer queue/permit/write/flush failure 都走 `StoppingForced`，不得发送 shutdown/exit：先关闭
  bounded writer sender并设置 cancel，随后由 `OwnedProcessTree` 终止**整个 process tree**，确认
  tree termination 并 reap direct child，之后才等待 pipe EOF/`IoThreadDone`。收到各 done event 后
  分别 join 已结束 thread；不得先等 EOF、不得只 kill direct child，也不得在共同 deadline 后盲 join。
- 同一个 manager tick 内发现的 initialize timeout、协议错误和 reader/writer/stderr/lifecycle failure
  必须先从 active session map 一次性退休：每个 session 的 pending、deferred 与 disk-revalidation waiting
  request 各完成一次，并把 process owner 与原始错误放入 keyed failure batch。tick 结束后整批只调用一次
  phased cleanup；cleanup 成功才按同一完成时刻推进各 key 的 backoff，未证明完成的 owner 只进入 permanent
  quarantine，不能同时记作成功 cleanup/backoff。退休到 batch 收口之间同 key 请求只返回 restarting failure，
  不得在旧 owner 尚存时启动 replacement。若 shutdown 与 failure 同 tick 到达，failed 与仍 active owners
  合入同一次 shutdown cleanup batch；不得先逐 session cleanup 再执行 drop cleanup。
- 只有 process tree 已终止、direct child 已 reap、三个 pipe owner 都已报告 `IoThreadDone` 且对应
  handle 均已 join，才能释放 `OwnedProcessTree` 并进入 `Backoff` 或 `Failed`；这两个状态绝不携带
  live descendant、child、pipe 或 I/O thread。tree termination/reap 失败时保持 cleanup state 并报告
  fatal lifecycle error，不能把未完成清理伪装成 Backoff。
- `NavigationRuntime::drop` 由单个 manager 对任意数量 session 执行 phased batch，不建立 per-session
  cleanup thread：共享 750 ms shutdown-response、750 ms clean tree-exit；随后 broadcast force cleanup，
  Unix 共享 250 ms TERM 后升级 KILL，再共享 1 s kill/reap（Windows terminate Job 后在相同共享 phases
  非阻塞轮询）；最后共享 750 ms I/O completion/join 和 250 ms finished-handle settle。总 wait window
  约 3.75 s 加 O(N) 非阻塞 passes/poll jitter，不随 session 数量乘法增长。各平台只 join 已收到 matching
  `IoThreadDone` 且 `is_finished` 的 thread，不在 deadline 上盲 join。标准、未逃逸的 process group/job descendant
  必须在真实 integration test 中达到全部 EOF/done/join；恶意 trusted server 主动逃逸 containment
  时只能报告 fatal teardown degradation，不能给出“任意 server 都不会留下 thread”的硬保证。

## 9. 交互设计

### 9.1 快捷键

Latte Lens 的 canonical 快捷键按作用域分组，而不是混用不同 IDE 的整套 keymap：

| 作用域 | 规则 | 现有示例 |
| --- | --- | --- |
| 全局命令 | `Ctrl` + mnemonic letter | `Ctrl+P` 文件搜索、`Ctrl+F` 当前内容查找、`Ctrl+Shift+F` 工作区搜索 |
| pane / tree / viewport | 无 modifier 的方向键或 TUI 单键 | `↑/↓/←/→`、`j/k`、`h/l`、`Tab`、`Enter` |
| 当前视图操作 | 无 modifier 的小写单键 | `p` Preview、`d` Diff、`r` Refresh、`q` Quit |
| 代码语义命令 | `Ctrl` + mnemonic letter | `Ctrl+D` Definition、`Ctrl+R` References、`Ctrl+O` Implementations、`Ctrl+S` Document Symbols |
| 历史方向 | `Alt` + 方向键 | `Alt+Left` Back、`Alt+Right` Forward |
| 鼠标语义提示 | `Alt` + mouse | `Alt+Moved` 显示 token 下划线、`Alt+左键` Definition |

代码语义命令只在 Content focus、`ContentMode::Preview`、无 Search/Find/navigation results
popup 时生效：

| 按键 | 行为 |
| --- | --- |
| `Char('d' | 'D')` + modifiers 恰为 `CONTROL` | 请求 Definition |
| `Char('r' | 'R')` + modifiers 恰为 `CONTROL` | 查询 References；有结果时始终打开 results popup |
| `Char('o' | 'O')` + modifiers 恰为 `CONTROL` | 查询 Implementations；有结果时始终打开 results popup，`O` 表示 Open implementations |
| `Char('s' | 'S')` + modifiers 恰为 `CONTROL` | 打开 Document Symbols results popup |
| `Left` / `Right` + modifiers 恰为 `ALT` | Back / Forward |
| `Moved` + modifiers 恰为 `ALT` | 只在精确命中可导航 token 时给整个 token 加下划线 |
| 左键 + modifiers 恰为 `ALT` | 在精确鼠标 point 请求 Definition |
| 任何额外 `ALT/SUPER/CONTROL/SHIFT` 组合 | 忽略，不做近似匹配；普通 hover/click 保持现有行为 |

不使用大写 `D/R/I` 作为 canonical 键，因为它们实际要求 `Shift`；不引入 `g d` 或
`Ctrl+K` 两段 chord；不依赖 macOS 顶排 F 键；也不依赖多数终端无法稳定上报的 Command/Super。
`Ctrl+I` 在传统终端与 `Tab` 编码相同，因此 Implementations 使用可稳定区分的 `Ctrl+O`。
`Ctrl+D` 不再兼任 content page-down；翻页保留 `PageDown`、`PageUp` 和鼠标滚轮。代码实现、README
controls 与 footer 必须只把本表作为主入口，不再宣传 IDE 兼容 alias。功能尚未发布，不为旧 alias
保留兼容承诺。

如果焦点不在 Preview，任一 semantic shortcut 都不切换 pane，只显示
`Focus Preview to navigate.`。Diff 中完全不发请求。Navigation loading 时 UI 继续接收滚动、焦点和
刷新输入；再次请求会取消前一个。`Esc` 只取消当前 semantic request 或关闭 results popup，不直接
触发应用退出。

三种 semantic operation 只有 caret/mouse byte 命中后台 `StructureSnapshot` 已收录的“最小 named
leaf、end-exclusive range”才创建 `NavigationInvocation`；不从 whitespace、行尾或 token end 向
相邻 token 吸附。首期 allowlist 固定为：

| family | Tree-sitter leaf kind |
| --- | --- |
| Rust | `identifier`, `type_identifier`, `field_identifier` |
| TypeScript / JavaScript | `identifier`, `property_identifier`, `private_property_identifier`, `type_identifier`, `shorthand_property_identifier`, `shorthand_property_identifier_pattern` |
| Python | `identifier` |
| Go | `identifier`, `field_identifier`, `type_identifier`, `package_identifier` |

所有 semantic shortcut 与 Alt hover/click 都只能在已排序、去重、互不重叠的 ranges 上使用
`partition_point`/二分查找：
取最后一个满足 `range.start <= caret` 的候选后，只以 `range.start <= caret < range.end` 判定命中；
禁止在 foreground 重新 Tree-sitter parse、线性扫描 AST/ranges 或向相邻 token 吸附。index
`complete=false` 时显示 `Navigation token index is incomplete; refresh Preview.`；完整 index 无命中
（包括成功的零 token、Markdown、whitespace、行尾和 token end）时显示
`No navigable token at caret.`。两种情况都不创建 invocation、不启动 session、不发送 LSP request。
该 allowlist 只决定用户是否点在一个源码 token 上，不产生 definition edge、同名查找或任何 AST
semantic fallback。`Ctrl+S` Document Symbols 不要求 caret token。Alt hover/click 使用鼠标命中的
精确 source byte 和同一 end-exclusive 规则。

Esc 在 `handle_key` 顶层按固定顺序只消费一个状态：navigation results popup → Preview Find/Search
（沿用各自 close/restore）→ 当前 generation 的 pending navigation cancel/restore → 现有双击
退出确认。其他按键仍由 Find/Search 优先消费。

`EnableMouseCapture` 必须保留 any-event tracking。`MouseEventKind::Moved` 只有在 modifiers 恰为
`ALT`、鼠标位于真实（非 synthetic）Preview text row 且命中完整 token index 时才设置独立的
`navigation_hover_highlight`；移动到 whitespace、行尾、synthetic row、popup、其他 pane，或下一次
不带 Alt 的 mouse/key event 都清除它。hover highlight 覆盖整个 end-exclusive token range，只添加
下划线，不改变 caret、selection、viewport、history，也不发送 LSP 请求。

鼠标 Down 先处理 popup/Find，再确认位于 `content_inner`。Preview fold gutter hit-test 必须先于
modified-click；即使带 Alt，点击 `▾/▸` 也只折叠。非 gutter 且位于真实 Preview text row 时，恰好
Alt 的左键更新 caret、清除零长度 selection 并发起 Definition，不进入 `begin_content_selection`；
其余左键最后才走现有 selection/drag 路径。

### 9.2 zero、one、many 与 results popup

- Definition：0 个显示状态，1 个直接跳，多个打开 results popup。
- References：0 个显示状态；1 个或多个都打开 results popup，避免查询引用后意外离开当前位置。
- Implementations：0 个显示状态；1 个或多个都打开 results popup。`Ctrl+O` 的语义是查询
  implementations，不是“打开唯一实现”。
- Document Symbols：有结果时始终打开 results popup，保持父子缩进；没有 symbol 显示状态。

results popup 使用 Search popup 的 dim underlay 和 mouse hitbox 基础设施，但状态独立。标题固定显示
`Definitions (N)`、`References (N)`、`Implementations (N)` 或 `Document Symbols (N)`。语义结果按安全的
display path 分组：workspace 文件显示 workspace-relative path；dependency 文件显示
`dependency/<package-root>/<relative-path>`，因此同名包不会相互合并。文件 group row 显示文件名、相对目录
和该文件的结果数，展开后的 location row 显示 1-based line/column 与单行摘要。排序先按 display path，
再按协议 range，重复 target 去重后才计算总数：

```rust
struct NavigationResultsState {
    title: String,
    invocation: NavigationInvocation,
    groups: Vec<NavigationResultGroup>,
    visible_rows: Vec<NavigationResultRow>,
    list_state: ListState,
    preview_generation: u64,
    preview: Option<NavigationResultPreview>,
    return_focus: FocusPane,
}
```

每个 location 固定保留 `path:(start.line + 1):(start UTF-16 character + 1)`，line/column 都是 1-based。
`NavigationTargetRange::Utf16` 直接使用协议 character；`Source` 必须用该 target document 已加载的
`LineIndex` 从 source byte 转回 UTF-16 后再构造 result row，绝不显示 UTF-8 byte offset 或终端
display width。LSP 能给 container/name 时显示摘要；任何代码预览都必须通过既有安全、bounded 的后台
Preview 路径加载，UI/foreground 不同步读盘。

当 terminal content area 宽度足以同时给预览至少 48 列、结果列表至少 32 列时，popup 使用左右分栏：
左侧显示当前 location 附近的只读代码预览和 target highlight，右侧显示按文件分组的结果列表。更窄时
退化为单列结果列表，不挤压出不可读的预览。`↑/↓/PageUp/PageDown` 只更新 selection；宽屏下以独立、
可取消的 generation 异步更新左侧预览，不替换主 Content、不改变 tree、caret、viewport 或 history。
`Enter` 或单击具体 location 都接受该 target 并执行原子导航；`Esc` 关闭 popup 后完整保持查询前位置。
group row 只展开/收起，不是可接受的导航 target。

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

1. results popup 和 pending 都持有完整的 App-owned invocation，并保存“成功后才采用”的 proposed
   back/forward；此时不
   pop/push stack。result item、pending target 与 history entry 都保留
   `NavigationTargetRange`；成功 commit 后新 history target 存成 `Source` variant。
2. 同文件：调用 `reveal_folded_line`、`scroll_to_logical_line(line, byte)`，设置 caret 和独立
   navigation highlight；同一 reducer turn 校验 generation 后才替换 stacks，否则从 restore
   原子还原。
3. 跨文件不得调用会立即 `reset_content` 的普通 `request_content`。给 `ContentRequest` 增加
   `ContentPurpose::NavigationPreview` 与 `NavigationStage`：前者使用独立 latest-wins generation
   只更新 popup 左侧预览，后者把 completion 放在 pending transaction；两者都不在成功接受前改变
   当前 tree/content/fold。
4. staged snapshot 成功后，先校验 navigation/content generation、URI safety、未截断
   NavigationDocument 和 UTF-16 range；全部通过才在一个 reducer turn 内提交 target content、fold reveal、
   caret/highlight、viewport 与 proposed history。workspace target 还会执行
   `apply_tree_scope(AllFiles)`、tree identity 和 ancestor reveal；dependency target 显示
   `Dependency Source` 标题与 `Dependency · <package>/<relative>` 标签，但严格保留原 Tree/Git scope、
   selection 与 expansion，且不发 directory request。
5. Preview error、cancel、timeout、非法/越界 range、安全过滤或仍由当前 transaction 持有的
   stale completion，都清除 results popup/pending/status 并从 `NavigationRestore` 原子恢复全部状态，
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

1. active Find/Search/navigation results popup 的专用 help；
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
- `Code navigation is unavailable for Rust: no language server was found.`
- `rust-analyzer does not provide implementations.`
- `No definition found.`
- `Navigation target is outside the opened workspace or unsafe.`
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

现有 Windows target dependency 固定并扩 feature 为：

```toml
[target.'cfg(windows)'.dependencies]
windows-sys = { version = "=0.61.2", features = [
  "Win32_Foundation", "Win32_Storage_FileSystem", "Win32_Security",
  "Win32_System_Environment", "Win32_System_IO", "Win32_System_JobObjects",
  "Win32_System_Memory", "Win32_System_Pipes", "Win32_System_Threading",
] }
```

`lsp-types` 只提供协议 data type，不提供进程生命周期；`url` 专门承担跨平台 file URI。
上述精确组合在独立 probe crate 中已分别通过原生
`cargo +1.88.0 check --locked` 和
`cargo +1.88.0 check --locked --target x86_64-pc-windows-gnu`；Windows probe 实际 import 了本文
列出的 `CreateProcessW`、pipes、attribute-list、Job Object、environment、wait/terminate API 与常量，
不是只解析 manifest。它保持 `rust-version = 1.88` 与 Rust 2024 edition。实现后 `Cargo.lock` 必须
锁定 transitive 版本，并在实际仓库再次执行原生与 Windows 1.88 checks；若 resolver 选出高于
1.88 的 transitive crate，必须降级，不能提高 MSRV。

## 11. 文件与接口改动

按以下边界实现，避免把协议代码塞入 `App`：

| 文件 | 改动 |
| --- | --- |
| `Cargo.toml` / `Cargo.lock` | 加入上节四个精确依赖，固定 `windows-sys 0.61.2` 与九个 Windows features |
| `src/navigation.rs` | invocation/Source-or-Utf16 target、LineIndex、语言映射、严格有界 trusted 配置、provider policy、history、result item、NavigationDocument payload permit ownership |
| `src/lsp.rs` | 4 MiB framing、JSON preflight + body-sized scratch permit、`RpcId`、bounded visitors、32 MiB/session + 192 MiB/global permits、sync/disk lane、writer、session actor、URI boundary |
| `src/lsp_process.rs` | `OwnedProcessTree` trait/common state、IoThreadDone protocol；cfg 分派 Unix/Windows backend |
| `src/lsp_process_unix.rs` | `CommandExt::process_group(0)`、group-first TERM/KILL、direct-child reap |
| `src/lsp_process_windows.rs` | direct `CreateProcessW`、STARTUPINFOEX handle + creation-time Job list、pipes/Unicode env/quoting、suspended owner/resume、retained-owner failure cleanup |
| `src/folding.rs` | 增加互相独立的 symbol 与 `RecognizableTokenIndex` pass；保留现有 fold/anchor budget 与三路 all-or-nothing 隔离 |
| `src/runtime.rs` | `ContentSnapshot` 增加后台产生的 crate-private navigation document/structure/token index；增加非破坏 `NavigationStage` purpose；计算受限 server root |
| `src/app.rs` | App-owned NavigationInvocation、generation、token `partition_point`、caret、results popup、status、完整 restore/staged history、精确 key/mouse/Esc reducer；不 foreground parse |
| `src/ui.rs` | results popup、hover/target overlay、footer 状态和 hitbox；无 I/O |
| `src/main.rs` | 加载用户 LSP 配置并传 `AppOptions` |
| `src/lib.rs` | 注册模块；只公开构造所需 options/settings，其他保持 crate-private |
| `tests/app_tui_integration.rs` | reducer/TestBackend；feature-gated Rust helper 经 production spawner 的 definition + atomic target + orderly shutdown journey |
| `tests/support/lsp_test_helper.rs` | 独立的 feature-gated Rust stdio helper binary；提供 framed LSP、PTY LSP 和 inherited-pipe descendant roles，不进入发行包 |
| `tests/lsp_process_integration.rs` | Unix process-group / Windows Job Object；Rust helper descendant 持 pipe 的 production forced cleanup；不依赖 PTY |
| `scripts/e2e/fixtures.py` / `scenarios.py` | hermetic 配置、production binary + Rust helper 的代码跳转 journey |
| `README.md` / `docs/README.md` | 实现后补快捷键、配置、外部进程 trust 和本文索引 |
| `.github/workflows/ci-pr.yml` | Windows stable 运行真实 lifecycle test；新增 Windows 1.88 all-target check 并纳入 gate |

现有 `PreviewContent`、`PreviewProvider::preview`、`PreviewRegistry` 和
`App::with_preview_registry` 不删字段、不改签名。`AppOptions` 是 additive constructor。

## 12. 实现顺序

1. 增加依赖与 `navigation.rs` 的位置、UTF 转换、language/config/URI boundary 和 payload permit
   ownership 单测。
2. 泛化 `folding.rs` 为独立 fold/symbol/recognizable-token projections，逐语言完成本文映射，并先
   证明三份预算与 all-or-nothing 结果互不影响；`runtime.rs` 随 Preview 在后台发布完整 index。
3. 实现 JSON preflight、body-sized serde scratch reservation、borrowed frame/envelope parser、
   `RpcId::Signed(i32)` allocator/epoch、32 MiB/session 与 192 MiB/global accounting、LSP state machine、
   stdin writer 和可注入 transport；所有 timeout/
   backoff 测试用显式 `now` 驱动，避免真实 sleep。
4. 实现精确 document-sync matrix 与隔离的 1 active + 1 queued disk-snapshot lane，先用注入的
   opener/reader/clock/cancel 验证 500 ms/deadline/generation，再连接 LSP request dispatch。
5. 实现 production `OwnedProcessTree`：Unix process group；Windows direct `CreateProcessW` 的 handle +
   creation-time Job list/Unicode env/quoting/suspended owner→resume；两端都完成 parent pipe ownership、bounded writer、
   stdout/stderr drain、`IoThreadDone` 与 tree-first forced cleanup。
6. 在 `runtime.rs` 后台构造完整、未截断且已计账的 NavigationDocument；实现 deepest safe repo root。
7. 接入 App generation/provider policy 和 token `partition_point`；先完成 same-file direct reveal，再完成 cross-file
   NavigationStage、完整 restore 与 history 成功后原子提交。
8. 接入 results popup、caret、mouse、快捷键和 footer；验证 Search/Find 优先级和 foreground 零 parse/I/O。
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
- payload accounting：逐项覆盖 `R4+Q8+M4+S4+O1+W1+D3+V0.501+E0.016 MiB`，验证 scratch permit
  在 serde 前取得并持有到 `end()`、ownership transfer/release、checked overflow、visitor 不控制
  serde scratch且不构造 unrestricted Value；接近 4 MiB 的全 escaped string 在 output cap 成功/失败
  两支都不越 budget，接近 4 MiB 的 long integer/float 因 128-byte number preflight fail；多 session
  耗尽 32 MiB/session、192 MiB/global 后 fail whole，释放后恢复。
- config/trust：64 KiB/64 KiB+1、strict UTF-8/BOM、所有层 duplicate/unknown field、entry/arg/string
  count/byte cap、平台用户配置、missing/invalid、内置默认、按字段覆盖、global/family disable、
  默认命令缺失只禁用单 family、绝对 program、自定义 basename PATH、相对 PATH、package-manager
  entry symlink canonical pin、broken/cyclic link、canonical workspace target、Unix
  execute bit、Windows `.exe` 以及 `.cmd/.bat/.com/.ps1` 拒绝；每次 spawn 前 unchanged/changed
  identity、workspace 外 executable 最终检查后替换属于明确 threat boundary 且文案不声称原子 pin。
- URI：空格/Unicode、percent encoding、Windows drive/UNC（Windows cfg）、non-file、query、
  workspace 内外的 symlink/reparse point、missing target；工作区外的 `go.mod`/`Cargo.toml`/
  `package.json`/`pyproject.toml`/`setup.py` package source 接受并保留 dependency root，任意无此
  no-follow package root 的外部文件拒绝。
- structure：本文每个 declaration/name field、匿名排除、Markdown 名称/rank hierarchy、
  full/selection range；symbol overflow/failure 返回空且 fold 输出/预算完全不变；四种 family token
  allowlist（显式包含 TS/JS `shorthand_property_identifier` 与 pattern）、100,000 visited-node /
  65,536 range 边界、排序/去重/无重叠、成功零 token complete、
  invalid/overflow/exhaustion 空且 incomplete，并证明 token/fold/symbol 三路失败互不传播。
- LSP symbols：nested preorder/parent/detail、flat SymbolInformation 无 synthetic hierarchy、unknown
  kind→Other、empty/null、mixed variant、selection containment、cross-file、depth/count/string/result byte
  cap，任一非法项都 fail whole。
- policy：每个 ServerState × operation；无可用 server 时三种 semantic operation 均不跳，所有结果
  都无 AST/same-name fallback。
- sync：capability missing、shorthand None/Full/Incremental 和 options 的精确 `open_close`/`change`
  矩阵，所有分支 didChange 永不发送；None/openClose=false 时注入 no-follow disk revalidation 的
  相同/增长/缩短/identity change/换行规范化/stale，1 active + 1 queued、`try_send` queue-full、
  `min(500 ms, request deadline)`、open 前/每个 64 KiB read 前 cancel/generation/deadline，以及本地
  timeout 不发 LSP、late completion 丢弃；永不返回 injected read 使 lane 永久 wedged 但不产生第二
  thread，runtime drop 在 100 ms 内 detach 且 worker 不持有 runtime/session/App/process。
- lifecycle：version/`RequestIdExhausted`、三类 timeout、cancel/stale epoch、blocked stdin 不阻塞其他
  session、stdout/stderr EOF、crash/backoff、同 key 单 server 复用、超过 8 个不同 key 与同 family
  不同 root 隔离、Ready-only shutdown/exit 与 Starting/失败 forced
  cleanup；12 个不同 root 的真实 process tree 在同一 manager tick 收到 I/O failure 并与 shutdown 并发时，
  必须在一个共享 deadline batch 内完成且 pending/deferred/disk-waiting completion、backoff、stats 各恰好一次；
  Unix `process_group(0)`/getpgid/group-first TERM-KILL，Windows UTF-16 quoting/environment、
  STARTUPINFOEX `HANDLE_LIST` + creation-time `JOB_LIST`、suspended owner→resume、parent pipe non-inheritance、
  inside-outer-job success与 CreateProcess policy-rejection-before-resume、每个 Win32 failure 点保留原始错误，
  cleanup 未证明完成时 keyed retained-owner quarantine、
  tree-first termination、direct child reap、每个 `IoThreadDone` 后才 join、Backoff/Failed gate、runtime
  drop 并发规则。真实 helper descendant 继承 stdout/stderr 并在 direct child 退出后继续持有 pipe，
  断言整个 tree 被终止、EOF/done/join 完成且无残留进程；禁止 deadline 上无条件 join。
- history：invocation viewport/caret、NavigationStage、每种失败/cancel/stale/safety 原子恢复、
  results popup/pending 的 App-owned invocation、Source/Utf16 target 延迟转换、back/forward success-only commit、
  new jump 清 forward、128 上限。

### 13.2 mock-LSP 集成

抽象 `LspTransportFactory` 允许 Rust 测试注入内存 transport，覆盖完整 actor/reducer 而不 spawn。
POSIX 另用 `tests/support/mock_lsp.py` 验证真实 Content-Length stdio：

- initialize → initialized → didOpen → definition；
- Location、Location[]、LocationLink[]；
- references 多结果、implementation capability 缺失；
- nested DocumentSymbol、flat SymbolInformation、negative/string server request id reply、diagnostics ignore；
- UTF-16 emoji target、工作区外安全 package source 进入 `Dependency Source`、任意外部 target 过滤；
- delayed stale response、`$/cancelRequest`、malformed/oversize frame、stdin stall、stderr flood、crash/backoff；
- sync capability 四种 shape 与注入 disk lane 的 queue/deadline/cancel/late result；
- concurrent permit exhaustion、ownership transfer/release 和 `RequestIdExhausted` restart 后旧 epoch response；
- shutdown/exit 后 process tree 退出，无 pipe deadlock；另由 `tests/lsp_process_integration.rs` 用真实
  inherited-pipe descendant 验证 direct child 退出也不能让 cleanup 提前完成。

Windows 除 frame/state-machine/in-memory tests 外，必须有两个互相独立、都经过 production
`CreateProcessW` spawner 的真实进程用例，不依赖 Python 或 PTY：

1. `tests/app_tui_integration.rs::windows_production_spawner_runs_framed_definition_journey` 将只由
   `navigation-test-support` feature 构建的独立 Rust helper binary 显式配置为 LSP，并选择
   `framed-lsp` role；production spawner 创建真实匿名 stdin/stdout/stderr pipes，helper 只按
   `Content-Length` frames 收发，禁止使用 in-memory `LspTransportFactory`。helper 必须依次接收并断言
   `initialize`（root URI/workspace folder/UTF-16 capability）→ `initialized` → `didOpen`（精确 file URI、
   languageId、checked version `1` 与含 emoji/CJK 的完整 text）→ `textDocument/definition`（精确
   UTF-16 line/character），返回 initialize capabilities 和指向第二个安全 workspace file 的 definition
   Location。App test 在 response 前断言原 content/tree/history 未改变，response 后一次 reducer commit
   同时断言 target `ContentIdentity`、UTF-16→byte caret/range、fold reveal、viewport/tree selection 与
   history 全部应用，不允许可观察的半次 cross-file transition。drop App 后 helper 必须接收
   `shutdown` request、返回 `result:null`、再接收 `exit` notification；它把严格协议序列和 clean exit
   写到 workspace 外的 test trace，测试 bounded wait 后断言 orderly teardown、pipe EOF、三个
   `IoThreadDone` 和无残留 process。
2. `tests/lsp_process_integration.rs::windows_job_cleanup_terminates_pipe_holding_descendant` 同样用
   helper binary 的 `descendant` role 和 production spawner：direct child spawn descendant 后
   异常退出，descendant 继续持有 stdout/stderr。该用例已在 `.github/workflows/ci-pr.yml` 的
   `windows`（`Windows tests and package`）job 中配置；待 PR CI 原生运行成功后，才证明 production
   forced cleanup 经 `TerminateJobObject` 终止 descendant、两个
   pipe EOF、三个 `IoThreadDone` 到齐、direct child reap、Job/process/pipe handles 全部关闭且无残留
   PID。`src/lsp_process_windows.rs` 的私有 Win32 function table 对 create/resume/terminate/wait/query
   提供 native fault injection；单测覆盖 creation-time Job/CreateProcess rejection、resume failure 后
   verified clean termination、terminate failure retained owner、wait timeout retained owner，以及原始错误
   不被 secondary cleanup error 覆盖。产生真实测试进程的注入用例都显式切回 production API 完成清理；
   native Windows pass 仍必须由 PR CI 证明。

helper binary 通过 Cargo `required-features = ["navigation-test-support"]` 与发行构建隔离，不进入发行
package；只有 helper 可把 trace 写到 workspace 外的测试临时目录，Latte Lens production runtime 仍
保持只读。Unix 同一 descendant helper 验证 process group；macOS/Linux production PTY journey 保持
blocking，不因增加 Windows Rust helper 而删除、替换或降级。

`.github/workflows/ci-pr.yml` 的 `windows-latest` stable job 在 full `cargo test --all-targets --locked`
前显式、逐个运行：

```pwsh
cargo test --locked --features navigation-test-support --test app_tui_integration `
  windows_production_spawner_runs_framed_definition_journey -- --exact --nocapture
cargo test --locked --features navigation-test-support --test lsp_process_integration `
  windows_job_cleanup_terminates_pipe_holding_descendant -- --exact --nocapture
```

二者任一失败都阻塞 Windows job，随后 full suite 仍必须通过。另新增 `windows-msrv` job
（`windows-latest` + toolchain 1.88.0）运行
`cargo check --all-targets --all-features --locked`，并把它加入最终 `gate.needs`/result 检查。Linux 1.88 probe 不能替代
这个 cfg(windows) 编译门禁。macOS/Linux 继续由现有 matrix 跑 production PTY；Windows 用真实 Rust
process integration + release/package job完成平台验收，不把 POSIX PTY 缺失解释为 Windows 功能缺失。

### 13.3 Ratatui TestBackend

- canonical `Ctrl+D/R/O/S` 精确 modifier matrix、`Alt+Left/Right`、额外 modifier 忽略，以及
  `Ctrl+D` 不再 page-down；fold-gutter 优先于 Alt click，Alt click 优先于 selection。
- Alt+Moved 只给完整、真实、可导航 token 加下划线；离开 token、松开 Alt 后的下一次 event、
  synthetic row、popup、whitespace、行尾、incomplete index 都清除 hover 且不发 LSP。
- 四种 family `RecognizableTokenIndex` allowlist；只用 `partition_point` 验证 start 命中、end-exclusive、
  whitespace/行尾/token-end 不吸附，incomplete/no-hit 都显示状态、不创建 invocation/不发 LSP，并
  断言 key/mouse foreground 不 parse；results popup 统一显示 1-based line/UTF-16 column（emoji/CJK/combining
  mark 不使用 byte/display width）。
- results popup → Find/Search → pending cancel → quit 的 Esc 顺序，以及 Starting/loading/failure/footer。
- Definition 的 one-direct/many-popup；References 与 Implementations 的 one-or-many-always-popup；
  文件分组计数/折叠、宽屏 preview+list、窄屏 list-only、异步 preview stale rejection，以及
  键盘/鼠标单击/Enter/Esc restore focus。
- target 展开 ancestor fold，保留无关 nested fold；wrap 后滚到含 byte 的 visual row。
- caret/target highlight 不改变 line number、selection 或 copied source text。
- same-file 和 staged cross-file back/forward；目标 Preview 失败、cancel、stale、非法或截断时
  完整 tree/content/caret/viewport/history 恢复。
- stale navigation/content completion 在 refresh、文件切换和连续 semantic shortcut 后被拒绝。
- `App::new`/`with_preview_registry` 不读 HOME/PATH 且 disabled；production options 与 fake
  settings 注入确定性。

### 13.4 production PTY

在现有 hermetic Sandbox 内创建两份 Rust 文件、mock `rust-analyzer.exe`/native executable，并用
用户级 `~/.latte/latte-lens.jsonc` 覆盖 Rust 默认命令。另有无配置场景把 `PATH` 指向空 sandbox
bin，验证默认发现失败。journey 至少覆盖：

1. 打开 caller Preview，按 `Ctrl+D` 完成唯一 Definition 直接跳转；返回后 Alt+hover 产生 token
   下划线、Alt+click 完成相同 Definition；
2. 等待 `Starting rust-analyzer…` 与 definition target；
3. `Ctrl+R` 和 `Ctrl+O` 各验证一个结果也先打开 popup；多结果按文件分组并切换左侧 preview，
   Enter 或单击具体 location 才导航；LSP 返回安全 package source 时打开临时 `Dependency Source`，
   Tree/Git scope 不扩张且 Alt+Left 返回 workspace target；无 package root 的外部 path 仍被拒绝；
4. `Ctrl+S` 打开 Document Symbols；Alt+Left/Alt+Right 返回/前进；
5. 无可用 server 时明确显示
   `Code navigation is unavailable for Rust: no language server was found.` 且绝不 spawn；非法
   workspace 内 canonical executable、broken/cyclic link 或 Windows shell-backed command 被拒绝；
   workspace 外的 package-manager entry symlink 被解析并固定到 canonical target；TUI 仍可滚动和退出；
6. `ReadOnlyOracle` 证明 Git status、Git metadata 和 host config 未改变；PTY 持续 drain 到 child
   exit，mock server 无残留进程。

### 13.5 交付命令

```sh
cargo +1.88.0 check --locked
make ci
make coverage
```

`make ci` 仍是交付主门禁，`make coverage` 必须保持 85% production E2E line floor；navigation、framing、UTF
转换和 policy 的新生产逻辑不能只依赖 PTY 覆盖。若修改 package 内容再额外运行
`make package-smoke`，本方案本身不要求改变发行包布局。macOS/Linux 本地命令之外，PR 的 blocking
Windows stable test/release/package 和 Windows 1.88 check 必须通过，才满足“全平台完整实现”。

## 14. 实现依据

- 本文按 Latte Lens 的 TUI 作用域定义 canonical keymap，不把任一桌面 IDE 的快捷键当成规范，也不依赖
  macOS 顶排功能键、Command/Super forwarding 或两段 chord。results popup 的“预览 + 分组结果”布局
  借鉴成熟代码浏览器的 peek-results 模式，但 zero/one/many、history commit、窄屏降级和安全读取语义
  以第 9 节为唯一契约。
- 本仓库事实：[`Cargo.toml`](../../Cargo.toml) 固定 Rust 2024/MSRV 1.88、Preview/Tree-sitter 与现有
  `windows-sys`；[`.github/workflows/ci-pr.yml`](../../.github/workflows/ci-pr.yml) 已有 Linux 1.88、
  macOS/Linux PTY、Windows stable test/package jobs；[`src/runtime.rs`](../../src/runtime.rs) 的现值是
  512 KiB / 2,000-line Preview cap。
- Microsoft [`CreateProcessW`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw)
  定义 mutable command line、Unicode environment、STARTUPINFOEX、handle-list 与 handle ownership；
  [`UpdateProcThreadAttribute`](https://learn.microsoft.com/en-us/windows/desktop/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute)
  定义 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 的 inheritable handle 要求；
  [`PROC_THREAD_ATTRIBUTE_JOB_LIST`](https://learn.microsoft.com/en-us/windows/win32/procthread/proc-thread-attribute-list)
  与 [Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects) 定义 creation-time/nested job、
  descendant inheritance、terminate/KILL_ON_CLOSE 行为；
  [`CommandLineToArgvW`](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-commandlinetoargvw)
  给出 backslash/quote 规则。
- LSP shape/position/sync 以
  [Language Server Protocol 3.17](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)
  为准。[`serde_json 1.0.150` source](https://docs.rs/serde_json/1.0.150/src/serde_json/de.rs.html#33)
  显示 `Deserializer` 自己持有 scratch，故本文采用外部
  body-sized reservation 而不声称 visitor 控制 internal allocation。当前 pinned
  `tree-sitter-typescript 0.23.2` 与 `tree-sitter-javascript 0.25.0` 的 generated `node-types.json`
  均含 `shorthand_property_identifier` 和 `shorthand_property_identifier_pattern`。
- 2026-07-15 的独立 Rust 1.88 probe 同时通过 native 与 `x86_64-pc-windows-gnu` check，并实际 import
  本文列出的 windows-sys APIs/features；实现仍须由仓库 lockfile 与 Windows CI 重新证明。

## 15. 明确不做

- 不自动安装或提示一键安装语言服务器。
- 不扫描任意可执行文件或猜测 server；只在按语言首次请求代码跳转时解析该语言的内置命令名，
  并对解析结果应用完整 executable trust validation。
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
