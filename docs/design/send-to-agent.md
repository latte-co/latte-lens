# Send Selection to Agent 设计

状态：草案评审。本期定义「把 Latte Lens 内容预览区的选区文本发送到一个正在运行的
Code Agent 会话输入框」的交互、通道抽象、状态机与测试边界。

## 1. 背景与目标

Lens 是 multi-agent 终端里的只读仓库查看器；同一终端 workspace（如 Herdr）中通常
并行运行着多个交互式 Code Agent 会话（Claude Code、Codex、OpenCode、TraeX 等）。
用户在预览代码时经常需要把一段代码送进 agent 的输入框，再补充问题后亲手发送。

本期提供：

- 划词后，通过**键盘**或**鼠标修饰手势**打开 agent 会话选择器；
- 选择器打开后可**直接键入一行注释**，并在「锚点模板 / 纯文本」间切换；
- 确认后把**组装后的消息**（锚点 + ▎注释 + 围栏选区）以 bracketed paste 语义
  投递到该会话输入框，**不回车、不提交**；
- 终端 workspace manager 作为汇聚层，Lens 只依赖其会话发现与文本注入能力，
  不与任何单一 agent 的私有协议耦合。

非目标（本期不做）：

- 不替用户提交（不调用 `agent prompt` 一类"粘贴并回车"接口）；
- 注释仅单行（多行/多段批注留作后续）；
- 不支持编辑模态（`EditSession`）选区；编辑态文本未经终端转义清洗，需独立安全评审；
- 不列出未被识别为 agent 的普通 shell pane；
- 不实现 Herdr 之外的终端适配（通道以 trait 预留）；
- 不做鼠标右键 / 中键 / Ctrl+点击入口（实测均被 iTerm2 等终端保留占用）。

## 2. 终端实测约束（iTerm2 + Herdr，2026-09）

通过 SGR 1006 鼠标探针实测：

1. 鼠标 down 时带 Ctrl（Ctrl+click）触发 iTerm2 本地菜单，事件不到达 pty；
2. 拖拽途中按下 Ctrl，**motion 事件稳定携带 ctrl 修饰位**（`cb = 32 + 16 = 48`）；
3. Ctrl 按住时松开鼠标，up 事件被 iTerm2 吞掉（无 release 到达 pty）；
4. 普通（无修饰）up 始终可靠到达；
5. 键盘 `Ctrl+E` 经 kitty keyboard protocol（Lens 已启用
   `DISAMBIGUATE_ESCAPE_CODES`）稳定上报为 `CSI 99;101;5u`；
6. Cmd（SUPER）+ 字母在 iTerm2 默认配置下全部被本地菜单保留，不上报；
7. SGR 1006 编码无 super 修饰位，鼠标通道只可能识别 Ctrl。

结论：

- 键盘主入口使用 `Ctrl`，三平台 canonical 一致（沿用全仓现有约定）；
- 鼠标手势**不能**依赖"带修饰的 up"，改用拖拽途中 ctrl 上升沿的 sticky arming；
- Cmd 仅作为键盘入口在支持透传的终端（kitty/wezterm/ghostty）上顺带接受，
  与现有 Ctrl+C/Cmd+C、Ctrl+S/Cmd+S 写法一致，不在文档中承诺。

## 3. 用户交互

### 3.1 入口 A：键盘（主入口，全平台）

前置门控（全部满足才激活）：

1. 运行环境可用（`AgentTargetProvider::available()`，见 §4）；
2. Content 面板聚焦；
3. `ContentMode::Preview` 或 `ContentMode::Diff`，且不在编辑模态。
   Preview 选区带源码坐标，使用锚点模板；Diff 选区的锚点从补丁自身推导
   （最近的 `diff --git a/… b/…` 头 + hunk 新文件行号），围栏为 ```diff
   且截断时不加数字侧栏（保留 `+`/`-`/`@@` 补丁语义）；选区跨多个文件头
   时无单一归属，回退 Plain（注释仍可加）；
4. 当前 tab 存在非空内容选区（`selected_content_text().is_some()`）。

按下 `Ctrl+E`（同时接受精确的 SUPER 别名）→ 打开 agent picker。

选区存活期间，footer 在正常帮助文本中插入 `^E send to agent`（遵循
`keyboard-shortcuts.md` §4.6 的上下文感知 footer 约定）。

### 3.2 入口 B：拖拽中 Ctrl arm（鼠标增强）

`ContentSelection` 新增 `send_armed: bool`，初始 `false`。

- 拖拽（左键按住、产生位移）期间收到首个带 Ctrl 修饰的 motion 事件 →
  `send_armed` 在 true/false 间翻转（toggle，重复按 Ctrl 可解除）；
- armed 期间内容区底部显示常驻提示行：
  `Release: send to agent · press Ctrl again to cancel`；
- 松开鼠标（普通 up，Ctrl 已可松开）：
  - `send_armed == true` 且选区非空 → **跳过松开即复制**，打开 agent picker；
  - 否则保持现状（复制选区）；
  - 无论哪种，结束后 `send_armed` 复位为 false。

判定只使用 motion 上的修饰位与普通 up，不依赖带修饰的 up（§2 第 3 条）。
不上报修饰位的终端上 `send_armed` 永不置位，行为与今天逐字节一致。

### 3.3 Agent picker

复用 navigation results popup / tab palette 的交互范式（键盘 + 鼠标同构）。
弹窗从上到下为：agent 行（最多 6 行，超出窗口滚动）、注释输入条、
模板/截断状态行、载荷实时预览、帮助行。

- 标题：`Send selection to agent · <actual>/4096 B`（组装后的真实字节数）；
- 每行：状态 glyph + agent 名（`claude`/`codex`/…）+ 状态词
  （idle/working/blocked/done/unknown）+ pane 标题 + `pane_id`；
- 排序：`cwd` 规范化后等于 Lens 当前仓库根（`App::root`）的会话置顶，
  其余按 agent 名、标题稳定排序；
- 过滤：排除 Lens 自身 pane（`HERDR_PANE_ID`）；`blocked` 会话渲染为置灰且不可选；
  仅列出被识别为 agent 的 pane；
- **注释输入条默认聚焦**：打开即可键入（真实终端光标落在输入条），
  不需要模式切换快捷键；agent 行只用 `↑/↓` 选择（方向键事件与可打印字符不冲突）；
- 键控：
  - 可打印字符（含 Shift）→ 写入注释；`Backspace` 删除、`←/→/Home/End` 移动光标；
  - `↑/↓` 移动 agent 选择（跳过 blocked，环绕）；
  - `Tab` 在「锚点 / 纯文本」两模板间切换（无锚点时锚点不可达）；
  - `Enter` 确认、`Esc`/点击外部取消；
- 总是弹出：即使只有一个可选目标也要求显式确认，避免误发；
- 空态：没有可选会话时显示
  `No active agent sessions detected.`；
- 打开 picker 时异步刷新会话列表；列表到达前显示 `Discovering agents…`，
  注释输入不被发现过程阻塞。

### 3.4 载荷模板、注释与截断

确认时由纯函数 `build_payload(selection, anchor, annotation, template)` 组装：

- **模板 Anchor（源码坐标选区的默认值）**：

  ```text
  src/app.rs:8415-8417
  ▎这里为什么不用 buffer？

  ```rust
  ...选区原文...
  ```
  ```

  - 锚点单行 `path:line`、多行 `path:start-end`（闭区间，1 基，沿用
    grep/编译器与 lens 自身状态栏的裸行号写法，可被终端 cmd+click）；
  - 围栏语言由扩展名映射（`fence_language`），未知扩展名开裸围栏；
    选区内含更长反引号游程时围栏自动加长；
  - 仅源码坐标预览可用：成功 Preview、显示行号、有 content identity；
    渲染态 Markdown、搜索快照无坐标，强制 Plain。Diff 模式有独立的推导锚点
    （见 §3.1 第 3 条）：文件取选区起点上方最近的 `diff --git` 头新侧路径，
    行号取选区内首个/末个 hunk 新文件行号（纯删除选区回退到 hunk 的新起始
    行），围栏语言固定为 `diff`；选区跨过后续文件头时不生成锚点。
- **模板 Plain（Tab 切换，或无锚点时）**：选区原文；有注释时注释段在前，
  不虚构文件名与围栏。
- **注释**：每行加 `▎` 前缀（提问/指令通用，且在 composer 与聊天记录中与
  引用代码视觉分离）；单行、UTF-8 硬上限 512 B（`MAX_ANNOTATION_BYTES`），
  控制字符拒收。
- **截断**：整条消息硬上限 4 KiB（`MAX_SEND_BYTES`，跨平台一致）。
  超出时按**整行**保留选区首部（约 55%）与尾部、省略中间：
  - Anchor：锚点仍写完整原始区间并加
    ` (N lines selected, M omitted)` 后缀；源码选区的保留行带 `行号│` 侧栏，
    Diff 锚点不加侧栏、逐字保留补丁行；
    中间插入
    `⋮ ── omitted M lines (X B) see path:起-止 ──`；
  - Plain：同样首尾保留，省略标记不带文件名；
  - 极端单行超长退化为字符边界截断；最终组装再做一次精确收缩，
    保证结果绝不超过 4096 B 且不切半个 UTF-8 字符；
  - picker 预览与状态行实时反映截断（`⚠ M of N lines omitted (middle)`）。

### 3.5 投递与反馈

确认后：

1. 后台 worker 执行 `send-text`（不回车）+ focus 目标 pane；
2. picker 关闭，状态栏显示 `Staging…`；
3. 成功：`Sent <N> chars to <agent> · <pane_id>`，清除内容选区，
   终端焦点由 workspace manager 切到目标 pane（用户随即补话、亲手 Enter）；
4. 失败：状态栏错误消息（见 §6），选区保留以便重试。

## 4. 通道抽象

新增顶层模块 `src/send_agent.rs`（无条件编译；与入站观测的
`src/agent/` 解耦，出站通道不依赖 `agent-observability` feature）：

```rust
pub enum AgentLifecycle { Idle, Working, Blocked, Done, Unknown }

pub struct AgentTarget {
    pub agent: String,        // "claude" | "codex" | ...
    pub pane_id: String,      // 唯一投递地址
    pub title: String,        // pane 标题（去控制字符）
    pub cwd: PathBuf,
    pub status: AgentLifecycle,
    pub selectable: bool,     // blocked 为 false
    pub same_workspace: bool, // cwd 规范化后等于 Lens 仓库根
}

pub trait AgentTargetProvider: Send + Sync {
    /// 环境是否可用（HERDR_ENV == "1"）。
    fn available(&self) -> bool;
    /// 发现全部 agent pane；输出有界、调用有超时。
    fn discover(&self, workspace_root: &Path) -> anyhow::Result<AgentDiscovery>;
    /// 把文本作为字面 argv 参数投递到 pane，不附加回车。
    fn send_draft(&self, pane_id: &str, text: &str) -> anyhow::Result<()>;
    /// 把 workspace 焦点切到目标 pane。
    fn focus_pane(&self, pane_id: &str) -> anyhow::Result<()>;
}
```

同模块内的 `SendToAgentState`（`Closed/Discovering/Picking/Sending` 状态机）
是不依赖 I/O 的纯 reducer，App 拥有 generation 与 runtime 通道。

唯一生产实现 `HerdrProvider`：

- 可执行文件：`HERDR_BIN_PATH`，否则 `herdr`（PATH 查找，不在 Lens 进程内做 shell）；
- 可用条件：`HERDR_ENV == "1"` 且能取到 `HERDR_SOCKET_PATH`；
- `discover()`：`herdr agent list`，解析 JSON
  `result.agents[].{agent,pane_id,terminal_title_stripped,cwd,agent_status}`，
  未知状态映射 `Unknown`；
- `send_draft()`：`herdr pane send-text <pane_id> <text>`（argv 直传，无 shell）；
- `focus_pane()`：`herdr agent focus <pane_id>`（失败不影响发送成功语义，
  降级为仅状态栏提示）；
- 全部 argv 以独立参数传递；对 stdout/stderr 设上限，进程设超时
  （discover 2s，send 3s），错误 fail-closed 并给出可操作消息。

纯解析与排序逻辑写成不 spawn 的自由函数，使用合成 JSON fixture 单测
（遵循 `docs/testing/code-agent-observability-test-gates.md`：
fake/default 实现不得进入生产注册表）。

测试注入：`App` 持有 `Box<dyn AgentTargetProvider>`，默认 `HerdrProvider`；
参照现有 `App::with_system_open_disabled` 增加测试构造入口注入 fake provider。
生产代码路径不出现 fake。

## 5. Runtime 与状态机

复用 external-open 的 generation/worker/completion 骨架（`src/runtime.rs`）：

```
Idle
  ── Ctrl+E / armed release（选区非空、available）──▶ Discovering
Discovering
  ── targets 到达 ──▶ Picking(targets, selection + anchor 快照,
  │                            annotation/template 草稿)
  ── 发现失败/无可选 ──▶ 状态栏消息，回到 Idle（选区保留）
Picking（注释输入条始终聚焦；键入写注释，↑↓ 选人，Tab 切模板）
  ── Enter(target) ──▶ Sending(pane_id, build_payload 结果)
  ── Esc / 点击外部 ──▶ Idle（选区保留）
Sending
  ── 成功 ──▶ Idle（清选区、状态栏 Sent、focus）
  ── 失败 ──▶ Idle（状态栏错误、选区保留）
```

- 发现请求带 generation；picker 打开期间切 tab/重置内容会作废结果；
- 发送期间忽略重复确认；
- 所有 spawn 在 runtime worker 线程，`ui.rs` 渲染不做任何 I/O
  （AGENTS.md §7 硬约束）。

## 6. 错误状态

| 情况 | 用户可见行为 |
| --- | --- |
| 非 Herdr 环境 | 入口静默不可用（footer 无提示） |
| `herdr` 二进制缺失/超时 | picker 打开后显示 `herdr unavailable: <reason>` |
| agent list JSON 异常 | 同上，记为发现失败，不弹空白列表 |
| 无 agent / 仅 blocked | 空态文案；blocked 项置灰 |
| send-text 非零退出/超时 | `Send failed: <reason>`，选区保留 |
| 目标 pane 在列表刷新后消失 | `Agent pane gone, re-pick`，回到 picker |

## 7. 键位与文档同步

`docs/design/keyboard-shortcuts.md`：

- §3.1 全局命令新增 `Ctrl+E`：内容选区非空时发送选区到 agent；
- §2 作用域表补一行说明；红线第 3 条增补例外措辞：
  "SUPER 不作为 canonical 绑定；沿用复制/保存现状，在终端确实透传 SUPER 时
  接受同字母 Cmd 别名（Ctrl 仍为三平台主路径）"；
- README "Inside the TUI" 控件表、`src/ui.rs` footer 各宽度/模式分支同步。

## 8. 安全边界

- 只读产品边界不变：本功能只 spawn Herdr CLI 且仅执行固定子命令与 argv 参数；
- 载荷是预览缓冲文本（已 `sanitize_terminal_text`），不读盘、不拼接 shell；
- 不发送 pane 中的密钥/环境变量；token/socket 路径不进入载荷或日志；
- 不把任何"自动回车/自动批准权限"能力暴露给 Lens；
- 编辑模态选区明确排除（原始文件字节未经清洗，存在终端转义注入面）。

## 9. 测试计划（已落地）

1. **纯逻辑单测（`src/send_agent.rs` 内联 `#[cfg(test)]`）**：
   - `herdr agent list` 真实形态 JSON fixture 的解析：全状态枚举、缺字段、
     标题控制字符清洗、cwd 置顶排序、自身 pane 排除、blocked 置灰；
   - 载荷组装：单行/多行锚点头、▎注释位置、Plain 无锚点回退、未知扩展名裸围栏、
     内嵌反引号升级围栏、4 KiB 首尾截断（侧栏行号/省略标记/总长不超上限）、
     注释 512 B 硬上限与控制字符拒收、Tab 模板门控、注释光标按字符移动；
   - picker reducer 的 generation 失效防护、键盘移动跳过 blocked、
     stale failure 忽略。
2. **App 集成（`tests/send_to_agent_integration.rs`，注入 fake provider，
   走真实 runtime worker + completion 通道）**：
   - 无后端时 `Ctrl+E`/footer 完全静默；
   - `Ctrl+E` → picker → Enter 默认锚点模板投递到正确 pane、焦点切换、
     成功清选区；直接键入注释后载荷含锚点/▎注释/围栏；Tab 切纯文本后发原文；
     picker 渲染注释输入条、占位提示与载荷预览；
     Git Diff 选区打开 picker 时无锚点、Tab 无法切到锚点、逐字发送 unified-diff
     原文（保留 `+`/`-` 前缀）；
     Esc 取消保留选区且不发送；仅有 blocked 会话时关闭 picker；
   - 拖拽中 ctrl 上升沿 arm：普通 up 不开 picker，armed up 开 picker。
3. **argv 协议（`tests/send_agent_protocol.rs`，POSIX）**：用一个固定行为的
   fake `herdr` shell 脚本（`HERDR_BIN_PATH` 指向它）端到端锁住
   `agent list` / `pane send-text <pane> <payload>` / `agent focus <pane>`
   的确切 argv 契约，多行载荷以单一参数逐字节到达、无额外回车；不依赖真实 Herdr。
4. `make ci`（fmt/check/lint/test/script-test/e2e/agent-e2e-tui/package-negative）全绿。

## 10. 分阶段提交建议

1. `feat(send)`：通道类型 + Herdr provider + 解析/排序单测；
2. `feat(send)`：runtime 发现/投递 + App picker 状态机 + headless 测试；
3. `feat(send)`：Ctrl+E 键位、footer/文档三方同步；
4. `feat(send)`：拖拽 ctrl arming + E2E。
