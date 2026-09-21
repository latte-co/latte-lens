# Herdr 只读 Observation Provider 设计（Session 感知增强）

状态：**草案评审，尚未实现。**

本期定义「把聚合型终端运行时 Herdr 作为 read-only ObservationProvider 接入
Latte Lens 的 Code Agent 可观测性架构」，落地
[Code Agent 可观测性设计](code-agent-observability.md) §3.3 预留的
「聚合型终端运行时 read-only bridge」：字段与能力映射、adapter/provider
实现契约、与既有 Hook 证据的身份合并与仲裁、轮询运行时、测试边界与分阶段
实施。

## 1. 背景与目标

Lens 当前的 agent 感知是 **hook 事件驱动 + 轻量 collector**：各 Code Agent 的
hook 事件经 `latte-lens hook` 规范化，Lens 在线走 live IPC，离线降级写
session metadata。该路径有四个设计文档自认的结构性盲区：

| # | 盲区（可观测性设计原文） | 根因 |
| --- | --- | --- |
| 1 | 「已经运行但 Lens 启动后没有再触发 Hook 的 session，不会被实时确认」 | 只有事件触发才能感知，无 current-state 快照 |
| 2 | Activity 「只有 30 秒 lease 的事件状态点，没有 current-state snapshot」 | 漏 Hook 后只能回退 Unknown |
| 3 | 「MetadataOnly 不能证明进程仍在运行」 | 无 liveness/presence 通道 |
| 4 | 「AgentTopology 没有现存 topology snapshot」 | SubagentStart/Stop 只是增量事件 |

Herdr（<https://herdr.dev>，Rust 终端 workspace 管理器）恰好持有这四类缺失
证据：它通过自有 integration hook 建立 pane↔agent session 绑定，通过屏幕
检测维护每个 agent pane 的 current status，拥有全量 pane/workspace 拓扑与
liveness。[Send Selection to Agent](send-to-agent.md)（#26）已经验证了
「Lens 作为 Herdr CLI 客户端」的执行纪律（argv 直传、无 shell、bounded、
fail-closed）；本设计把同一模式反向用于 inbound 感知，与既有 Hook 路径形成
**snapshot-first + event reconcile** 互补：Herdr 提供 current-state 快照，
Lens 自有 Hook 提供精确增量事件，两路证据在既有 reducer 中仲裁。

本期提供：

- 新 Observer `herdr/cli-snapshot`：后台有界轮询 `herdr agent list`，
  decode 为 `SnapshotEnvelope`（`AcquisitionMode::AggregatedSnapshot`）；
- 与 `anthropic/claude-code-hook` 等直连 Hook 证据的**可证明身份合并**
  （同 SubjectNamespace + 同 AuthorityId + 同 native session UUID →
  同一 SessionKey）；
- 一次补齐盲区 1/2/3 的 current-state 感知，盲区 4 的 pane 级拓扑归属性
  证据（vendor subagent 拓扑仍以 Hook 事件为准）；其中盲区 2 依赖本设计
  **新增**的 Activity Observational 降级仲裁规则（§6.3）——这是对
  `arbitrate_activity` 的一处受限扩展，不是既有行为。

非目标（本期不做）：

- 不调用 Herdr 的任何写/控制命令（`pane send-text`、`agent prompt/focus/
  start/attach` 等）；provider 与 #26 的出站通道共享执行纪律但不共享能力；
- 不读取 `herdr agent read` 的终端屏幕内容，不采集 `terminal_title`
  （含用户 prompt 派生文本，隐私边界见 §12）；
- 不直连 `herdr.sock` 私有协议，只使用官方声明为稳定契约的 CLI；
- 不采集 `--machine` 远端 SSH machine 的 session：现有 AuthorityId 是
  与机器无关的静态常量（§5.3），本机与远端的同 UUID 条目会被无条件
  误合并，身份歧义在现模型内无法表达，见 §12；
- 不改变 known_count/live_count 语义、Agents UI 既有契约与 Hook 路径行为；
- 不为 Herdr 生成 resume/attach/控制能力（沿用可观测性设计 §10.10）。

## 2. 实测约束（Herdr 0.9.1，Linux，2026-09）

### 2.1 `herdr agent list` 真实输出形态

默认输出单行 JSON（已格式化示意）：

```json
{
  "id": "cli:agent:list",
  "result": {
    "agents": [
      {
        "agent": "claude",
        "agent_session": {
          "agent": "claude",
          "kind": "id",
          "source": "herdr:claude",
          "value": "3aa69e02-a2ab-4f63-9398-e70f05fe5182"
        },
        "agent_status": "working",
        "cwd": "/data00/projects/latte-co",
        "focused": false,
        "foreground_cwd": "/data00/projects/latte-co",
        "pane_id": "w2A:p1",
        "revision": 12,
        "state_change_seq": 125,
        "tab_id": "w2A:t1",
        "terminal_id": "term_65bc277e8c8291",
        "terminal_title": "◑ …",
        "terminal_title_stripped": "…",
        "workspace_id": "w2A"
      }
    ],
    "type": "agent_list"
  }
}
```

关键字段语义（按实测与 Herdr 0.9.1 skill 文档交叉确认）：

| 字段 | 语义 | Lens 用途 |
| --- | --- | --- |
| `agent` | Herdr 识别的 agent 产品名（`claude`/`codex`/`opencode`/…，共 18 种 integration） | SubjectNamespace 映射（§5.2） |
| `agent_session.kind == "id"` | 原生 session ID（来自 Herdr 自有 hook 上报） | 与直连 Hook 证据可证明合并 |
| `agent_session.source` | 证据来源标注（`herdr:claude` 等） | 合并证明链的诊断信息 |
| `agent_status` | `working`/`idle`/`blocked`/`done`/`unknown` | Activity current-state（Observational） |
| `pane_id`/`workspace_id`/`tab_id`/`terminal_id` | server 内作用域的拓扑标识 | PresenceRef 与归属性证据 |
| `state_change_seq` | 每 agent 单调递增的状态变更序号 | 变更检测与 epoch 回退检测 |
| `revision` | pane revision | 诊断信息，不参与语义 |
| `cwd`/`foreground_cwd` | 原始 cwd | 仅经 WorkspaceHint keying 匹配，raw 值不落盘 |
| `focused` | 当前焦点 pane | 诊断信息，不采集 |

### 2.2 稳定性与环境边界

- Herdr 官方声明 CLI 是权威契约（skill：`The installed binary is the
  authority`），但 JSON 形状无正式版本承诺 → `InterfaceStability::
  VersionedExperimental`，probe 期字段校验，形状漂移 fail-closed；
- 无公开事件订阅 API；`herdr agent wait --until <status> --timeout <ms>`
  是单目标 bounded long-poll（S2 备选，S1 不用）；
- 运行环境门控：`HERDR_ENV == "1"` **且** `HERDR_SOCKET_PATH` 非空
  （instance ID 由 socket path 派生，缺失则无法定位 server，直接视为
  不可用）。注意与 #26 的差异：`HerdrProvider::available()`
  （`src/send_agent.rs`）只检查 `HERDR_ENV == "1"`，不检查 socket
  path；`docs/design/send-to-agent.md` 对此同样表述过宽，属既有文档
  漂移，应另行修正——本设计不声称「一致」。可执行文件解析顺序
  `HERDR_BIN_PATH` → PATH 中的 `herdr`（测试注入用）；
- pane/agent/workspace ID 均为单 server 作用域（skill 明示「IDs and live
  agent names are scoped to one server」）；不加 `--machine` 时只访问
  本机 session。

## 3. 能力映射与证据边界

遵循可观测性设计 §3.3 的能力映射表，逐条落实到 `InstanceContract`：

| Herdr 证据 | Lens capability | 说明 |
| --- | --- | --- |
| agent pane 在 `agent list` 中出现 | Presence：Confirmed，Authoritative（scope = 该 server 的 agent panes） | `PresenceOp::Seen`，不自动建立 session |
| 条目从 Complete snapshot 消失 | 该 observer 的 presence tombstone → `PresenceOp::Released` | pane 终态证据；**不是** vendor SessionEnd |
| `agent_status: working/idle/blocked` | Activity：Partial，**Observational**，provenance `AggregatedScreenInference`，带 snapshot 刷新 lease | Herdr status 来自其 hook 绑定 + 屏幕检测，无 per-domain authority 证明 |
| `agent_status: done` | 仅 presence 终态证据 | 进程/屏幕级终态；Lifecycle 保持 Unsupported，不合成 Ended/Failed |
| `agent_session.kind == "id"` + 可映射 `agent` | Session identity：Partial→可合并 | 经 IdentityKeyer 生成与直连 Hook 相同的 SessionKey（§5.3） |
| `cwd`/`foreground_cwd` | Observed workspace locator | 经 WorkspaceHint 安全映射；不精确匹配则不出现在当前 workspace 视图 |
| subagent / change / artifact / turn / permission / tool | Unsupported | 无独立可验证证据，不猜测映射 |
| `terminal_title`/`terminal_title_stripped` | 不采集 | 用户 prompt 派生文本，隐私边界未定（§12） |

五个 session 维度的影响：

- **Discovery**：仅 Herdr 证据不改变 Discovery；已由 Hook 建立的
  `StartConfirmed` 不被降级，Herdr 首次观察到的既有 session 若无 Hook
  证据则按 `DiscoveredMidSession` 进入（首观察时间 = snapshot
  `captured_at`，不冒充 `started_at`）；
- **ObservationMode**：Herdr snapshot 到达即 `LiveObserved`（它是 Lens
  本次运行期间的 live provider evidence）；
- **Lifecycle**：Herdr 不提供 lifecycle 证据（Unsupported）；`done` 与
  条目消失只影响 presence/activity；
- **Activity**：Herdr 候选为 Observational，永不下调 Hook 侧
  Authoritative 证据的胜出（§6.3）；仅当无未过期 Authoritative 候选时
  （恰是盲区 2 场景），Observational 候选按 §6.3 新增的降级规则胜出，
  而不是被现有 `arbitrate_activity` 的 Authoritative-only 过滤直接丢弃为
  `Unknown`；
- **Freshness**：snapshot 刷新即刷新 lease；轮询间隔 + 一次容错轮次后
  未见新 snapshot → `Stale`，Activity 回退 Unknown（不合成终态）。

## 4. Provider 设计

新增 `src/agent/herdr_provider.rs`（生产代码，无条件编译）与
`src/agent/herdr.rs`（adapter，§5）。Provider 实现
[`ObservationProvider`](../../src/agent/provider.rs) 全部方法：

```rust
pub const HERDR_SNAPSHOT_OBSERVER_ID: &str = "herdr/cli-snapshot";

pub struct HerdrSnapshotProvider { /* poll 线程句柄、有界缓存、draining 标记 */ }
```

### 4.1 执行纪律（复用 #26 模式）

- 子进程 argv 独立参数传递，无 shell；
- argv allowlist 只允许两个形状：`herdr agent list`（快照采集）与
  `herdr --version`（§4.2 `discover()` 的 instance version 来源）；
  其余任何子命令/参数（含 send-text、prompt、focus、read、wait、
  `--machine`）一律禁止；fake-binary 协议测试同时锁定这两个形状
  （§10.2，模式沿用 `tests/send_agent_protocol.rs`）；
- stdout 解析上限 128 KiB、agent 条目上限 64（`MAX_RAW_SNAPSHOT_ITEMS`
  256 与 snapshot 聚合 256 KiB 之内）；超限 → `completeness = Truncated`，
  保留稳定排序前缀，不静默截断；
- 每次调用 timeout 2s（与 #26 discover 一致）；失败
  `ProviderError::{Unavailable, DeadlineExceeded, InvalidResponse}`
  fail-closed，不影响其他 observer；
- 全部轮询在 provider 自有后台线程（§6.1），trait 方法在 deadline 内
  只读写有界缓存，满足既有 10ms/100ms provider 操作预算。

### 4.2 trait 方法语义

- `discover()`：环境门控（§2.2）；可用时恰好返回一个
  `ProviderInstance`：instance ID = 规范化 `HERDR_SOCKET_PATH` 的安全
  digest（区分同机多 Herdr server），version = `herdr --version` 有界
  文本，`endpoint_kind = LocalSocket`，health 按最近一次轮询结果；
- `probe()`：执行一次 `herdr agent list`，校验外层形状
  （`result.type == "agent_list"`、`result.agents` 为数组）；从静态
  template 收窄出 `InstanceContract`（§3 表 + §5.1 声明）；形状不符 →
  `InvalidResponse`，contract 层面降为 Unavailable；
- `snapshot()`：返回最近一次成功轮询的缓存，编码为 `RawSnapshot`：
  每个条目一个 `RawProviderItem`（`event_name = "agent.list.entry"`，
  payload = 该条目原始 JSON 的有界字节，`observed_at = captured_at`）；
  `complete = true`（scope 内），`watermark = None`（§4.3），
  `cursor = None`；缓存为空且从未成功 → `Unavailable`；
- `next_event()`：S1 恒返回 `Idle`（纯快照 provider）；S2 起返回
  `state_change_seq` 差分构造的 per-entry `RawEvent`（§11）；
- `begin_draining()`：停止轮询线程、清空缓存；不向 Herdr 发送任何
  控制消息（含退出）。

### 4.3 序列与 epoch

- Herdr 无全局流序列号：`sequence/watermark` 恒 `None`，不伪造
  （可观测性设计 §5.4「不能保证 sequence 应始终返回 None」）；
- `state_change_seq` 是 provider 内部优化与失效检测信号，**不是**
  `StreamSequence`，不进入 envelope；
- epoch 与 Reset 通道（**两拍机制**）：S1 是 snapshot-only provider
  （`next_event()` 恒 `Idle`，`RawSnapshot` 结构不带 epoch 字段），
  因此不经 `ProviderEventOutcome::Reset` 上报（该通道保留给 S2 差分
  事件流），而用两拍闭合信号通道：
  1. 第一拍——轮询线程检测到同一 `pane_id` 的 `state_change_seq`
     回退，或 instance version 变化，判定为 server 重启：作废缓存、
     内部 epoch 计数 +1，此后 `snapshot()` 返回
     `ProviderError::Unavailable`，命中 runtime 既有「snapshot `Err` →
     `ProviderRuntimeStatus::Reconciling`」分支；
  2. 第二拍——下一轮 `probe()` 将内部 epoch 计数编入 contract
     revision，`provider_epoch`（= digest(instance digest, revision)）
     随之变化产生新 `StreamEpoch`，Reconciling 以新 snapshot 收敛；
- provider 重启（Lens 重启）天然产生新 epoch，无需持久化。

## 5. Adapter 设计

`src/agent/herdr.rs` 以 `CodeAgentAdapter` 注册，observer
`herdr/cli-snapshot`，复用 `hook_json.rs` 有界 JSON 选择性读取器。

### 5.1 InstanceContractTemplate（静态声明）

- `subjects`：仅映射表（§5.2）覆盖的 SubjectNamespace；
- `acquisition`：`{AggregatedSnapshot}`；
- `capabilities`：按 §3 表逐 domain 声明
  `support/authority/provenance/reason`，其中 Activity 的
  `max_authority = Observational`、provenance =
  `AggregatedScreenInference`；Presence 的 authority = `Authoritative`
  且 reason 注明 scope 限于该 server 的 agent panes；
- `snapshot_semantics`：scope = 本机单 Herdr server 的 agent panes +
  已映射 subjects + Presence/Session/Activity 三个 domain；
  complete 语义 = 「该 scope 的全量列表」；
- `stream_semantics`：无 sequence、无原生事件流；S2 起声明
  差分事件为 best-effort Upsert；
- `requires_instrumentation = false`（Herdr 侧 integration 由 Herdr
  自己安装管理，Lens 不安装、不校验、不修改）；
- `stability = VersionedExperimental`（reason：0.9.x，JSON 形状无
  版本承诺）。

### 5.2 SubjectNamespace 映射（声明式，保守）

| Herdr `agent` | SubjectNamespace | 可合并的直连 observer |
| --- | --- | --- |
| `claude` | `anthropic/claude-code` | `anthropic/claude-code-hook` |
| `codex` | `openai/codex` | `openai/codex-hook` |
| `opencode` | `opencode/opencode` | `opencode/plugin` |

- 映射表是 adapter 内显式常量，逐项要求「Herdr 采集的 native ID 与
  直连 Hook 的 native ID 属于同一产品同一 identity 体系」的验证记录
  （claude：两侧均为 Claude Code session UUID；codex：均为 Codex
  session id；opencode：均为 OpenCode session id）；
- **AuthorityId 复用硬契约**：可映射条目的 AuthorityId 必须与对应
  Hook adapter 的 `authority()` 输出**逐字节相等**（claude 条目 ≡
  `ClaudeHookAdapter::authority()`，codex/opencode 同理）。现有 Hook 侧
  AuthorityId 是静态常量摘要（如
  `stable_hash(b"claude-session-authority", [namespace, b"session_id"])`），
  与机器/install 无关；实现时须将三个构造提取为共享常量/函数供两侧
  复用，禁止 Herdr adapter 自行从 socket path、`source` 字段或 install
  位置派生——任何自派生值都会使 `session_key` 不等、合并静默失败成
  两行（§10.1 契约测试锁定）；
- 未映射的 `agent` 值（cursor/kimi/droid/…共 18 种中的其余）→ 该条目
  **只产生 unattributed presence**，不创建 session、不猜测 namespace；
  后续新增映射必须附带验证记录并走设计修订；
- `agent_session.kind != "id"`（如 path 类标识）→ 不作为 identity 证据。

### 5.3 身份合并证明链

以 Claude Code 为例：

```text
Herdr entry:  agent="claude", agent_session={kind:"id", value:UUID_X}
  ──映射──▶ SubjectNamespace = anthropic/claude-code
  ──authority──▶ AuthorityId = ClaudeHookAdapter::authority() 的字节复用
              （静态常量摘要，与机器/install/来源无关；
               source="herdr:claude" 仅作诊断，不参与派生）
  ──IdentityKeyer.session_key──▶ SessionKey_H

Lens hook:   session_id = UUID_X
  ──同一 SubjectNamespace + 同一 AuthorityId + 同一 native UUID──▶
  SessionKey_H（与直连证据完全一致）
```

- 合并仅依赖三者同时可证明；`pane_id`/`cwd`/`terminal_title`/
  `workspace_id` **绝不参与**合并（可观测性设计 §5.1）；
- `--machine` 远端数据不采集：静态 AuthorityId 不区分机器，本机与远端
  出现同 UUID 条目时会被无条件合并，身份歧义在现模型内无法表达；未来
  若采集，必须先扩展 authority 模型（远端专属 authority 构造或显式
  拒绝跨 scope 合并），列为 S3 前置条件（§12）；
- `cwd` 只经 `IdentityKeyer.workspace_hint()` 生成 keyed hint；raw
  cwd 在 adapter 有界内存中即弃，不进入 `AgentObservation`。

### 5.4 decode 规则（条目 → AgentObservation facts）

每个条目至多产生 4 条 facts，共用一个 `EventId`（由
`IdentityKeyer.event_id` 以 `pane_id + state_change_seq + captured_at`
的有界 composite 构造）：

1. `Presence::Seen`（PresenceRef = instance digest + pane_id）；
2. 可映射 identity 时：`Session` upsert（SessionRef，DiscoveredMidSession
   语义由 reducer 决定）；
3. `Activity::Set(working→Working | idle→Idle | blocked→WaitingPermission)`
   @ Observational + lease（`valid_until = captured_at + 轮询间隔 × 2`）；
   `unknown`/`done` 不产生 Activity op（缺失不是清除）；
4. workspace 匹配当前选择目录时的 `WorkspaceHint` 一致性校验事实。

不读取/不保留：`terminal_title`、`terminal_title_stripped`、`focused`、
`foreground_cwd`（与 `cwd` 二选一，取 `cwd`）、raw 路径字符串。

## 6. Runtime 接入

### 6.1 轮询模型

- provider 构造时启动一个后台轮询线程：cadence 默认 5s（profiling 前
  起点可调 2–10s），单次 = spawn `herdr agent list`（2s timeout）→
  有界解析 → 写入有界缓存（`Mutex<Option<LatestList>>`，容量即 §4.1
  上限）→ 丢弃旧缓存整体替换；
- 连续 2 次失败 → health 降级 Degraded 并在缓存上标记 stale；恢复后
  下一次成功即刷新（reducer 侧按 §3 Freshness 规则处理）；
- 线程退出条件：`begin_draining()` 或 AgentRuntime shutdown；无其他
  副作用；
- 该模型不改 `AgentRuntime` 的调度：runtime 仍按既有 round-robin 调
  `snapshot()`/`next_event()`，30s 重 probe contract；§4.3 的两拍
  Reset 通道也只复用 runtime 既有分支。但 S1 **并非零核心改动**：§6.3
  的降级仲裁需要对 `AgentState` 的 `arbitrate_activity` 及其 trace
  构造做受限扩展（既有 Authoritative 路径行为逐字节不变），交付物有
  三：① Observational 降级 pass；② winner trace 的 competing 留痕
  （现状 `applied_trace` 的 `competing` 恒为空，需扩展为收集落选的
  未过期候选）；③ Observational 版 conflict trace（现状
  `conflict_trace` 的 `authority` 硬编码 `Authoritative`，直接复用会
  错误标注冲突方）。这是本设计显式声明的核心改动面。

### 6.2 注册

`src/agent/bootstrap.rs` 将 `HerdrSnapshotProvider` + adapter 注册进
production registry。**这修订可观测性设计决策日志第 22 条**：原决策具名
三个 adapter（Codex/Claude/OpenCode），而生产 registry 实际已注册四个
（`src/agent/mod.rs` 含 TraeX，测试卡点文档亦按四个表述）——修订时先
补齐 TraeX 的具名，再改为五个（新增 `herdr/cli-snapshot`），fake/default
decoder 禁令不变；同步更新该文档 §11 与
`docs/testing/code-agent-observability-test-gates.md` 的 registry 清单。

### 6.3 与 Hook 证据的仲裁

- Activity：现有 `arbitrate_activity`（`src/agent/state.rs`）是
  **Authoritative-only**：先过滤只留 Authoritative 候选，为空即回退
  `Unknown` + `Suppressed`；`EqualAuthorityConflict` 只会在多个
  Authoritative 候选值不一致时出现，`observed_at` 只在值一致时选
  winner。而 Hook 侧 Activity 的 contract 声明就是 Authoritative
  （claude/codex/opencode 三个直连 adapter 均如此），Herdr 是
  Observational——两侧**永远不同级**，现有代码下不存在「Hook vs
  Herdr 同级冲突」这一比较。因此 S1 对 `arbitrate_activity` 增加一条
  降级规则（既有 Authoritative 路径行为逐字节不变）：
  1. 存在未过期 Authoritative 候选 → 胜出规则完全不变：Hook 证据
     胜出，Herdr 候选不参与比较；落选的未过期 Herdr 候选进入 winner
     trace 的 `competing` 留痕（交付物②：现状 `applied_trace` 的
     `competing` 恒为空，非冲突场景无任何承载，需扩展收集）；多个
     Authoritative 候选值不一致仍回退 `Unknown` + conflict trace
     （行为不变）；
  2. 无未过期 Authoritative 候选（盲区 2 场景：Hook 缺失或 lease
     过期）→ 对未过期 Observational 候选执行降级 pass：值一致 →
     胜出，`DecisionTrace.authority` 如实记录 `Observational`、
     provenance 记录 `AggregatedScreenInference`，绝不声称
     Authoritative；是否新增 `DecisionDisposition::Degraded` 变体在
     实现期决定，但 trace 必须能区分「降级胜出」与「Authoritative
     胜出」；
  3. 多个 Observational 候选值不一致 → 回退 `Unknown` +
     **Observational 版 conflict trace**（交付物③：直接复用
     `conflict_trace` 会把 `authority` 硬编码为 `Authoritative`，
     错误标注冲突方的证据级别），绝不按 observer 名称定胜负
     （S1 单实例下不会触发，规则为多实例/未来 provider 预留）；
  4. Herdr 候选自身 lease 过期 → 不参与任何 pass，走既有 Stale 回退。
- Lifecycle：Herdr 无 lifecycle 证据，不参与该 domain 仲裁；Hook 的
  SessionEnd/Stop 不受影响；
- Presence：Herdr 在自己 scope 内 authoritative；其 tombstone 只移除
  Herdr 自己的 presence 候选，Hook 侧 presence（若有）不受影响。

## 7. UI 增量

Agents 视图：

- Observers 列出现 `herdr/cli-snapshot`（display name `Herdr`）；
- 由 Herdr 快照建立/刷新的 session 行：Coverage 显示 snapshot
  completeness、captured_at、轮询 gap；Explain 显示 winning observer、
  provenance（screen inference）、lease 到期；
- 未映射 subject 的 agent pane 显示在既有「Unattributed agent
  presence」区域，带 workspace 匹配过滤与 freshness；不进入
  known_count/live_count；
- known_count/live_count/visible_count/completeness 语义不变。

## 8. 错误状态

| 情况 | 行为 |
| --- | --- |
| 非 Herdr 环境（无 env/二进制） | `discover()` 返回空列表；无轮询线程副作用；UI 无痕迹 |
| `herdr agent list` 超时/非零退出 | 该轮放弃，缓存保持上次值并标 stale；连续 2 次后 health Degraded |
| JSON 形状漂移（字段缺失/类型变化） | 单条目丢弃 + diagnostic；外层形状不符 → probe/snapshot `InvalidResponse`，instance 降 Unavailable，待 30s 重 probe |
| 条目数/字节超限 | `completeness = Truncated`，UI 显示 Partial |
| Herdr server 重启（seq 回退/version 变化） | 缓存作废、`snapshot()` 报 `Unavailable` → Reconciling；下轮 probe 新 epoch → 新 snapshot 恢复（§4.3 两拍） |
| Lens 自身 pane 出现在列表 | 正常处理：Lens 进程不是 agent，不会出现在 `agent list` |

## 9. 安全与隐私边界

- 只读边界：provider 方法集无任何写/控制方法；argv allowlist 只含
  `agent list`；协议测试锁死；
- 与 #26 出站通道的关系：共享子进程执行纪律与 env 解析，但类型、
  注册与能力完全分离；`AgentTargetProvider` 的 `send_draft/focus_pane`
  不可从 observation provider 到达；
- 隐私：`cwd`/`foreground_cwd`/`terminal_title`/`terminal_id`/
  native UUID 均为 transient——只在 adapter 有界内存中存在，
  `AgentObservation`/IPC/metadata/日志/DecisionTrace 只含 keyed digest
  与安全枚举；byte canary 扫描覆盖新增 fixture（§10）；
- 权限：不读写 Herdr 配置、不安装/卸载 Herdr integration
  （`requires_instrumentation = false`）；Herdr socket 权限由 Herdr
  自己的 current-user 边界保证，Lens 不触碰 socket 本身（只经 CLI）；
- 三方共写 `~/.claude/settings.json`（Lens/Herdr/botmux hooks 并存）
  已是现状；本设计不新增写入，但 §10 增加「Herdr 已安装时
  `latte-lens hooks setup`」的共存回归测试。

## 10. 测试计划

遵循 `docs/testing/code-agent-observability-test-gates.md` 分层：

1. **Adapter UT（`src/agent/herdr.rs` 内联，合成 fixture）**：
   - §2.1 真实形状 fixture 的 decode：全状态枚举映射、`kind != "id"`
     降级、未映射 agent → presence-only、字段缺失容错、条目截断；
   - privacy canary：fixture 注入 raw cwd/native UUID/terminal_title
     标记串，断言不出现于 observation/metadata projection；
   - 映射表契约：每个 SubjectNamespace 映射的合并/不合并双向
     （同 UUID 合并、异 UUID 不合并、无 identity 不建 session）；
   - AuthorityId 字节相等契约：三个映射 subject 的 Herdr 侧
     AuthorityId 与对应 Hook adapter `authority()` 输出逐字节相等
     （防止自派生回归，§5.2）。
2. **Provider UT（`src/agent/herdr_provider.rs`，fake `herdr` 脚本）**：
   - argv 协议：`HERDR_BIN_PATH` 指向固定行为脚本，锁死「只调用
     `agent list` 与 `--version` 两个允许形状、argv 逐参数、绝不
     出现 send-text/prompt/focus/read/wait/`--machine`」；
   - 轮询线程：cadence、2s 超时、连续失败降级、draining 停止；
   - 缓存语义：snapshot 返回最近成功值、Unavailable 路径、Truncated。
3. **Contract/Registry**：production registry 含五 observer 的注册
   断言；`InstanceContractTemplate` 不越权（Activity 上限 Observational、
   Lifecycle Unsupported）；probe 收窄不扩大。
4. **Reducer 集成（合成 envelope，`tests/` 注入 fake provider）**：
   - Herdr snapshot + 同 UUID Hook 事件 → 单 session 行、双 observer、
     Coverage 正确；
   - 仲裁矩阵（对齐 §6.3 新规则）：Hook Authoritative 未过期 vs Herdr
     任意值 → Hook 胜、Herdr 留 competing；Hook 缺失/过期 + Herdr
     working/idle/blocked → 降级胜出且 trace 如实标记 Observational
     （**盲区 2 验收用例**）；两侧均过期 → Unknown/Stale；合成第二个
     Observational provider 值不一致 → Unknown + conflict trace；
     snapshot 刷新 → Freshness `Stale` 回到 `Current`
     （`ObservationFreshness` 仅 Unknown/Current/Stale，无 "Revived"
     术语）；
   - Complete snapshot 缺失条目 → presence tombstone，lifecycle 不变；
   - seq 回退 → `snapshot()` 报 `Unavailable` → `ProviderStatus::
     Reconciling` → 下轮 probe 新 epoch → 恢复（§4.3 两拍机制）；
   - 冷启动盲区回归：Lens 启动时 fake provider 已有两条 session，无
     任何 Hook 事件 → Agents 视图即显示两条（盲区 1/3 的验收用例）。
5. **E2E**：headless journey（fake `herdr` 脚本 + metadata 断言）进
   `make ci`；PTY journey 后置到 S1 落地后按需补充。
6. **Canary（optional，默认 ignored，不进 CI）**：
   `make herdr-snapshot-canary` 对本机已安装 Herdr 执行一次真实
   `herdr agent list` 并校验 decode 形状；不读写用户配置。

## 11. 分阶段实施

| 阶段 | 内容 | 验收 |
| --- | --- | --- |
| **S1（本期）** | adapter + provider（轮询线程/缓存、两拍 Reset）+ `arbitrate_activity` 降级仲裁与 trace 三项交付物（§6.1/§6.3）+ registry 注册（修订决策 22）+ §10.1–10.5 测试 + 文档同步 | 冷启动盲区用例、盲区 2 降级仲裁用例、合并用例、仲裁矩阵、argv 协议全绿；`make ci` 通过 |
| **S2** | `state_change_seq` 差分 → per-entry `RawEvent` 增量流；cadence 可配置；（可选）`agent wait` long-poll 线程 | 事件路径 contract 测试；无变化轮次零 envelope |
| **S3（候选）** | Herdr 公开 stream API 接入；`terminal_title` 作为 Presentation 证据（隐私评审前置）；远端 `--machine` scope（observer-isolated AuthorityId） | 另立设计修订 |

提交切分建议（S1）：

1. `feat(agent): herdr/cli-snapshot adapter 与 SubjectNamespace 映射`；
2. `feat(agent): HerdrSnapshotProvider 轮询与缓存`；
3. `feat(agent): Activity Observational 降级仲裁与 trace 扩展`；
4. `feat(agent): production registry 注册与决策 22 修订（含文档同步）`；
5. `test(agent): fake herdr 协议/reducer 仲裁/冷启动回归`。

## 12. 风险与开放问题

- **JSON 形状漂移**：Herdr 0.9.x 迭代快、无 schema 承诺——靠 probe
  校验 + fail-closed + canary 缓解；字段级容错（丢条目不丢整轮）；
- **`state_change_seq` 语义未文档化**：作为私有推导信号使用（变更检测/
  重启检测），不承担正确性关键路径；语义变化最坏影响 = 多一次 reconcile；
- **`agent_status` 的屏幕检测准确性**：blocked 的语义（等权限 vs 等输入）
  未由 Herdr 承诺——映射为 `WaitingPermission` 仅是 Observational 候选，
  DecisionTrace 保留 provenance；若实测歧义大，S2 可改为只区分
  busy/not-busy 两档；
- **`terminal_title` 隐私**：标题常由用户 prompt 派生，等同弱 prompt
  泄漏面——首期不采集，S3 需独立隐私评审；
- **轮询开销**：5s 一次子进程 spawn 的 CPU/时延待 profiling；必要时
  S2 换 `agent wait` 或加 idle 退避；
- **开放问题**：cadence 与 lease 倍数标定；`done` 是否可作为 lifecycle
  的 Observational Ended 候选（首期否）；Windows 上 Herdr env/CLI 行为
  未验证（无 cfg 门控，靠 runtime 探测自然 Unavailable）；同机多 Herdr
  server（socket 不同）时 discover 是否需要枚举多个 instance（首期仅
  `HERDR_SOCKET_PATH` 指向的一个）；静态 AuthorityId 的跨机器语义
  （`--machine` 采集的前置条件，见 §5.3）。
