# Latte Lens 项目测试卡点设计

状态：项目级测试门禁基线。本文覆盖当前已经上线的 Files、Git Changes、Preview/Search、终端交互，以及未来的 Code Agent 可观测性。

## 1. 核心决策

1. Files 和 Git Changes 是当前产品主路径，必须有 production binary E2E，不只依赖 Rust module tests 或 Ratatui TestBackend。
2. `tests/app_tui_integration.rs` 属于进程内 UI integration；`scripts/e2e_tui.py` 启动真实 binary、经过 PTY 和真实 Git/filesystem，是 production E2E。两者不能互相替代。
3. Code Agent 是后续功能域，其专项测试设计服从本文，不改变当前 Files/Git Changes 的阻断地位。
4. 用户可见行为变更遵循“最低层回归测试 + 关键用户旅程 E2E”；纯 parser/internal refactor 不强制复制成 PTY case。
5. E2E 断言当前 terminal screen 和最终外部状态，不以历史 ANSI stream 中曾出现某段文字作为通过。
6. 所有 E2E 使用隔离临时 workspace；Latte Lens 运行前后必须验证 read-only invariant。
7. 阻断用例不自动 retry，不允许通过提高 timeout、删除负向断言或缩小 fixture 规避失败。

## 2. 项目级门禁

~~~mermaid
flowchart LR
    Q0["Q0 Static / Build"] --> Q1["Q1 Unit / Contract"]
    Q1 --> Q2["Q2 In-process Integration"]
    Q2 --> Q3["Q3 Production Binary E2E"]
    Q3 --> Q4["Q4 Specialized Harness E2E"]
    Q4 --> Q5["Q5 Package / Platform"]
~~~

| Gate | 内容 | 典型命令 | 阻断范围 |
|---|---|---|---|
| Q0 | format、locked check、Clippy、MSRV、feature matrix | `make fmt-check check lint` | 所有代码改动 |
| Q1 | 纯函数、parser、bounded model、contract UT | `make test` 的 module/contract cases | 所有生产逻辑 |
| Q2 | filesystem/Git/App/TestBackend integration | `make test` 的 integration targets | Files、Git、Search、Agent state |
| Q3 | 默认 production binary + PTY | `make e2e` | Files、Git Changes、Preview/Search、terminal lifecycle |
| Q4 | 非默认专项 harness | 计划中的 Agent headless/PTY target | 需要 synthetic service 的未来功能 |
| Q5 | coverage、release package、installer、平台 | `make coverage package-smoke` | 生产交付 |

Q3 是当前 Files/Git Changes 的正式门禁。Q4 不是 Q3 的替代品：未来 Agent harness 通过后，默认 binary 的 Files/Git Changes E2E 仍然必须继续运行。

## 3. 当前测试证据

### 3.1 已有层级

| 当前文件 | 真实层级 | 主要责任 |
|---|---|---|
| `src/*` 中的 `#[test]` | Q1 | parser、layout、generation、bounded worker、preview 等纯逻辑 |
| `tests/tree_integration.rs` | Q2 | Files traversal、hidden/ignored、depth、cap、sorting |
| `tests/git_integration.rs` | Q2 | status/diff、rename、non-UTF-8、untracked、安全与只读 Git 边界 |
| `tests/repo_graph_integration.rs` | Q2 | nested repo、worktree、submodule、owner routing、limits |
| `tests/app_tui_integration.rs` | Q2 | App state + Ratatui TestBackend + keyboard/mouse |
| `tests/cli_e2e.rs` | Q3-lite | production binary help/version/invalid path |
| `scripts/e2e_tui.py` | Q3 | production binary、真实 PTY、Files/Git Changes 用户旅程 |
| `tests/agent_observability_contract.rs` | Q1/Q2 | future Agent core fake contract |

### 3.2 当前 production PTY 已覆盖

现有 `scripts/e2e_tui.py` 已经验证：

- 默认 binary 启动、立即渲染、mouse mode 和 terminal cleanup；
- Files 初始目录折叠、键盘/鼠标展开收起；
- Tree/Content divider resize 和最小宽度；
- 进入 Git Changes 时刷新真实 Git 状态并展开 changed ancestors；
- root/nested repository header 的键盘和鼠标折叠；
- nested repository diff 按 owning repository 路由；
- Git refresh 后保持 owning selection；
- Git Changes → Files scope switch 恢复各自 expansion state；
- clean file Preview；
- mouse selection、OSC 52/native clipboard 和 Ctrl+C；
- 退出时持续 drain PTY，避免 child exit deadlock。

因此这部分应被命名为 Files/Git Changes production E2E，而不是笼统的“启动 smoke”。

### 3.3 当前主要缺口

- PTY harness 是一个长场景，失败定位和按域执行不够清晰；
- Files 尚无 production E2E 覆盖 file/text search、preview find、refresh selection、partial/unsafe content；
- Git Changes 尚无 production E2E 覆盖 staged/worktree/rename/copy 完整矩阵、submodule、repository error/truncation；
- PTY E2E 尚未在运行前后显式比较 Git index/config/ref/worktree digest；
- Windows 没有真实 terminal E2E，只覆盖 Q0–Q2 和 package；
- future Agent 尚无 Q4 headless/PTY harness。

## 4. Files 测试卡点

### 4.1 Files UT / integration

| ID | 不变量 | 最低测试层 | 必须覆盖 |
|---|---|---|---|
| F-UT-001 | workspace scan 有界 | Q1/Q2 | 0/1/limit/limit+1、稳定子集、PARTIAL |
| F-UT-002 | All Files 语义固定 | Q2 | dotfile、ignored、exclude `.git`、directory-first sorting |
| F-UT-003 | shallow startup + lazy expand | Q2 | 两层启动、boundary directory、逐层异步加载、stale epoch |
| F-UT-004 | selection/expansion 稳定 | Q2 | refresh、隐藏 selection fallback、新目录默认 |
| F-UT-005 | content safety：All Files follow / 仓库 no-follow | Q1/Q2 | All Files 跟随文件软链与经目录软链的普通文件、拒绝目录/特殊/断链；仓库读取 final symlink target-text-only、intermediate symlink、reparse、FIFO、socket、device、missing/race |
| F-UT-006 | preview 有界且可回退 | Q1/Q2 | bytes/lines、binary、invalid UTF-8、provider decline |
| F-UT-007 | wrap/copy 保持原文 | Q1/Q2 | tabs、wide/combining grapheme、logical lines、highlight ranges |
| F-UT-008 | search lazy/cancellable | Q1/Q2 | ignored toggle、regex、generation cancel、result cap、no eager inventory |
| F-UT-009 | worker stale result 不污染 UI | Q1/Q2 | refresh/content/search generations 与 workspace switch |

### 4.2 Files production E2E

| ID | 用户旅程 | 必须断言 | 当前状态 |
|---|---|---|---|
| F-E2E-001 | 启动 Files | 立即看到 Files/Tree/loading→snapshot；Git metadata 不出现 | covered |
| F-E2E-002 | 展开/收起目录 | keyboard 与 mouse 各一次；当前 screen 与 hitbox 一致 | covered |
| F-E2E-003 | scope state 保持 | Git Changes→Files 后恢复 Files expansion/selection | covered |
| F-E2E-004 | clean file Preview | 选中文件后内容、line number、focus cue 正确 | covered |
| F-E2E-005 | pane resize | Tree/Content 两侧 minimum；恢复后 hitbox 不漂移 | covered |
| F-E2E-006 | mouse copy | selection 可见；OSC 52/native payload 精确；Ctrl+C 可重复 | covered |
| F-E2E-007 | Files refresh | 新增/删除文件收敛；合理保持 selection/expansion | gap |
| F-E2E-008 | File/Text search | 打开、切模式、结果预览、Enter、Esc 恢复、ignored toggle | gap |
| F-E2E-009 | Preview find/mode | Ctrl+F、next/previous、tabs/wrap、changed file Preview↔Diff | gap |
| F-E2E-010 | unsafe/partial UX | symlink/FIFO 不挂起；大 workspace 显示 PARTIAL 而非完整 | gap |
| F-E2E-011 | graceful exit | mouse mode/alternate screen 恢复；PTY drain 完成 | covered |

F-E2E-007 至 F-E2E-010 不要求一个超大场景完成。它们应使用独立 fixture/scenario，避免已有基础导航失败时掩盖搜索或安全问题。
All Files 跟随软链另有 `symlink-preview-smoke` production-binary 场景：fixture 只在
一次性测试沙箱中创建指向工作区外的文件软链与目录软链（目录 target 内含一个文件）。
断言 All Files 跟随文件软链、显示 target 文件内容；展开目录软链后能预览其中文件的
真实内容。Git Changes 对软链保持 no-follow 由 `git_changes_preview_does_not_follow_a_changed_symlink`
集成测试覆盖。

## 5. Git Changes 测试卡点

### 5.1 Git UT / integration

| ID | 不变量 | 最低测试层 | 必须覆盖 |
|---|---|---|---|
| G-UT-001 | porcelain status byte-preserving | Q1/Q2 | spaces、rename/copy、submodule、non-UTF-8、short record |
| G-UT-002 | staged/worktree 状态不混淆 | Q1/Q2 | staged only、worktree only、both、untracked、deleted |
| G-UT-003 | diff 路由正确 | Q2 | root/nested/worktree、rename source、copy source、untracked |
| G-UT-004 | repository ownership 稳定 | Q2 | ordinary nested、submodule、linked worktree、placeholder |
| G-UT-005 | discovery 有界且可解释 | Q2 | entry/repo/depth cap、invalid marker、clean repository |
| G-UT-006 | parent/child 不重复计数 | Q1/Q2 | nested suppression、submodule pointer/internal dirt |
| G-UT-007 | Git 调用只读 | Q2 | optional locks disabled、stale index stat 不刷新、无 stage/reset/config |
| G-UT-008 | refresh generation 稳定 | Q1/Q2 | stale graph/diff completion、owning selection、error snapshot |

### 5.2 Git Changes production E2E

| ID | 用户旅程 | 必须断言 | 当前状态 |
|---|---|---|---|
| G-E2E-001 | 进入 Git Changes | 先刷新；changed ancestors 展开；Diff 属于选中 row | covered |
| G-E2E-002 | repository group 导航 | root header keyboard/mouse 折叠展开 | covered |
| G-E2E-003 | nested repo ownership | nested file 显示 child repo diff，不出现 parent diff | covered |
| G-E2E-004 | refresh 保持 selection | child repo 新 change 出现，原 owning diff 保持 | covered |
| G-E2E-005 | scope 独立性 | Git expansion 与 Files expansion/selection 不互相覆盖 | covered |
| G-E2E-006 | status matrix | staged/worktree/both/untracked/deleted/rename/copy 均有正确 marker/diff | gap |
| G-E2E-007 | submodule/worktree | pointer、internal dirt、placeholder、linked worktree 分离 | gap |
| G-E2E-008 | repo error/limit | isolated error 与 PARTIAL 可见但不成为假 change row | gap |
| G-E2E-009 | Diff interaction | wrap/tabs、find、mouse copy、Preview↔Diff 保持 owning repo | gap |
| G-E2E-010 | read-only invariant | index/config/refs/worktree digest 运行前后不变 | gap |
| G-E2E-011 | review state 与 line stats | change row 显示 `+/-`；Space 标记；内容刷新后变为 changed；可重新标记 | covered |

G-E2E-006 的 PTY 断言只验证用户可见 marker 和代表性 diff；完整 porcelain 组合仍由 G-UT-001/002 负责。E2E 不应复制 parser 全矩阵。

## 6. Preview/Search 与共享交互

| ID | 用户旅程 | 适用 scope | 目标层 |
|---|---|---|---|
| S-E2E-001 | file search 打开 clean/changed file | Files + Git Changes | Q3 |
| S-E2E-002 | text search stream、toggle ignored、打开结果 | Files | Q3 |
| S-E2E-003 | in-content find 与 workspace search handoff | Preview + Diff | Q3 |
| S-E2E-004 | refresh 后保存 query 并更新 result | Files | Q2 + Q3 |
| S-E2E-005 | keyboard/mouse focus、resize、scroll | 两个 scope | Q3 |
| S-E2E-006 | clipboard 不污染 terminal/native state | Preview + Diff | Q3 |

Search 的组合语义主要由 `tests/app_tui_integration.rs` 阻断；Q3 保留三条代表性用户旅程，不把所有 regex/ignore case 移到 PTY。

## 7. Code Agent 专项关系

Code Agent 属于 Q4 specialized harness，因为 C0–C2 不允许在 production registry 注册 fake。其 UT、contract、headless 和 PTY 详细卡点见 [Code Agent 可观测性测试卡点设计](./code-agent-observability-test-gates.md)。

必须保持以下顺序：

1. 当前 Files/Git Changes Q3 始终运行；
2. Agent reducer/metadata 先走 Q1/Q2/headless；
3. Agents UI 稳定后再增加独立 PTY harness；
4. `make e2e` 最终聚合 Files、Git Changes、Search 和当期适用的 Agent E2E。

新增 Agent E2E 不能重写或删除 Files/Git Changes 断言来缩短运行时间。

Agent Hook 链路必须先用结构化 headless 证据证明 emitter、ACK、EventId 和 reducer/view 收敛，再运行 PTY presentation journey。终端文本只能证明用户看到了什么，不能单独证明 Hook 确实触发或 IPC 正确接收。真实 Agent compatibility 属于未来专项门禁，不进入当前 synthetic core E2E。

## 8. Production PTY harness 演进

### 8.1 目标结构

现有 `scripts/e2e_tui.py` 先保持行为不变，再拆成可单独执行的 scenario：

~~~text
scripts/
  e2e_tui.py                 统一 runner
  e2e/
    terminal.py              TerminalScreen、PTY drain、input helpers
    fixtures.py              isolated Git/filesystem fixture builders
    files_navigation.py      F-E2E-001..007
    search_preview.py        F/S-E2E-008..010
    git_changes.py           G-E2E-001..010
~~~

计划中的执行接口：

~~~text
python3 scripts/e2e_tui.py target/debug/latte-lens --scenario files
python3 scripts/e2e_tui.py target/debug/latte-lens --scenario git-changes
python3 scripts/e2e_tui.py target/debug/latte-lens --scenario search-preview
python3 scripts/e2e_tui.py target/debug/latte-lens --scenario all
~~~

拆分提交必须先证明 `--scenario all` 保留当前全部 marker、absent marker、clipboard 和 terminal cleanup assertion；不能在“重构 harness”名义下减少覆盖。

### 8.2 Fixture 隔离

每个 scenario 独立创建临时目录并显式声明：

- isolated HOME、XDG、Lens state root 和 runtime root；
- initial file tree；
- Git repositories、index、refs、submodule/worktree relation；
- worktree mutations；
- expected visible rows 和 owning repository；
- expected read-only digest；
- expected child/endpoint/temp cleanup receipt；
- timeout budget。

不复用开发者全局 Git config、clipboard、HOME 或 state root。macOS native clipboard smoke 运行后恢复原内容；CI 默认使用 OSC 52。只有必须覆盖 native clipboard 的 scenario 可以访问宿主 clipboard，并将恢复结果写入 cleanup receipt。

### 8.3 Read-only oracle

Q3 开始前和退出后计算：

- workspace regular-file content/metadata snapshot；
- 每个 repository 的 `.git/index`、`config` 和 refs digest；
- symlink target text；
- Git status byte snapshot；
- 真实用户 Git/Lens 配置的 digest 或“不存在”状态；
- sandbox 外 allowlist 路径的 mutation snapshot。

只有 scenario 明确在 Latte Lens 运行期间由测试驱动创建的文件允许变化。Latte Lens 自身不能 stage、reset、refresh index stat、修改 config、refs、worktree 或 nested repository。

## 9. E2E 稳定性规则

- 单个 screen wait 上限 10 秒；scenario 总预算 60 秒；suite 总预算 4 分钟。
- Runner 提供 `--self-test`，验证 sandbox、PTY drain、deadline watchdog、failure capture 和 cleanup oracle 后，业务 scenario 才允许执行。
- 等待 marker/状态，不使用固定 sleep 表示业务完成。
- 失败输出 scenario ID、current screen、missing/forbidden markers 和最多 200 KiB terminal tail。
- 每次执行生成 bounded `summary.json`；失败额外生成 sanitized terminal/screen tail 和 `cleanup.json`，并区分 readiness、screen convergence 与 child-exit timeout。
- child 退出期间持续 drain PTY；不能先阻塞 wait 再读 output。
- 一个 scenario 一个临时 workspace；不能依赖执行顺序。
- CI 不自动 retry。平台独有失败保持阻断。
- 终端断言同时包含正向和 absent marker，防止陈旧内容或错误 scope 误通过。
- 坐标交互前先断言 divider/row 的当前 cell，避免布局变化造成点击错误对象。

## 10. CI 与命令规划

当前 `make e2e` 是 Files、Git Changes、Search/Preview 和 Code Navigation 的 blocking production E2E：

| Target | 内容 |
|---|---|
| `make e2e-self-test` | sandbox、PTY、watchdog、evidence、cleanup oracle |
| `make e2e-files` | F-E2E cards |
| `make e2e-git` | G-E2E cards |
| `make e2e-search` | F/S search-preview cards |
| `make e2e-navigation` | folding、structure navigation、LSP navigation/lifecycle cards |
| `make e2e` | 聚合所有当前 required scenarios |

CI 映射：

- Linux quality：Q0–Q2；
- Linux/macOS PTY：Q3 Files + Git Changes + Search + Code Navigation，以及 required 的 Agent journey；
- Windows：Q0–Q2、package；ConPTY 未持续验证前不声称 Q3；
- Agent jobs：按专项文档增加 Q4，不替代主 PTY job；
- coverage-unit：分母以 Makefile 的 `UT_COVERAGE_IGNORE_REGEX` 为准。当前过滤后由 Q1 直接单测负责的 surface 包含 `clipboard.rs`、`diff.rs`、`folding.rs`、`lsp.rs`、`lsp_process.rs`、当前 target 编译的 process backend、`navigation.rs`、`preview.rs`、`search.rs` 和 `text_layout.rs`，保持 93% line floor；
- coverage-e2e：用 production binary + PTY 执行全部 required scenarios，分母以 Makefile 的 `E2E_COVERAGE_IGNORE_REGEX` 为准。当前 production PTY surface 包含 `app.rs`、`folding.rs`、`lsp.rs`、`lsp_process.rs`、当前 target 编译的 process backend、`main.rs`、`navigation.rs` 和 `ui.rs`，保持 85% line floor；
- coverage-agent：统计完整 synthetic Agent Core 的 G1–G3 Rust 执行路径，保持 80% line floor；不得通过排除新增 Agent 模块维持数字；
- 被上述过滤器排除的其余边界模块仍由 Q2 integration/contract tests 独立阻断；`lsp_process_unix.rs` 与 `lsp_process_windows.rs` 只在各自适用的 target 编译和计量，覆盖率报告不替代上述 Windows native PR CI 证据。`make coverage` 顺序执行三个 coverage gate；
- package：Q5，并验证只有 production binary/资产。

## 11. 改动与卡点映射

| 改动 | 最小阻断集合 |
|---|---|
| `tree.rs` / Files traversal | F-UT-001..004 + 受影响 F-E2E |
| `git.rs` parser/read-only boundary | G-UT-001..003/007；用户 marker/diff 变化时加 G-E2E（当前 review state/line stats：G-E2E-011） |
| `repo_graph.rs` | G-UT-004..006 + G-E2E-003/004/007/008 |
| `preview.rs` / content safety | F-UT-005..007 + F-E2E-004/009/010 |
| `search.rs` | F-UT-008 + S-E2E-001..004 |
| `runtime.rs` generation/backpressure | F-UT-009/G-UT-008 + 对应 scope refresh E2E |
| `app.rs` / `ui.rs` | Q2 TestBackend + 对应 Files/Git/Search Q3 |
| `agent/*` | 项目 Q0–Q2 + Agent 专项门禁 |
| CLI/release/package | CLI E2E + Q3 smoke + Q5 package |

纯内部重构若证明 observable behavior 不变，可以不新增 PTY case，但必须运行已有相关 Q3 scenario。任何用户可见状态、导航、selection、diff/preview、错误或 terminal lifecycle 变化都需要更新对应 E2E 卡。

## 12. 测试卡模板

~~~text
ID:
Product domain: Files / Git Changes / Preview / Search / Agent / CLI
Invariant or user journey:
Layer: Q1 / Q2 / Q3 / Q4 / Q5
Fixture:
Actions:
Expected current screen/state:
Forbidden screen/state:
Read-only oracle:
External isolation oracle:
Platforms:
Timeout budget:
Failure evidence:
Cleanup receipt:
~~~

## 13. 实施顺序

1. 先把现有 `scripts/e2e_tui.py` 的断言映射为 F-E2E/G-E2E card，并保持单命令全部通过。
2. 抽取共享 TerminalScreen/PTY helper，并先落 `--self-test`、外部 oracle 和 cleanup receipt。
3. 拆分 Files 与 Git Changes scenario；重构阶段不减少断言。
4. 优先补 F-E2E-007/008、G-E2E-006/010：Files refresh/search、Git status representative matrix、read-only oracle。
5. 再补 unsafe/partial、submodule、repo error 和 Diff interaction。
6. Code Agent 先完成 structured headless 证据，再推进 PTY；主 Q3 gate 始终保留。

项目测试完成不能只报告总测试数或覆盖率；必须分别报告 Files、Git Changes、Search/Preview、Agent 各自 required card 的通过状态。
