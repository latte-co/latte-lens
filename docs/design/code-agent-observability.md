# Latte Lens Code Agent 可观测性设计：Session Metadata、Live Hooks 与 Read-only Providers

## 1. 核心决策

首期方案不保存完整 Hook 事件历史，也不要求常驻 lensd。

首期方案支持两类采集入口、三条数据路径：

~~~text
Instrumented emitter，Lens 未启动：
vendor hook/plugin
→ latte-lens hook + registered CodeAgentAdapter
→ AgentObservation allowlist
→ 有界更新 Session Metadata Beacon
→ exit 0

Instrumented emitter，Lens 已启动：
vendor hook/plugin
→ latte-lens hook + registered CodeAgentAdapter
→ AgentObservation allowlist
→ 仅当前用户可访问的本地 IPC
→ Lens bounded AgentRuntime
→ in-memory reducer
→ Agents TUI
→ 合并、限频更新 Session Metadata Beacon

Read-only provider，Lens 已启动：
Terminal multiplexer / vendor server / local control plane
→ ObservationProvider
→ InstanceContract negotiation
→ SnapshotEnvelope + EventEnvelope
→ Lens bounded AgentRuntime
→ in-memory reducer
→ Agents TUI
~~~

首期方案的磁盘状态是一个有损、覆盖写、固定上限的 session index，不是 event log、transcript、审计库或历史数据库。

首期方案不包含完整事件的离线 spool、replay、checkpoint、sealed-segment compression，以及跨重启的 changes/artifacts 恢复。这些能力纳入历史增强方案，不是首期实现的前置条件。

Read-only provider 只在 Lens 运行期间工作。Lens 不会为了补全状态而启动外部 runtime、vendor server 或 Code Agent，不 attach 已有进程，也不会把 provider 的外部状态持久化成完整历史。Provider 断开、事件出现 Gap 或 epoch 变化时，Lens 通过有界 snapshot reconcile 恢复当前视图；无法恢复时显式降级为 Partial。

### 1.1 SessionStart 不是唯一入口

只要某个 Hook payload 能提供稳定 SessionRef，任何事件都可以创建或更新 session：

- SessionStart；
- UserPromptSubmit；
- permission；
- PreToolUse/PostToolUse；
- Stop/StopFailure；
- SubagentStart/SubagentStop；
- vendor session status/error/diff；
- 其他经过版本映射确认包含稳定 session identity 的 Hook。

SessionStart 只提供更强的启动证据、启动时间和初始元数据。Lens 在 session 中途首次观察到其他事件时，必须创建 DiscoveredMidSession session，而不是等待永远不会再次发生的 SessionStart。

如果某类 Hook 没有稳定 session identity，它不能凭 cwd、最近一行 UI 或“当前唯一 session”进行猜测绑定；它只能成为 live diagnostic 或被安全忽略。

### 1.2 首期范围

- Lens 启动后实时观察 session、agent/subagent、turn、permission、tool activity。
- Lens 未启动时保存有限 session/agent 摘要。
- Lens 晚于 Agent 启动时，从 metadata index 发现此前被 Hook 观察过的 session。
- 任意带 SessionRef 的 Hook 都能建立 session。
- Lens 在线后把 MetadataOnly session 升级为 LiveObserved。
- Lens 在线后可以从 read-only provider 获取有界 current snapshot，并用增量 event 保持收敛。
- Live changes、文档和链接 artifact 具有 provenance、scope 和 confidence。
- Hook 快速、fail-open、本地、metadata-only，不控制 Agent。
- Codex、Claude Code、OpenCode、TraeX 通过同一规范化协议扩展，但采集模式和证据等级可以不同。

### 1.3 首期非目标

- Lens 未启动时不保存 turn/tool/change/artifact 事件。
- Lens 重启后不恢复之前的完整 live timeline。
- 不提供当前系统中所有运行中 Code Agent 的完整清单。
- 不把 terminal pane、OS process 或 provider instance 数量直接称为 session 数量。
- 已经运行但 Lens 启动后没有再触发 Hook 的 session，不会被实时确认。
- 仅当可用 provider 的 snapshot 覆盖该 session 时，上述 Hook 缺口才可能被补充；缺少稳定 SessionRef 时仍不得猜测合并。
- MetadataOnly 不能证明进程仍在运行。
- 不把 Stop 当成 SessionEnd。
- 不把 workspace Git diff 冒充 session 的精确 change。
- 不持久化 prompt、response、tool input/output、command、diff、正文、transcript 或 token。
- 不安装常驻 daemon。
- 不 attach、注入或接管已经运行的 Agent。

## 2. 产品语义

### 2.1 UI 展示的是 Observed Sessions

首期方案的计数定义为：

> selected workspace 中，在 metadata retention 内曾被 Hook 观察，或自 Lens 本次启动后被 live Hook/provider envelope 观察到的 sessions。

它不是操作系统进程总数，也不是 vendor 的 authoritative running-session inventory。

UI 必须显示：

- known_count：metadata/live 已知 session 数；
- live_count：Lens 本次运行期间收到 live Hook/provider evidence 且尚未 stale 的 session 数；
- visible_count：当前过滤后可见数；
- completeness：MetadataOnly、LivePartial 或 LiveObserved；
- observing_since：当前 Lens 最早开始 live observation 的时间；detail 中按 observer/instance 分别显示；
- truncated：metadata/agent 上限是否触发。

不得把 known_count 标成“正在运行的总 session 数”。

### 2.2 Session 的五个独立维度

~~~rust
enum SessionDiscovery {
    StartConfirmed {
        started_at: Timestamp,
    },
    DiscoveredMidSession {
        first_observed_at: Timestamp,
    },
}

enum ObservationMode {
    MetadataOnly,
    LiveObserved,
}

enum SessionLifecycle {
    Open,
    Ended,
    Failed,
    Unknown,
}

enum ActivityState {
    Working,
    WaitingPermission,
    Idle,
    Unknown,
}

enum ObservationFreshness {
    Current,
    Stale,
    Unknown,
}
~~~

五个维度不能折叠：

- Discovery 描述是否看到了真实启动证据。
- ObservationMode 描述信息来自 metadata 还是 Lens 本次运行期间的 live Hook/provider observation。
- Lifecycle 只描述 session 是否仍然开放、已经结束或失败。
- Activity 描述最近可靠证据中的 Working、WaitingPermission 或 Idle。
- Freshness 描述当前 activity/lifecycle evidence 是否仍在有效期内。

first_observed_at 不能冒充真实 started_at。MetadataOnly 不能直接标记 Working；没有 terminal 只表示“结束未知”。TTL 到期只能把 Freshness 变为 Stale，并让过期 activity 回退为 Unknown，不能把 Lifecycle 改成 Ended、Failed 或其他终态。

### 2.3 历史覆盖

每个 session row 都带 coverage：

~~~rust
struct ObservationCoverage {
    metadata_first_observed_at: Timestamp,
    live_observing_since: Option<Timestamp>,
    start_event_seen: bool,
    terminal_event_seen: bool,
    observers: BoundedVec<ObserverCoverage, 8>,
    observers_truncated: bool,
    agents_truncated: bool,
    dropped_live_events: u64,
}

struct ObserverCoverage {
    observer: ObserverId,
    instance: ObserverInstanceId,
    observing_since: Timestamp,
    snapshot_completeness: Option<SnapshotCompleteness>,
    last_reconciled_at: Option<Timestamp>,
    stream_gap_count: u64,
    dropped_events: u64,
}
~~~

UI 示例：

~~~text
Codex · Metadata only
首次观察：10:12
真实启动：未知
最后观察：10:31
Live 观察：尚未开始

Codex · Live · Partial
首次观察：10:12
Live 观察自：10:35
最近收敛：10:36 · snapshot partial
之前的 turn/change/artifact 不可用
~~~

## 3. 可行性证据

### 3.1 Codex Hook 验证

在 Codex CLI 0.144.3 上，普通 codex exec 使用临时、显式 trusted 项目配置，无 app-server，真实触发：

~~~text
SessionStart → UserPromptSubmit → Stop
~~~

安全化记录分别为 206 B、226 B、214 B，包含一致的 hashed session/turn identity；没有 prompt、response、tool input/output、transcript path、assistant body、raw cwd、authorization 或 token。

验证结果还表明：

- project config trust 与 hook-definition trust 是两个不同边界；
- 64 KiB 输入上限和 metadata allowlist 可行；
- Hook 失败可以 exit 0 且 stdout/stderr 为空；
- SessionStart、UserPromptSubmit、Stop 都能提供 session 关联证据。

现有证据尚未覆盖：

- Codex binary 的完整 turn/tool/permission/subagent 触发矩阵；
- Codex exact file change；
- 跨平台、跨 Codex 版本的真实触发验证；
- Codex Hook 的显式、可逆安装器；
- 任意 Hook 都必然带完整 SessionRef。

这些证据与后续 adapter UT、CLI/receiver E2E 共同支撑当前 `openai/codex-hook` 集成，但不能把 SessionStart canary 推断为完整 Codex 兼容矩阵，也不能推断其他 Code Agent 已经集成。

### 3.2 当前 Latte Lens 接入 seam

现有代码可复用以下边界：

- src/runtime.rs:177-257,288-301,354-449：generation、stale rejection、background worker/completion。
- src/app.rs:43-57,549-570,2642-2660：row identity、registry 更新、App::poll_background reducer seam。
- src/search.rs:95-138：现有 unbounded channel 不可复用。
- src/preview.rs:187-250：registry/factory/priority 结构。
- src/repo_graph.rs:19-57,252-273：RepoId、RepoPath、nested repository ownership。
- Makefile:19-56：make ci、coverage、E2E、package gate。

AgentRuntime 必须使用独立 bounded channel；src/ui.rs 只渲染 view model，不打开 metadata 文件、不监听 IPC、不调用 adapter。

### 3.3 聚合型终端运行时对照约束

聚合型终端运行时通常拥有 terminal pane、PTY、前台进程和屏幕缓冲区；Latte Lens 只观察用户已经运行的 Code Agent。因此这里只吸收可迁移的接口约束，不把 terminal ownership 当成 Lens 能力。

可迁移约束如下：

| 聚合型运行时中的常见实现证据 | 对 Latte Lens core 的约束 |
|---|---|
| 状态、session identity 与 presentation metadata 使用不同事件 | session identity、lifecycle、presentation 必须是不同证据域；拿到 session ID 不自动获得状态 authority |
| 同一 source 使用 seq 丢弃乱序报告 | EventId 去重之外还需要 observer stream sequence；两者不能互相替代 |
| authority clear、agent release、metadata clear 都是显式操作 | 字段缺失只表示“没有新证据”，不得解释为清除或结束 |
| presentation metadata 可以带 TTL | 临时状态需要显式有效期；到期只让证据失效，不合成 SessionEnd |
| lifecycle hooks 只有覆盖完整时才成为状态 authority | InstanceContract/CapabilityClaim 必须同时表达 support 和 authority，不能只有布尔支持状态 |
| Codex 集成只报告 session identity，状态来自 screen detection | Hook 机制相似不代表能力相同；每个 adapter 按真实覆盖声明能力 |
| list snapshot 与 event subscription 在内部 sequence 上接续并用 entity snapshot 补偿 | Provider 接入必须 snapshot-first，event 只负责增量；Gap/Reset 后强制 reconcile |
| 聚合记录同时区分 agent product 与 session evidence source | 被观察产品的 identity namespace 与 observer/provider identity 必须分开 |
| screen-derived 与 hook-derived authority 按 entity 动态变化 | 能力协商必须是 per-instance/per-observation，不能只按 adapter/version 静态判断 |
| explain 接口暴露 matched rule、fallback、skip reason 与 detector version | Lens 需要 bounded DecisionTrace，让 Partial/Unknown 可解释而不是只显示状态结果 |

以下聚合型终端机制不能照搬：

- Lens 不拥有 pane/process/screen，不能用 pane id、前台进程或屏幕检测补全缺失的 Hook 状态。
- 终端运行时可能为会话恢复保存 raw native session reference；Lens 不负责 resume，只保存 install-scoped HMAC key。
- 终端运行时可能使用 Agent enum、source allowlist 和 source-specific resume match；Lens core 保持 ObserverId registry 与 SubjectNamespace value object，不增加 vendor 分支。
- 通用 shell emitter 可能依赖临时文件、脚本运行时和较长 socket timeout；这不能满足 Lens Hook 的低开销目标。
- pane 生命周期、pane revision 和 process exit 不是 vendor SessionEnd；它们只能成为 presence/activity evidence。
- 可更新的 detection manifest 可以调整检测规则，但 Lens 不加载远程可执行 adapter。未来若支持 manifest，只允许声明式、签名或随版本发布的 bounded mapping。

聚合型运行时还说明“相同 Hook 格式”不等于“同一个 Agent”：TraeX 即使兼容部分 Codex Hook 形状，仍需要独立 SubjectNamespace 和能力声明。Lens 的复用单位是 decoder/helper，不是把不同产品折叠为同一个 subject。

聚合型终端运行时可以通过可选的 read-only bridge 作为 ObservationProvider，而不是 Lens 的依赖或真相源。建议能力映射如下：

| 聚合型运行时 evidence | Lens capability |
|---|---|
| agent/pane presence | Confirmed presence；不自动建立 session |
| Working/Blocked/Idle | 默认 Observational；仅当该 pane 的完整 lifecycle hook authority 生效时可在 Activity domain 声明 Authoritative |
| agent_session | Partial session identity；字段缺失时不得用 pane/cwd 猜测 SessionRef |
| pane exit/Done | presence terminal evidence；不是 vendor SessionEnd |
| cwd/foreground_cwd | Observed workspace locator，经 WorkspaceHint 安全映射后使用 |
| subagent/change/artifact | Unsupported，除非未来 provider API 提供独立、可验证的证据 |

因此聚合型 provider 适合验证 snapshot/event 收敛、observer/subject 分离和动态 authority，但不改变 OpenCode/Codex/Claude/TraeX 各自 adapter 的真实能力声明。

## 4. 首期端到端架构

~~~mermaid
flowchart LR
    A["Codex / Claude / OpenCode / TraeX hooks"]
    H["Trusted Hook / Plugin"]
    E["Future Adapter / Emitter"]
    P["Terminal Runtime / Vendor Server / Local Control Plane"]
    O["Read-only ObservationProvider"]
    C["InstanceContract"]
    S["SnapshotEnvelope + EventEnvelope"]
    M["Session Metadata Index"]
    I["Owner-only Local IPC"]
    R["Bounded AgentRuntime"]
    D["Idempotent In-memory Reducer"]
    U["Agents TUI"]

    A --> H --> E
    E -->|"Lens absent / IPC failed"| M
    E -->|"Lens live"| I --> R --> D --> U
    P --> O --> C --> S --> R
    D -->|"合并写 metadata"| M
    M -->|"startup bootstrap"| D
~~~

两类采集入口最终都进入同一个 ObservationEnvelope、AdapterRegistry validation 和 AgentState；它们不共享外部连接实现。Hook path 追求短 deadline 和 fail-open，Provider path 在 Lens background runtime 内允许有界连接、snapshot、subscription 与 reconcile。

### 4.1 Lens 未启动

通用 Hook CLI 与未来 adapter 只做：

1. 读取最多 64 KiB + 1 byte。
2. 通过 CodeAgentAdapter 解码为 AgentObservation。
3. ObservationDispatcher 尝试 live publish，得到 Unavailable。
4. Dispatcher 通过 project_metadata 产生 delta；若能建立 SessionRef，通过 SessionMetadataStore.merge 更新对应 Session Metadata Beacon。
5. exit 0。

不写 turn/tool/change/artifact event，不创建 event spool。

### 4.2 Lens 已启动

通用 Hook CLI 调用已注册 adapter，并优先尝试 local IPC：

1. 通过 CodeAgentAdapter 构造 AgentObservation。
2. Hook 根据事件的精确 WorkspaceHint 读取有界 receiver registry，向所有选择同一目录的 Lens endpoint fan-out 同一个 EventId。
3. 每个 Lens 独立校验 current-user peer、协议、contract 与 workspace membership 后返回 ACK。
4. 所有匹配 receiver 均 ACK_ACCEPTED 时不再由 Hook 写 metadata；各 Lens 负责合并、限频更新 metadata。
5. 没有 receiver、部分投递失败、NACK、timeout 或 version mismatch 时，降级为只更新 Session Metadata Beacon；其他 Lens 通过周期 metadata refresh 至少获得摘要状态。
6. 所有路径 exit 0，stdout/stderr 为空。

完整 live event 只存在于 Lens 有界内存状态。首期方案不保存 timeline。

Read-only provider 不走 Hook IPC：

1. AgentRuntime 在后台发现并 probe provider instance。
2. AdapterRegistry 根据 probe 结果建立 InstanceContract。
3. Provider 使用 subscribe-before-snapshot、cursor snapshot 或等价的无缝握手，得到 SnapshotEnvelope 与后续 EventEnvelope。
4. Snapshot 先建立 current state，buffered event 再按 watermark/sequence 应用。
5. Gap、Reset、epoch 变化或 provider reconnect 使 instance 进入 Reconciling；完成新 snapshot 前不得声称 Complete。
6. Provider 不可用时保留其他 observer 的证据，并把对应 coverage 降级为 Partial/Unavailable。

### 4.3 Lens 启动

顺序固定：

1. 解析 selected workspace。
2. 枚举匹配的 workspace metadata，并有界加载 session beacons。
3. 构造 MetadataOnly rows。
4. 启动独立 AgentRuntime 与 bounded channel。
5. 为当前 Lens 实例生成 receiver ID，绑定独立、仅当前用户可访问的 IPC endpoint。
6. 在 ephemeral receiver registry 发布带心跳和 TTL 的 workspace membership lease。
7. 在 background runtime 中发现 provider instances，协商 InstanceContract，并启动各自的 snapshot/subscription handshake。
8. 发布 receiver manifest、generation 与 heartbeat；provider 则分别发布自己的 instance epoch/readiness。
9. 后续 Hook event 或 provider envelope 将对应 row 升级为 LiveObserved。

IPC endpoint 在 metadata bootstrap 完成前不得对 Hook 返回 ACK_ACCEPTED。

### 4.4 Lens 正常退出

1. endpoint 进入 draining，不再接受新 event。
2. 新 Hook 降级为更新 metadata。
3. Lens 有界 drain 已接收的 queue。
4. 合并并写最后一批 session metadata。
5. 取消 provider subscriptions 并关闭 read-only connections；不向 provider 或 Agent 写控制消息。
6. 删除 receiver manifest 和 endpoint。
7. 停止 receiver heartbeat。

Lens crash 后 receiver lease 过期，后续 Hook 忽略 stale manifest，并在没有成功接收方时自动降级为更新 metadata。首期方案不恢复 crash 前尚未写入 metadata 的 live-only events。

## 5. 核心领域模型与身份

### 5.1 SessionRef 与 AgentRef

~~~rust
struct SessionRef {
    key: SessionKey,
    workspace: WorkspaceHint,
}

struct AgentRef {
    key: AgentKey,
    parent: Option<AgentKey>,
    kind: Option<AgentKind>,
}

struct PresenceRef {
    stable_id: StableDigest,
    subject_hint: Option<SubjectNamespace>,
    workspace: Option<WorkspaceHint>,
}

struct SessionKey {
    subject: SubjectNamespace,
    install_id: InstallId,
    authority_id: AuthorityId,
    stable_id: StableDigest,
}

struct AgentKey {
    session: SessionKey,
    stable_id: StableDigest,
}

struct SubjectDescriptor {
    namespace: SubjectNamespace,
    display_name: BoundedString,
}

struct ObserverDescriptor {
    id: ObserverId,
    display_name: BoundedString,
    adapter_version: BoundedString,
}
~~~

SubjectNamespace 表示被观察产品的 native identity namespace，例如 `openai/codex`、`anthropic/claude` 或 `bytedance/traex`。ObserverId 表示提供证据的入口，例如 `openai/codex-hook`、`opencode/server` 或 `terminal-runtime/socket`。两者都是 adapter 声明的稳定、有界命名空间字符串，不是 core enum；最长 64 字节，只允许小写 ASCII 字母、数字、`.`、`_`、`-` 和 `/`，第三方使用 `organization/name` 命名空间。Core 不按名称做 source-specific 分支。

Raw native session/agent ID 只允许短暂存在于 adapter decoder 的有界内存中。Adapter 在构造 SessionRef/AgentRef 前，必须通过 core 提供的 IdentityKeyer 按 `(SubjectNamespace, InstallId, AuthorityId, native_id)` 计算 install-scoped HMAC stable digest；AgentObservation、IPC、metadata 和日志都只携带 SessionKey/AgentKey。

两个 observer 只有在都能证明 native ID 属于同一 SubjectNamespace 和 AuthorityId 时，才能得到相同 SessionKey。例如某个聚合 provider 明确暴露 Codex native session reference 时，可以与 Codex Hook 合并；只有 pane id、cwd 或 agent label 时必须保持为独立 presence evidence。不同 subject、install 或 authority 的 ID 不能碰撞，也不能通过最近活动、cwd 或“当前唯一 session”猜测合并。

AuthorityId 表示 native identity 的真实作用域，例如同一 vendor install/account/control-plane，而不是 observer connection。Provider 无法证明与 direct Hook 属于同一 authority 时，IdentityKeyer 必须生成 observer-isolated AuthorityId；宁可显示两个 Partial rows，也不能误合并不同用户、容器、远端主机或沙箱中的同名 native ID。

EventId、PresenceRef、SessionKey、AgentKey、TurnKey、StableDigest、SubjectNamespace、ObserverId 和 WorkspaceHint 的字段对 adapter 保持私有，只能通过 identity 模块的校验构造器获取。

### 5.2 AgentObservation

~~~rust
struct AgentObservation {
    observed_at: Timestamp,
    valid_until: Option<Timestamp>,
    presence: Option<PresenceRef>,
    session: Option<SessionRef>,
    agent: Option<AgentRef>,
    turn: Option<TurnKey>,
    workspace: Option<WorkspaceHint>,
    kind: ObservationKind,
    evidence: EvidenceClaim,
}

enum ObservationKind {
    Presence(PresenceOp),
    Session(SessionOp),
    Lifecycle(LifecycleOp),
    Activity(ActivityOp),
    Turn(TurnOp),
    Permission(PermissionOp),
    Tool(ToolOp),
    Agent(AgentOp),
    Change(ChangeObservation),
    Artifact(ArtifactObservation),
    Presentation(PresentationOp),
    Diagnostic(SanitizedDiagnostic),
}

enum EvidenceDomain {
    Presence,
    Session,
    Lifecycle,
    Activity,
    Turn,
    Permission,
    Tool,
    AgentTopology,
    Change,
    Artifact,
    Presentation,
    Diagnostic,
}

enum LifecycleOp {
    Set(ReportedSessionLifecycle),
    Clear,
}

enum ReportedSessionLifecycle {
    Open,
    Ended,
    Failed,
}

enum ActivityOp {
    Set(ReportedActivityState),
    Clear,
}

enum ReportedActivityState {
    Working,
    WaitingPermission,
    Idle,
}

enum PresenceOp {
    Seen,
    Released,
}

struct EvidenceClaim {
    support: CapabilitySupport,
    authority: EvidenceAuthority,
    provenance: EvidenceProvenance,
}

impl AgentObservation {
    fn validate_shape(&self) -> Result<(), ObservationError>;
    fn domain(&self) -> EvidenceDomain;
}
~~~

AgentObservation 是 adapter 与 core 之间唯一的规范化事实。它不包含 raw payload、transport 字段、observer identity 或 source-specific enum；observer、instance、epoch、EventId 和 sequence 属于外层 ObservationEnvelope。字段必须有界，并能在 4 KiB event frame 中完整编码。没有稳定 SessionRef 的 observation 只能用于 Diagnostic 或独立 Presence evidence，不得创建 session。

EvidenceClaim 描述这条 observation 的实际证据等级。它必须受对应 InstanceContract 的上限约束：adapter/provider 可以从 Authoritative 降级为 Observational，不能反向越权。相同 adapter 的不同 instance、同一 instance 的不同 session，甚至同一 session 的不同 domain 都允许具有不同 authority。

valid_until 只允许用于 Presence::Seen、Activity、Presentation，或 contract 明确声明为 lease-backed 的 Lifecycle::Set(Open)。证据到期后 reducer 将 Freshness 标为 Stale、移除该 observer 的过期候选并重新仲裁，不得据此生成 SessionEnd、AgentEnd 或成功/失败结论。

PresenceOp、SessionOp、LifecycleOp、ActivityOp、AgentOp 和 PresentationOp 中影响既有状态的清除、释放、结束操作必须是显式 variant。Optional 字段缺失永远不表示清除。

AgentObservation::validate_shape 在 dispatch 和 receive 边界上必须通过以下结构检查：

- AgentKey.session 和 TurnKey.session 与 observation.session 一致；
- session.workspace 与 observation.workspace 同时存在时必须一致；
- 缺少 SessionRef 时，kind 只能是 Diagnostic 或 Presence；Presence 必须带 observer-scoped PresenceRef；
- PresenceRef 不得被转换成 SessionKey，也不得参与 known_count/live_count；
- valid_until 必须晚于 observed_at，且只用于允许过期的 evidence；
- EvidenceClaim 不超过当前 InstanceContract 对该 domain/subject 的 support 与 authority；
- 所有字符串、列表和编码尺寸在硬上限内。

结构检查之后，AdapterRegistry 还必须按 ObserverId、ObserverInstanceId、instance epoch 与 InstanceContract 做语义检查。Unsupported/Unknown domain、越权 Clear/Delete、以及 observational evidence 试图声明 authoritative replacement 都返回 UnsupportedCapability，不进入 metadata、IPC 或 reducer。

### 5.3 Turn identity

Turn 不能依赖“当前 turn”：

~~~rust
struct TurnKey {
    session: SessionKey,
    authority_id: AuthorityId,
    stable_id: StableDigest,
}
~~~

有稳定 turn ID 的事件更新对应 TurnKey。没有稳定 turn ID 的 Stop/failure 只能产生 UnattributedTurnEvidence，影响 session activity/completeness，但不能关闭任意 turn。

### 5.4 SnapshotEnvelope 与 EventEnvelope

~~~rust
struct StreamRef {
    observer: ObserverId,
    instance: ObserverInstanceId,
    epoch: StreamEpoch,
}

enum ObservationEnvelope {
    Snapshot(SnapshotEnvelope),
    Event(EventEnvelope),
}

struct SnapshotEnvelope {
    stream: StreamRef,
    snapshot_id: SnapshotId,
    chunk_index: u16,
    final_chunk: bool,
    captured_at: Timestamp,
    scope: SnapshotScope,
    completeness: SnapshotCompleteness,
    watermark: Option<StreamSequence>,
    observations: BoundedVec<AgentObservation, 64>,
}

enum SnapshotCompleteness {
    Complete,
    Partial,
    Truncated,
}

struct EventEnvelope {
    stream: StreamRef,
    event_id: EventId,
    sequence: Option<StreamSequence>,
    op: StreamOp,
}

enum StreamOp {
    Upsert(BoundedVec<AgentObservation, 8>),
    Delete {
        entity: ObservedEntityKey,
        domains: BoundedSet<EvidenceDomain, 8>,
    },
    Reset,
    Gap { expected: Option<StreamSequence>, received: Option<StreamSequence> },
}

enum ObservedEntityKey {
    Presence(PresenceRef),
    Session(SessionKey),
    Agent(AgentKey),
    Turn(TurnKey),
    Artifact(ArtifactKey),
}
~~~

ObserverInstanceId 是统一的逻辑观测实例标识，不等于进程或网络连接。Read-only provider 用它区分 endpoint/server instance；Hook emitter 用 install/authority 的安全 digest 构造逻辑 instance，因此不要求常驻 provider process。

EventId 必须在同一 observer delivery 重试时保持稳定，且不从 observed_at 或本地随机数单独生成。Sequence 只在同一 `(ObserverId, ObserverInstanceId, StreamEpoch)` 内单调；provider/emitter restart 必须切换 epoch 或发送 Reset，不能让新序号与旧 epoch 混用。Adapter 如果不能保证 sequence，应始终返回 None，而不是用 wall clock 伪造顺序。

同一个 native event 可以规范化为最多八条 AgentObservation facts，并在一个 EventEnvelope 中共享 EventId/sequence 原子应用。例如 permission event 可以分别产生 Permission 与 Activity=WaitingPermission，而不是让一个 EvidenceDomain 隐式修改另一个 domain。整个 EventEnvelope 仍必须满足 4 KiB 上限。

Hook EventEnvelope 中的 facts 必须属于同一 SessionKey/WorkspaceHint，或全部是不落盘的 Diagnostic/Presence，便于在短 deadline 内合并为一个 metadata delta。Provider event 可以携带不同 entity facts，但仍受八项/4 KiB 上限。

Delete 必须列出受影响 domains，并且只删除当前 observer/instance 在这些 domains 的候选 evidence。`Delete(Presence)`、pane close 或 process exit 不能升级为 `Lifecycle::Ended`；SessionEnd 仍需要独立、具有相应 authority 的 Lifecycle fact。

SnapshotScope 明确 snapshot 覆盖的 workspace membership、subject namespaces、entity kinds 和 evidence domains。同一 snapshot 的 chunks 必须具有相同 snapshot_id、stream、captured_at、scope、completeness 和 watermark，按 chunk_index 从 0 连续到 final_chunk；缺块、乱序、重复或超出 aggregate cap 都视为 Gap。只有完整收到 final_chunk 的 Complete snapshot 才能为其 scope 内缺失的实体生成该 observer 的 tombstone；Partial/Truncated snapshot 只能 upsert，不能删除。Tombstone 只移除该 observer 的候选证据，其他 observer 的有效证据仍参与仲裁。

Gap、Reset、epoch 变化或无法衔接 watermark 时，instance 进入 Reconciling 并请求新 snapshot。新 snapshot 完成前可以继续展示最后有效值，但 Freshness/coverage 必须标 Partial，不得声称 current inventory complete。

## 6. Session Metadata Index

### 6.1 统一 state root

解析优先级：

1. LATTE_LENS_STATE_DIR，必须是绝对安全路径。
2. LATTE_HOME/lens/state，LATTE_HOME 必须是绝对路径。
3. user-home/.latte/lens/state。

默认结构：

~~~text
~/.latte/lens/state/
  session-index/
    installs/<install-id>/
      install.meta
      workspaces/<workspace-key>/
        workspace.meta
        sessions/
          <session-key>.meta
          <session-key>.lock
~~~

Unix 目录 0700、文件 0600；Windows current-user-only ACL。拒绝 symlink/reparse、FIFO/socket/device 和 network share。Hook 不写被观察 workspace。

### 6.2 Workspace metadata

~~~rust
struct WorkspaceMetadata {
    workspace_key: WorkspaceKey,
    fingerprint_key_id: KeyId,
    last_observed_at: Timestamp,
    checksum: Checksum,
}
~~~

`workspace_key` 是 Lens/Hook 启动时所选 canonical 目录的 install-keyed digest；不保存 raw cwd 或绝对路径，不上溯 Git root，也不建立父子目录 membership。

### 6.3 SessionMetadata

~~~rust
struct SessionMetadata {
    subject: SubjectNamespace,
    install_id: InstallId,
    workspace_key: WorkspaceKey,
    session_key: SessionKey,
    observers: BoundedVec<ObserverId, 4>,
    observers_truncated: bool,

    discovery: SessionDiscovery,
    first_observed_at: Timestamp,
    last_observed_at: Timestamp,
    lifecycle_hint: SessionLifecycleHint,
    last_activity_hint: ActivityStateHint,
    last_event_kind: ObservationKindTag,

    main_agent: Option<AgentSummary>,
    known_agents: BoundedVec<AgentSummary, 32>,
    agents_truncated: bool,

    start_observed: bool,
    terminal: Option<TerminalSummary>,
    generation: u64,
    checksum: Checksum,
}

struct AgentSummary {
    key: AgentKey,
    parent: Option<AgentKey>,
    kind: Option<AgentKind>,
    lifecycle_hint: AgentLifecycleHint,
    first_observed_at: Timestamp,
    last_observed_at: Timestamp,
}
~~~

单个 session metadata 编码后的硬上限为 4 KiB。32 是逻辑 agent 上限；如果紧凑编码在更早时达到 4 KiB，必须提前截断。超过任一上限时保留能够完整编码的稳定排序前缀，并设置 agents_truncated=true；UI 显示“至少 N / Partial”。

### 6.4 Metadata 不包含什么

永久禁止进入 workspace.meta、session.meta、lock/journal、fixture 和日志：

- prompt/response/message body；
- tool input/output；
- command/shell body；
- transcript path/body；
- source/diff/file content；
- raw cwd、完整环境变量；
- auth header、token、URL query/fragment；
- native session/agent ID；
- changes/artifacts 列表。

### 6.5 Upsert 与写入限频

持久化入口只接收由 project_metadata 产生的 delta：

~~~rust
struct SessionMetadataDelta {
    workspace: WorkspaceHint,
    session: SessionKey,
    observer: ObserverId,
    observed_at: Timestamp,
    discovery: Option<SessionDiscovery>,
    lifecycle_hint: Option<SessionLifecycleHint>,
    activity_hint: Option<ActivityStateHint>,
    event_kind: ObservationKindTag,
    agent: Option<AgentSummaryDelta>,
    terminal: Option<TerminalSummary>,
    write_class: MetadataWriteClass,
}

enum MetadataWriteClass {
    Structural,
    Activity,
}
~~~

事件分为两类。

结构事件立即尝试 upsert：

- 首次观察 session；
- SessionStart/SessionEnd；
- failure terminal；
- SubagentStart/SubagentStop；
- workspace/session identity correction；
- agent parent/type 变化。

高频活动事件限频：

- UserPromptSubmit；
- PreToolUse/PostToolUse；
- permission；
- Stop；
- 普通 activity。

Profiling 前默认：

~~~text
metadata_write_interval = 2s / session
per-session lock deadline = 2ms
~~~

SessionMetadataStore 的 filesystem 实现先读取 metadata 文件的 mtime；文件存在、两秒内更新且 write_class=Activity 时，直接跳过磁盘写。Lens 在线时由 AgentState 在内存中合并，并按同一限频规则提交 PersistMetadata request。

### 6.6 Monotonic merge

持有 per-session short lock 后，最多读取 4 KiB 旧记录并执行：

- first_observed_at 取最小可靠值；
- last_observed_at 取最大可靠值；
- Unknown 不能覆盖 Known；
- StartConfirmed 不能被 DiscoveredMidSession 降级；
- terminal evidence 不因较早 activity 消失；
- terminal 之后的更晚可靠 event 产生 Revival evidence，保留原 terminal；
- AgentSummary 按 AgentKey 归并；
- ObserverId 只做稳定去重并受四项上限约束；metadata 不持久化完整 DecisionTrace；
- 同 authority 同 generation 冲突产生 Partial diagnostic，不能按 observer 名称硬编码优先级；
- older generation 不能覆盖 newer generation。

写入使用同目录、no-follow 的独占 temp 文件，经过 checksum 编码和逐字节回读验证后 atomic replace。Metadata 是提示性索引，首期方案默认不对每次更新执行 fsync；损坏记录会被隔离或忽略，并标记为 Partial，不能阻塞 Agent。

锁竞争、解析失败或写入失败时 Hook exit 0。Metadata update 可以丢，不能为了完整性等待或重试无界时间。

### 6.7 容量与清理

Profiling 前默认：

| 项目 | 上限 |
|---|---:|
| Session metadata | 4 KiB |
| Agents/session | 32 |
| Sessions/workspace | 256 |
| Sessions/install | 4096 |
| Ended retention | 24 小时 |
| Non-terminal retention | 7 天 |

Lens 启动后在 background runtime 中有界清理。Hook 不做目录 GC。

WorkspaceMetadata 只持久化精确 canonical workspace fingerprint，不保存 raw cwd 或祖先目录。写入侧在 install 范围内同时执行 256 workspace、4096 session 和每 workspace 256 session 的容量闸门；清理删除最后一个 session 后同步回收 workspace metadata 与容量。

已有 session 在容量满时仍可刷新；新 session 创建不得扫描超过 cap + 1 个 entry。无法安全创建时跳过 metadata，并在 Lens 在线时发送 MetadataCapacityExceeded diagnostic。

## 7. Live IPC

### 7.1 ObservationFrame 与 ACK

~~~rust
struct ObservationFrame {
    protocol_major: u16,
    protocol_minor: u16,
    workspace_hints: BoundedVec<WorkspaceHint, 1>,
    event: EventEnvelope,
}

enum LiveAck {
    Accepted {
        receiver_generation: u64,
    },
    NotMember,
    Busy,
    VersionMismatch,
    Invalid,
}
~~~

ObservationFrame 是 Hook emitter 的单事件 framing，硬上限为 4 KiB，只允许 StreamOp::Upsert/Reset；Hook 不通过 IPC 发送 snapshot。未知 major version 返回 VersionMismatch；未知但可安全忽略的 minor 字段不进入 AgentObservation。ACK 只证明 Lens bounded queue 已接受 event，不证明持久化。

Publisher 未收到 ACK 时，Lens 可能已经接收 observation 但响应丢失；降级写入的 metadata 与 live observation 必须通过稳定 SessionKey/EventId 幂等合并。

### 7.2 Receiver registry 与多 Lens fan-out

每个 Lens 进程创建一个独立 live receiver，不设置 install 级唯一 owner：

- Linux/macOS：仅当前用户可访问的 nonblocking Unix socket。
- Windows：current-user-only named pipe。

Endpoint 位于 ephemeral runtime root，不进入 durable state：

| 平台 | Runtime endpoint |
|---|---|
| Linux | XDG_RUNTIME_DIR/latte-lens；不可用时使用仅当前用户可访问的 TMPDIR |
| macOS | 仅当前用户可访问的 TMPDIR/latte-lens-uid |
| Windows | current-user named pipe |

Ephemeral registry 按 install 分区。每个 receiver manifest 只包含 receiver ID、endpoint、receiver generation、协议版本和 selected-workspace keyed hints，不包含 event payload 或 raw path。Manifest 使用当前用户私有权限、三秒心跳和十五秒 TTL；正常退出主动删除，crash 后由 TTL 淘汰。

Hook 最多枚举 16 个新鲜 manifest，按共同总 deadline 向所有 workspace 匹配的 endpoint 投递相同 EventId。所有健康且队列可接收的 Lens 都得到 live event；部分 receiver 失败时结果为 Partial，Hook 同时写 metadata fallback。每个 Lens 每两秒有界刷新 metadata，因 Busy、crash race 或 completion backpressure 错过 live detail 的实例仍能收敛到 MetadataOnly 摘要，但不能伪装成完整 live timeline。

### 7.3 Publish deadline

Profiling 前起点：

~~~text
connect deadline = 2ms
send + ACK total deadline = 5ms
message 硬上限 = 4 KiB
Lens ingress queue = 256
~~~

任一 live deadline 超时都降级为更新 metadata，并 exit 0。Live connect/send/ACK 共用五毫秒 deadline；metadata fallback 在 live 返回后获得独立的两毫秒 lock budget，不能复用已经过期的 live deadline。不得等待 Lens 启动、重试连接、拉起进程或打印 warning。

### 7.4 Workspace 解析与 membership

Lens 和 Hook CLI 必须调用同一个 workspace resolver：将各自的启动目录 canonicalize 后直接生成 install-keyed WorkspaceHint。它不接受显式父 root，不上溯最近 Git worktree，也不生成祖先 hints。

Receiver manifest 只保存 Lens 所选精确目录的 keyed hint；Hook frame 只携带事件自身的单个 workspace hint。两者完全相等且 event contract 校验通过时才 ACK。Lens 在仓库根目录启动而 Codex 在子目录启动时，两者属于不同 workspace；子目录 Hook 只写自己的 metadata fallback，不会出现在父目录 Lens 中。

同一 workspace 下多个 Code Agent 不按 cwd 合并。SessionKey 仍由 SubjectNamespace、authority 和稳定 native session identity 决定；同一产品的多个 session、不同产品以及各自 subagent topology 都保持独立。缺少稳定 SessionRef 的 Hook 不得因为“该 workspace 只有一个候选”而猜测归属。

### 7.5 Backpressure

AgentRuntime 使用 bounded queue。Queue 满时：

- Lens 返回 Busy；
- Lens 在能够安全解析 SessionKey 时增加该 session 的 dropped_live_events，否则增加 instance/global dropped counter；
- Hook 降级为更新 metadata；
- tool/change/artifact live detail 可能丢失；
- session row completeness 变 LivePartial；
- Code Agent 不被阻塞。

首期方案不为 dropped event 建 durable Gap。Hook ingress drop 没有 snapshot recovery 时使对应 coverage 保持 LivePartial；Dropped counter 只在当前 Lens 进程内可见，重启后 metadata 仍然只能表示 MetadataOnly。Provider event drop 则必须在内存中产生 Gap 并触发 snapshot reconcile，不能继续声称 Current/Complete。

### 7.6 Provider transport 与 reconcile

ObservationProvider 运行在 Lens background runtime 中，不受 Hook 的 5 ms deadline，但必须满足独立的 connect/read/reconcile budget、取消和 byte/item caps。默认单 observation 仍为 4 KiB；每个 SnapshotEnvelope 最多 64 项/64 KiB，完整 snapshot 聚合最多 256 项/256 KiB。超过 aggregate cap 时 final chunk 的 completeness 标为 Truncated，不能静默截断后声明 Complete。

每个 provider instance 必须选择一种无缝握手：

1. subscribe 后记录 cursor，buffer event，再读取与 cursor 对齐的 snapshot；或
2. 获取带 watermark 的 snapshot，再从 watermark 订阅；或
3. provider 原生提供等价的 atomic snapshot+stream handshake。

如果 provider 只能分别执行 list 与 subscribe，Adapter 必须在序列边界前后复核 snapshot；无法证明无缝时，InstanceContract 将 snapshot/event consistency 标为 Partial，并周期性有界 reconcile。

Provider reconnect、epoch 变化、sequence gap、queue drop、Reset 或 decode/version failure 都进入 Reconciling。旧 snapshot 只作为 stale view 保留；新 Complete snapshot 才能在声明 scope 内应用 tombstone。Provider 不得调用 agent.send、focus、start、resume 或任何写/控制 API。

## 8. Reducer 与 Agents UI

### 8.1 Any-event session upsert

Reducer 对每条包含 SessionRef 的 Upsert（来自 Hook event 或 provider snapshot/event）先执行 ensure_session：

~~~text
session 已存在
→ monotonic merge session/agent evidence

session 不存在 + SessionStart
→ StartConfirmed + LiveObserved

session 不存在 + 其他 event
→ DiscoveredMidSession + LiveObserved
~~~

然后才应用具体 SessionOp/LifecycleOp/ActivityOp/TurnOp/ToolOp/AgentOp。Presence evidence 没有稳定 SessionRef 时进入独立、低置信度的 unattributed presence view，不增加 known_count/live_count。

### 8.2 Metadata bootstrap

Lens 从 metadata 创建的 row：

~~~text
observation_mode = MetadataOnly
lifecycle = Ended（仅有 terminal evidence时）或 Unknown
activity = Unknown
freshness = Unknown
completeness = Partial
changes/artifacts/turns = unavailable
~~~

不能从 last_observed_at 推断 Working/Open。收到新的 live Hook/provider evidence 后：

~~~text
observation_mode = LiveObserved
live_observing_since = 首个匹配 Hook endpoint/provider instance 开始观测的时间
lifecycle/activity/freshness = 根据当前可靠 evidence 分别更新
~~~

### 8.3 Activity、turn、session 分离

- UserPromptSubmit：Activity=Working；有稳定 TurnKey 时开始 turn。
- Permission：Activity=WaitingPermission。
- PreToolUse：Activity=Working；有稳定 key 时关联 turn/tool。
- PostToolUse：结束对应 tool，不自动结束 session。
- Stop：只结束同一 TurnKey；无 key 时记录 unattributed evidence。
- SessionStart 或可靠 lease：Lifecycle=Open。
- SessionEnd：Lifecycle=Ended；failure terminal：Lifecycle=Failed。
- TTL：Freshness=Stale，过期 Activity 回退 Unknown；Lifecycle 不自动结束。
- stale 后有更晚 evidence：Freshness=Current，并记录 Revived coverage。

Codex 没有 SessionEnd，因此 Stop 后 session 仍可能存在。

### 8.4 Evidence authority 与乱序

Reducer 对 live envelope 使用固定的接受顺序：

1. generation 不匹配时拒绝，不触碰当前 workspace state；
2. ObserverId/ObserverInstanceId/epoch 与当前 InstanceContract 不匹配时返回 WrongEpoch/UnknownInstance；
3. EventId 已见时返回 Duplicate；
4. sequence 违反当前 StreamRef 单调约束时返回 StaleSequence；
5. Gap/Reset 或无法衔接 watermark 时标记 Reconciling，并阻止 destructive delete；
6. valid_until 已经过期时返回 Expired；
7. 在对应 evidence domain 内应用 support、authority、provenance 与 freshness 仲裁；
8. Complete snapshot 在 SnapshotScope 内为缺失的 observer evidence 生成 tombstone；Partial/Truncated snapshot 不删除；
9. 最后执行 metadata projection。

Authoritative 表示 observer 可以在该 domain、instance、subject 和声明 scope 内执行 Set/Clear/Delete；Observational 只增加“曾观察到”证据、coverage 和 timeline，不得覆盖 authoritative semantic state。例如 session-only Hook 可以创建 session row，但不能把 Activity 标成 Working；tool Hook 可以显示工具活动，但在 activity coverage 不完整时不能永久占有 Working。

同一 domain 有多个候选时，先丢弃 expired/suppressed evidence，再按 authority 等级比较；同 authority 内使用明确 sequence/watermark，而不是 observer 名称决定新旧。两个同等级 authoritative observer 给出不可调和的当前值时，effective value 回退为 Unknown/Partial，并记录 conflict DecisionTrace，不能静默 last-write-wins。

sequence 只解决乱序，EventId 只解决重试重复。两者都不用于跨 session 排序，也不能从 wall clock、随机数或接收顺序推导。

### 8.5 Agents 视图

Agents 视图包含以下字段：

| 字段 | 说明 |
|---|---|
| Subject | SubjectDescriptor.display_name |
| Observers | 当前提供 evidence 的 ObserverDescriptor 列表 |
| Session | stable short key |
| Discovery | Start confirmed / Mid-session |
| Mode | Metadata only / Live |
| Lifecycle | Open/Ended/Failed/Unknown |
| Activity | Working/WaitingPermission/Idle/Unknown |
| Freshness | Current/Stale/Unknown |
| Agents | known/live count + truncated |
| Last observed | metadata/live timestamp |
| Coverage | observing since、snapshot completeness、reconcile、gap、dropped |
| Evidence | identity/lifecycle/activity/tool/topology/change/artifact 的 support + authority + provenance |
| Changes | live-only count |
| Artifacts | live-only count |

没有稳定 SessionRef 的 Presence evidence 显示在独立的“Unattributed agent presence”区域，带 observer、subject hint、workspace、freshness 和 confidence；它不进入 session 列表计数，也不显示为 Running session。

进入 session detail 后显示：

- 已知 agent/subagent topology；
- Lens 本次启动后的 turn/tool timeline；
- live changes；
- live artifacts；
- metadata 与 live coverage 边界；
- 当前状态为什么成立、来自哪个 observer、是否过期或被其他 evidence 抑制。

### 8.6 Explain 与 DecisionTrace

Lens 不保存 raw screen、Hook payload 或 provider response，但必须为每个 session/domain 保留最后一个有界解释：

~~~rust
struct DecisionTrace {
    domain: EvidenceDomain,
    effective_value: BoundedValueSummary,
    winning_observer: Option<ObserverId>,
    authority: EvidenceAuthority,
    provenance: EvidenceProvenance,
    observed_at: Option<Timestamp>,
    valid_until: Option<Timestamp>,
    disposition: DecisionDisposition,
    competing: BoundedVec<CompetingEvidenceSummary, 4>,
}
~~~

DecisionDisposition 至少区分 Applied、Expired、Suppressed、StaleSequence、WrongEpoch、AwaitingSnapshot、UnsupportedCapability 和 EqualAuthorityConflict。CompetingEvidenceSummary 只包含 observer、domain、authority、freshness 与安全 reason enum，不包含 raw cwd、native ID、matched screen text、payload、diff 或 message body。

Agents detail 的 Explain 区域用于回答“为什么是 Unknown/Partial/Working”，并显示 provider version、instance epoch、snapshot completeness、last reconcile、gap/drop 和 local override/manifest version 等已经安全化的诊断信息。该能力只解释 Lens 自己的仲裁，不复制外部运行时的原始检测文本。

## 9. Live Changes 与 Artifacts

首期方案不把 changes/artifacts 写入 Session Metadata。

### 9.1 Change attribution

~~~rust
enum Confidence {
    Exact,
    Observed,
    Inferred,
}

enum AttributionScope {
    Turn(TurnKey),
    Session(SessionKey),
    Workspace,
}
~~~

- Exact：vendor authoritative patch/file operation 明确给出 producer 和 path。
- Observed：direct Hook 明确来自 producer，但没有 authoritative final diff。
- Inferred：VCS/window、Bash、shell、未知 MCP 或 workspace Git diff。

Workspace scope 不能冒充 session attribution。缺失稳定 TurnKey 时保持 Session 或 Workspace scope，不能绑定“当前 turn”。

### 9.2 Artifact

Live artifact 只保存到内存 view state：

- stable/sanitized key；
- kind/title/media type；
- 安全 workspace-relative path 或 opaque digest；
- producer SessionKey/AgentKey；
- 可选 TurnKey；
- observer/provenance/confidence。

URL 必须移除 userinfo、query、fragment，限制 scheme，并对可能含 secret 的 path segment 做 redaction/hash。不得为了提取链接保存 assistant message 或 tool output。

Lens 退出后，artifact/change 随内存状态一同消失。如需保留历史，使用后续的历史增强方案。

## 10. 核心接口

### 10.1 依赖方向

核心层只在需要替换的边界使用 trait：adapter、read-only provider、identity、metadata store 和 Hook live transport。Reducer、metadata projection、instance registry 和 runtime orchestration 使用具体类型，不为暂无第二实现的内部组件预设 trait。

~~~text
bounded Hook/provider input
→ CodeAgentAdapter
→ AgentObservation
→ ObservationEnvelope
→ AdapterRegistry validation
→ InstanceContract validation
→ ValidatedEnvelope

Hook emitter:
Validated EventEnvelope
→ ObservationDispatcher
   ├─ LiveObservationPublisher
   └─ SessionMetadataStore fallback

LiveObservationReceiver / ObservationProvider / SessionMetadataStore
→ AgentRuntimeCompletion
→ App::poll_background
→ AgentState
→ AgentViewState
→ UI
~~~

约束：

- adapter 只做 bounded decode、identity keying 和 normalize，不连接 provider、不写 metadata、不发 IPC、不修改 App state；
- provider 只在 background runtime 中发现/probe/read/subscribe，不修改 Agent、外部 runtime 或 vendor 配置；
- AdapterRegistry 与 InstanceRegistry 根据 observer/instance/epoch/contract 校验 evidence domain 与 authority，只有 ValidatedEnvelope 能进入 persistence/transport/reducer；
- transport 只处理 framing、peer、deadline 和 backpressure，不理解 session 语义；
- metadata store 只持久化 bounded SessionMetadata，不接收 raw payload；
- AgentState 是无 I/O 的 deterministic reducer；
- UI 只读取 AgentViewState。

### 10.2 CodeAgentAdapter

~~~rust
trait CodeAgentAdapter: Send + Sync {
    fn descriptor(&self) -> ObserverDescriptor;
    fn contract_template(
        &self,
        observer_version: Option<&str>,
    ) -> InstanceContractTemplate;
    fn decode(
        &self,
        input: AdapterInput<'_>,
        identity: &dyn IdentityKeyer,
    ) -> Result<DecodeOutcome, AdapterError>;
}

struct AdapterInput<'a> {
    delivery: AdapterDelivery,
    event_name: &'a str,
    observer_version: Option<&'a str>,
    observed_at: Timestamp,
    payload: &'a [u8],
}

enum AdapterDelivery {
    HookEvent,
    ProviderSnapshotItem,
    ProviderEvent,
}

enum DecodeOutcome {
    Observations(BoundedVec<AgentObservation, 8>),
    Ignore(IgnoreReason),
}

struct AdapterRegistry {
    adapters: BTreeMap<ObserverId, Arc<dyn CodeAgentAdapter>>,
}

impl AdapterRegistry {
    fn register(
        &mut self,
        adapter: Arc<dyn CodeAgentAdapter>,
    ) -> Result<(), DuplicateObserverId>;

    fn resolve(
        &self,
        observer: &ObserverId,
    ) -> Option<&dyn CodeAgentAdapter>;

    fn validate_envelope(
        &self,
        envelope: ObservationEnvelope,
        contract: &InstanceContract,
    ) -> Result<ValidatedEnvelope, ObservationError>;
}

struct ValidatedEnvelope {
    envelope: ObservationEnvelope,
    contract_revision: ContractRevision,
}
~~~

AdapterInput 由调用方限制在 64 KiB 内。AdapterError 和 IgnoreReason 必须是有界、可安全记录的枚举，不得附带 payload、cwd、native ID 或 source error body。Core 不接收 raw JSON 或 source-specific enum。

AdapterRegistry 是具体容器，不是新的扩展 trait。重复 ObserverId 必须拒绝；resolve 失败时返回 UnknownObserver，不使用默认 adapter 猜测解码。validate_envelope 先执行 envelope/observation shape validation，再用当前 InstanceContract 校验 subject、scope、domain、support、authority、epoch 和 destructive operation。新增 Code Agent 或聚合入口只需实现 CodeAgentAdapter，并按需要提供 ObservationProvider；不得修改 AgentState、AgentRuntime 或 UI 的 observer-specific 分支。当前生产 registry 只注册 `openai/codex-hook`、`anthropic/claude-code-hook` 与 `opencode/plugin`，不包含默认 decoder 或测试 adapter。

### 10.3 ObservationProvider

~~~rust
trait ObservationProvider: Send {
    fn observer_id(&self) -> ObserverId;

    fn discover(
        &mut self,
        selector: &WorkspaceSelector,
        limits: ProviderDiscoveryLimits,
        deadline: Instant,
    ) -> Result<Vec<ProviderInstance>, ProviderError>;

    fn probe(
        &mut self,
        instance: &ProviderInstance,
        deadline: Instant,
    ) -> Result<InstanceContract, ProviderError>;

    fn snapshot(
        &mut self,
        instance: &ProviderInstance,
        cursor: Option<&ProviderCursor>,
        limits: SnapshotLimits,
        deadline: Instant,
    ) -> Result<RawSnapshot, ProviderError>;

    fn next_event(
        &mut self,
        instance: &ProviderInstance,
        deadline: Instant,
    ) -> ProviderEventOutcome;

    fn begin_draining(&mut self);
}
~~~

ObservationProvider 是采集 SPI，CodeAgentAdapter 是规范化 SPI。Provider 返回的 RawSnapshot/RawEvent 仍需经对应 adapter bounded decode 后才能形成 ObservationEnvelope。ProviderInstance 只保存 observer、opaque instance digest、版本、endpoint kind 和安全 health summary；不得把 socket path、raw cwd、token 或 native ID 暴露给 App/UI。

Provider 必须声明 read-only method allowlist。聚合型终端 provider 首期只允许经过设计确认的 list/get/subscribe 等读取方法；OpenCode/Codex provider 也只允许经过设计确认的 list/read/status/event 方法。任何 start/send/focus/resume/config mutation 均不属于 ObservationProvider。

Runtime 对 discover/probe/snapshot/reconcile 使用 100ms 总预算和单操作 10ms deadline，active instance 按 round-robin 每轮只 poll 一个，并每 30 秒重新 probe contract。Provider 必须协作遵守 deadline；shutdown 时 runtime 调用 begin_draining。Contract revision 更新会先发出 ContractUpdated completion，再进入 snapshot reconcile；AgentState 按 observer+instance source 重新校验 lifecycle/activity/agent/turn/artifact/change/presence evidence，撤销新 contract 无法证明的部分，同时保留其他 source 的证据。

### 10.4 IdentityKeyer

~~~rust
trait IdentityKeyer: Send + Sync {
    fn event_id(
        &self,
        observer: &ObserverId,
        instance: &ObserverInstanceId,
        epoch: &StreamEpoch,
        native_or_composite_id: SensitiveId<'_>,
    ) -> Result<EventId, IdentityError>;

    fn session_key(
        &self,
        subject: &SubjectNamespace,
        authority: &AuthorityId,
        native_id: SensitiveId<'_>,
    ) -> Result<SessionKey, IdentityError>;

    fn presence_ref(
        &self,
        observer: &ObserverId,
        instance: &ObserverInstanceId,
        native_presence_id: SensitiveId<'_>,
        subject_hint: Option<&SubjectNamespace>,
        workspace: Option<WorkspaceHint>,
    ) -> Result<PresenceRef, IdentityError>;

    fn agent_key(
        &self,
        session: &SessionKey,
        native_id: SensitiveId<'_>,
    ) -> Result<AgentKey, IdentityError>;

    fn turn_key(
        &self,
        session: &SessionKey,
        authority: &AuthorityId,
        native_id: SensitiveId<'_>,
    ) -> Result<TurnKey, IdentityError>;

    fn workspace_hint(
        &self,
        locator: SensitiveWorkspaceLocator<'_>,
    ) -> Result<WorkspaceHint, IdentityError>;
}
~~~

SensitiveId 和 SensitiveWorkspaceLocator 不实现 Debug、Display 或 Serialize。IdentityKeyer 是 raw identity 进入 core 后的唯一处理边界；成功返回后，adapter 必须丢弃 raw 值。event_id 的 composite input 只能由稳定 native delivery ID、event name 和 provider cursor 等有界字段组成，不得包含 prompt、response、wall-clock-only timestamp 或 tool body。

### 10.5 SessionMetadataStore

~~~rust
struct MetadataLoadLimits {
    max_workspaces: usize,
    max_sessions: usize,
    max_total_bytes: usize,
}

struct MetadataSnapshot {
    workspaces: Vec<WorkspaceMetadata>,
    sessions: Vec<SessionMetadata>,
    truncated: bool,
    corrupt_records_ignored: u32,
}

trait SessionMetadataStore: Send + Sync {
    fn load_workspace(
        &self,
        selector: &WorkspaceSelector,
        limits: MetadataLoadLimits,
    ) -> Result<MetadataSnapshot, MetadataError>;

    fn merge(
        &self,
        delta: &SessionMetadataDelta,
        deadline: Instant,
    ) -> MetadataWriteOutcome;

    fn prune(
        &self,
        policy: &RetentionPolicy,
        budget: MaintenanceBudget,
    ) -> Result<PruneSummary, MetadataError>;
}
~~~

load_workspace 必须在 MetadataLoadLimits 内返回稳定排序的 snapshot 与 truncated 标记。merge 不重试，返回 Updated、SkippedFresh、Contended、CapacityReached 或 Failed。prune 只能由 Lens background runtime 调用。MetadataError 只携带分类和安全摘要，不包含 raw path 或文件内容。

SessionMetadataDelta 由纯函数 project_metadata(&ValidatedEnvelope) 生成，adapter 不得直接构造 metadata 记录。

~~~rust
fn project_metadata(
    envelope: &ValidatedEnvelope,
) -> BoundedVec<SessionMetadataDelta, 64>;
~~~

### 10.6 Live transport 与 ObservationDispatcher

~~~rust
trait LiveObservationPublisher: Send + Sync {
    fn publish(
        &self,
        event: &EventEnvelope,
        deadline: Instant,
    ) -> PublishOutcome;
}

enum PublishOutcome {
    Accepted { receiver_generation: u64 },
    Partial { accepted: u16, attempted: u16 },
    Unavailable,
    NotMember,
    Busy,
    Incompatible,
    Rejected,
}

trait LiveObservationReceiver: Send {
    fn receive(&mut self, deadline: Instant) -> ReceiveOutcome;
    fn begin_draining(&mut self);
}

enum ReceiveOutcome {
    Event {
        receiver_generation: u64,
        event: EventEnvelope,
    },
    Idle,
    Closed,
    Rejected(TransportRejectReason),
}

enum DispatchOutcome {
    LiveAccepted { receiver_generation: u64 },
    Metadata(MetadataWriteOutcome),
    IgnoredNoSession,
    RejectedInvalid,
}

struct ObservationDispatcher<'a> {
    adapters: &'a AdapterRegistry,
    publisher: &'a dyn LiveObservationPublisher,
    metadata: &'a dyn SessionMetadataStore,
}

impl ObservationDispatcher<'_> {
    fn dispatch(
        &self,
        event: EventEnvelope,
        contract: &InstanceContract,
        deadline: Instant,
    ) -> DispatchOutcome;
}
~~~

ObservationDispatcher 是 Hook emitter 路径的具体类型，不定义 trait。Provider envelope 由 AgentRuntime 直接走同一 registry/contract validation，不经过 metadata fallback dispatcher。dispatch(event, contract, deadline) 保留单 deadline 便利入口；Hook 使用 dispatch_with_budget(event, contract, live_deadline, metadata_budget) 保证 live 与 fallback 预算独立。固定语义是：

1. 先执行 AdapterRegistry::validate_envelope；结构、epoch 或 capability 失败时返回 RejectedInvalid。
2. 尝试 LiveObservationPublisher.publish，线上 framing 只编码已验证 EventEnvelope。
3. 所有匹配 receiver Accepted 时返回 LiveAccepted，不由 emitter 写 metadata。
4. Partial 或其他非 Accepted 结果通过 project_metadata 从 ValidatedEnvelope 生成 delta；Hook event 的同-session facts 先折叠成一个 delta，再调用 SessionMetadataStore.merge。
5. 缺少 SessionRef 时不创建 metadata，只返回 IgnoredNoSession。
6. 任何 I/O 失败都是有界结果，不 panic、不重试、不改变 Code Agent 的退出状态。

Emitter 侧 validation 只是尽早拒绝错误，不能替代信任边界。LiveObservationReceiver 在 ACK_ACCEPTED 前必须按 current-user peer、install identity、ObserverId、ObserverInstanceId、epoch 和当前 InstanceContract 重新执行 envelope validation；install 不一致归类为 StateRootMismatch，其余验证失败返回 Invalid/Rejected，不能把发送方附带的 authority 当成已授权事实。

### 10.7 AgentRuntime

AgentRuntime 是具体 background runtime，不是扩展 SPI。它与 App 之间只交换有界 request/completion：

~~~rust
enum AgentRuntimeRequest {
    SelectWorkspace {
        generation: u64,
        selector: WorkspaceSelector,
    },
    RefreshProviders {
        generation: u64,
    },
    PersistMetadata {
        generation: u64,
        delta: SessionMetadataDelta,
    },
    ScheduleExpiry {
        generation: u64,
        expiry: EvidenceExpiry,
    },
}

enum AgentRuntimeCompletion {
    MetadataLoaded {
        generation: u64,
        snapshot: MetadataSnapshot,
    },
    EnvelopeReceived {
        generation: u64,
        envelope: ValidatedEnvelope,
    },
    EvidenceExpired {
        generation: u64,
        keys: BoundedVec<EvidenceExpiryKey, 64>,
    },
    ProviderStatus {
        generation: u64,
        status: ProviderRuntimeStatus,
    },
    RuntimeStatus {
        generation: u64,
        status: AgentRuntimeStatus,
    },
}

impl AgentRuntimeHandle {
    fn submit(
        &self,
        request: AgentRuntimeRequest,
    ) -> Result<(), RuntimeBackpressure>;

    fn try_next(&self) -> Option<AgentRuntimeCompletion>;

    fn begin_shutdown(&self);
}
~~~

Request 与 completion queue 都必须 bounded。submit 不阻塞 UI thread；queue 已满时立即返回 RuntimeBackpressure。AgentRuntime 为 provider connection/reconcile 和 evidence expiry 维护有界任务集合与最小堆，不引入 UI-thread timer。到期时发出 EvidenceExpired；即使没有新 Agent event，App 仍能把 Freshness/Activity 正确降级。

App::poll_background 通过 try_next 有界拉取 completion，先校验 generation，再将 envelope/expiry 交给 AgentState；旧 workspace 的 completion 不得修改当前 state。Provider queue drop 必须先产生 ProviderStatus::GapDetected，再安排 reconcile，不能只增加计数后继续运行。

### 10.8 AgentState 与 AgentViewState

~~~rust
impl AgentState {
    fn bootstrap_metadata(
        &mut self,
        generation: u64,
        snapshot: MetadataSnapshot,
    ) -> ApplyResult;

    fn apply_envelope(
        &mut self,
        generation: u64,
        envelope: ValidatedEnvelope,
    ) -> ApplyResult;

    fn expire_evidence(
        &mut self,
        generation: u64,
        keys: &[EvidenceExpiryKey],
    ) -> ApplyResult;

    fn view(&self) -> AgentViewState;
}

struct ApplyResult {
    disposition: ApplyDisposition,
    changed: bool,
    metadata_deltas: BoundedVec<SessionMetadataDelta, 64>,
    expiry_updates: BoundedVec<EvidenceExpiryUpdate, 64>,
}

enum ApplyDisposition {
    Applied,
    Duplicate,
    StaleSequence,
    UnsequencedAfterSequenced,
    Expired,
    WrongGeneration,
    WrongEpoch,
    GapDetected,
    AwaitingSnapshot,
    UnsupportedCapability,
    EqualAuthorityConflict,
}
~~~

AgentState 不读文件、不访问 IPC、不调用 adapter。apply_envelope 按 8.4 的固定顺序执行 snapshot/event reconcile、幂等、epoch/sequence、expiry 和 authority 规则，再原子应用 EventEnvelope 内的 bounded facts、执行 any-event session upsert 与 monotonic merge。expire_evidence 只移除到期候选并重新仲裁，不合成终态。App 将 ApplyResult.metadata_deltas 和 expiry_updates 作为 bounded request 发回 AgentRuntime。UI 只获取不含 raw identity 和 I/O handle 的 AgentViewState。

### 10.9 InstanceContract 与动态 capability

~~~rust
struct InstanceContract {
    observer: ObserverId,
    instance: ObserverInstanceId,
    revision: ContractRevision,
    observer_version: Option<BoundedString>,
    subjects: BoundedVec<SubjectNamespace, 32>,
    acquisition: BoundedSet<AcquisitionMode>,
    capabilities: BoundedMap<EvidenceDomain, CapabilityClaim>,
    snapshot_semantics: SnapshotSemantics,
    stream_semantics: StreamSemantics,
    requires_instrumentation: bool,
    stability: InterfaceStability,
}

struct CapabilityClaim {
    support: CapabilitySupport,
    max_authority: EvidenceAuthority,
    provenance: EvidenceProvenance,
    reason: BoundedReason,
}

enum EvidenceProvenance {
    NativeControlPlane,
    InstrumentedHook,
    AggregatedHookAuthority,
    AggregatedScreenInference,
    ProcessPresence,
    VcsInference,
}

enum EvidenceAuthority {
    Authoritative,
    Observational,
    None,
}

enum AcquisitionMode {
    HookEvent,
    NativeSnapshot,
    NativeEventStream,
    AggregatedSnapshot,
    AggregatedEventStream,
    ProcessPresence,
}
~~~

InstanceContract 由 adapter 的静态 template 与 provider/emitter 的实际 probe 共同产生；probe 只能收窄 template，不能扩大。Hook payload 或远端 provider response 中自报的 authority 不被直接信任，必须由本地 adapter/provider 代码和已验证版本映射确认。字段包括：

- presence identity/liveness；
- session_ref_events；
- agent_ref_events；
- native_session_start/session_end；
- native_turn_id 与 stable_turn_key_across_events；
- permission_events 与 tool_coverage；
- subagent_topology；
- change_evidence 与 artifact_evidence；
- snapshot scope/completeness/watermark；
- stream epoch/sequence/reset/gap；
- reload/trust/instrumentation requirement；
- interface stability 与 live delivery。

CapabilitySupport 只能是 Confirmed、Partial、Unsupported 或 Unknown；EvidenceAuthority 只能是 Authoritative、Observational 或 None；InterfaceStability 至少区分 Stable、VersionedExperimental、PrivateExperimental 和 Unknown，并附带 bounded reason。Contract 声明该 instance 的上限，AgentObservation.evidence 声明单条证据的实际值，实际值只能等于或低于上限。

Contract 是 per-instance 且可按 revision 更新。以同时提供 screen inference 与 lifecycle hook authority 的聚合型 runtime 为例，server instance contract 可以声明“Activity 最大可达 Authoritative，且每条记录提供 authority/provenance flag”；具体 pane observation 仍必须根据当时 flag 发出 Authoritative 或 Observational EvidenceClaim。单个 pane 的 hook clear/process takeover 通常只改变该 pane evidence，不抬升整个 instance；provider/manifest/protocol version 或全局语义变化才更新 ContractRevision。Contract downgrade 使受影响 evidence 重新验证/过期并触发 snapshot reconcile，但不直接伪造新状态。Core 和 UI 只根据 InstanceContract、EvidenceClaim 和 DecisionTrace 决定 completeness，不得根据 observer/subject 名称分支。

### 10.10 Integration management 边界

真实 adapter 未来可能需要 install/remove/status/doctor/reload，但该控制面不属于 CodeAgentAdapter 或 ObservationProvider。首期 core 只保留独立边界，不实现 production manager：

- ObservationProvider 永远不能隐式安装 Hook、修改配置或开启 feature flag；
- status/doctor 是只读诊断，可以生成安全化 IntegrationHealth；
- install/remove/reload 必须由显式用户动作触发，先生成 plan，使用原子合并与可逆备份，不能覆盖无关用户配置；
- executable assets 随 Latte Lens 版本发布并校验，不从 detection manifest 或远程 provider 动态下载执行；
- IntegrationHealth 可以影响 InstanceContract 的 `requires_instrumentation` 和 availability，但不能直接伪造 AgentObservation。

这沿用成熟终端运行时将 detection、integration 和 resume 分层的经验，但 Lens 不提供 resume，也不把安装管理耦合进 reducer/runtime。

## 11. 当前实现边界

当前已经实现 vendor-neutral core，以及 Codex command-hook、Claude Code command-hook、OpenCode plugin-event、TraeX command-hook 四个 adapter。四者共用同一 Hook CLI、exact-workspace 路由、live fan-out、metadata fallback、IdentityKeyer 和有界 JSON 安全边界；core/runtime/UI 中没有 observer-specific 分支。

### 11.1 Codex Hooks

- Codex adapter 以 `openai/codex-hook` 注册，不创建默认 decoder；
- `latte-lens hook --observer openai/codex-hook --event <Event>` 解码 Codex 官方 command-hook JSON；
- 支持 `SessionStart`、`UserPromptSubmit`、`PreToolUse`、`PermissionRequest`、`PostToolUse`、`SubagentStart`、`SubagentStop` 与 `Stop`；`PreCompact`/`PostCompact` 经过有界 JSON 校验后安全忽略；
- 不读取或保存 `prompt`、`tool_input`、`tool_response`、`last_assistant_message`、`transcript_path`、`agent_transcript_path`、`model` 或 raw `cwd`；官方明确把 transcript 格式标为不稳定接口；
- `session_id`、`turn_id`、`tool_use_id` 与 `agent_id` 只在 IdentityKeyer 边界内用于 install-scoped HMAC；core、metadata、IPC 和 UI 只接收稳定 digest；
- SessionStart 证明 `Lifecycle=Open`，但 Codex 没有 SessionEnd Hook，因此 Stop 只完成 turn 并设置短期 Idle evidence，绝不结束 session；
- PermissionRequest 只证明 Requested；PostToolUse 只证明受支持工具完成，不读取 tool result 猜测 success/failure；Codex 官方说明工具拦截并不完整，因此 Activity、Turn、Permission、Tool 与 AgentTopology capability 均声明 Partial；
- activity evidence 使用 30 秒 lease；没有新 Hook 时回退 Unknown，不把一次 Working/Idle 永久化为当前状态；
- 不自动创建或修改 `.codex/hooks.json` / `config.toml`，也不实现 install/remove/doctor/status；安装管理仍需要独立、显式、可逆的产品动作。

`Partial` 的边界是具体能力缺口，不是笼统降级：

| Domain | 已证明 | 尚不完整 |
|---|---|---|
| Lifecycle | `SessionStart` 证明 Open | 没有 SessionEnd 和 lifecycle snapshot，无法确认退出 |
| Activity | prompt/tool/permission/Stop 事件提供带 lease 的状态点 | 没有 current-state snapshot，漏 Hook 后只能回到 Unknown |
| Turn | 已观察的 turn hook 带稳定 `turn_id` | 没有历史/snapshot，无法恢复未观察 turn |
| Permission | `PermissionRequest` 证明 Requested | 不暴露 allow/deny/cancel resolution |
| Tool | 已拦截工具的 Pre/Post 证明 Started/Completed | Codex 工具拦截不完整，不推断 success/failure，也不能恢复漏事件 |
| AgentTopology | SubagentStart/Stop 证明直接 start/stop | 没有 topology snapshot，无法恢复启动前或漏掉的 subagent |

Change 与 Artifact 当前为 Unsupported；Codex Hook adapter 不读取 diff、文件内容或产物。Snapshot 与可重放 stream 也为 Unsupported，因此进程重启后只能从 metadata 摘要恢复，不能恢复 live timeline。

验证包含 adapter UT、官方 release 文档形状 fixture、production registry contract、离线 metadata fallback CLI E2E，以及 `latte-lens hook` 子进程到运行中 receiver 的 live E2E。所有进程级测试隔离 HOME、state root、runtime root 与 workspace，并对 prompt/transcript/cwd/native ID 做 byte canary 扫描；它们不读取或修改真实用户 Codex 配置。`make codex-hooks-canary` 还会用隔离 CODEX_HOME、loopback mock Responses provider 与本机已安装 Codex binary 验证 SessionStart 的最终命令接线；该 canary 默认 ignored、不进入常规 CI，也不能替代后续完整 turn/tool/subagent compatibility matrix。

未来的单个 Code Agent 或聚合运行时集成仍必须作为独立工作项，只能通过 CodeAgentAdapter、可选 ObservationProvider、LiveObservationPublisher 和独立安装器边界接入，不得修改 AgentState 或 UI 的 observer/subject-specific 分支。

#### Codex 配置契约

当前实现不自动安装 Hook。手工验证时，应把下列命令中的 `latte-lens` 替换为已安装 binary 的绝对路径，并在隔离的 Codex 配置目录中为每个事件配置 command hook：

~~~text
latte-lens hook --observer openai/codex-hook --event SessionStart
latte-lens hook --observer openai/codex-hook --event UserPromptSubmit
latte-lens hook --observer openai/codex-hook --event PreToolUse
latte-lens hook --observer openai/codex-hook --event PermissionRequest
latte-lens hook --observer openai/codex-hook --event PostToolUse
latte-lens hook --observer openai/codex-hook --event SubagentStart
latte-lens hook --observer openai/codex-hook --event SubagentStop
latte-lens hook --observer openai/codex-hook --event Stop
~~~

Codex 在 session cwd 中执行 command，因此无需把 raw cwd 拼进参数。Lens 与 Hook CLI 都把各自启动的 canonical 目录作为 workspace；只有目录精确相同时才实时感知。相同目录中的多个 Codex session 仍通过各自 SessionKey 独立展示。Hook 必须保持 stdout/stderr 为空并始终 fail-open；配置 timeout 建议为 1 秒，内部 live 与 metadata budget 仍分别受 5 ms 与 2 ms 上限约束。

### 11.2 Claude Code Hooks

Claude Code adapter 使用 observer `anthropic/claude-code-hook`，支持官方 command Hook 的以下事件：

- `SessionStart`、`SessionEnd`；
- `UserPromptSubmit`、`Stop`、`StopFailure`；
- `PreToolUse`、`PostToolUse`、`PostToolUseFailure`；
- `PermissionRequest`、`PermissionDenied`；
- `SubagentStart`、`SubagentStop`。

`PreCompact` / `PostCompact` 经过有界 JSON 校验后安全忽略；其他尚未声明的事件返回 UnsupportedEvent，不猜测映射。Adapter 只读取 `session_id`、可选 `prompt_id`、`tool_use_id`、`agent_id`、`agent_type`、`source`、`trigger` 和 `tool_name`。`prompt`、`tool_input`、`tool_response`、`error`、`error_details`、`reason`、`last_assistant_message`、`transcript_path`、`agent_transcript_path`、`model`、`permission_mode` 与 raw `cwd` 均被语法校验并跳过。

| Domain | Support | 证据边界 |
|---|---|---|
| Session | Confirmed | 所有已支持 Hook 都携带 `session_id` |
| Lifecycle | Confirmed | SessionStart/SessionEnd 分别证明 Open/Ended |
| Activity | Partial | 只有 30 秒 lease 的事件状态点，没有 current-state snapshot |
| Turn | Partial | `prompt_id` 从 Claude Code 2.1.196 起提供；旧版本和漏事件不能恢复 turn |
| Permission | Partial | 能看到 Requested 与 auto-mode Denied，看不到用户 allow/deny/cancel 结果 |
| Tool | Confirmed | Pre、成功 Post、失败 Post 分别证明 Started/Completed/Failed |
| AgentTopology | Partial | SubagentStart/Stop 是增量事件，没有 topology snapshot |

Change、Artifact、Snapshot 与可重放 stream 当前为 Unsupported。Stop/StopFailure 只结束或失败当前 turn，并设置短期 Idle evidence；只有 SessionEnd 才结束 session。缺少 `prompt_id` 时，UserPromptSubmit 只能生成 UnattributedEvidence，Stop/StopFailure 不会伪造 TurnKey 或 terminal turn。

Claude Code 的 command Hook 使用 `${CLAUDE_PROJECT_DIR}` 表示 session project root。配置必须通过 exec-form `args` 把它传给 `--workspace`，避免 session 中途 `cd` 后把同一 Agent 错误归入另一个 Lens 工作区。完整、可复制的手工配置见 [Claude Code Hooks 集成](../integrations/claude-code-hooks.md)。当前实现不自动修改 `~/.claude/settings.json`、项目 `.claude/settings.json` 或 `.claude/settings.local.json`。

验证包含官方文档形状 UT、production registry contract、最终二进制 exact-workspace offline/live E2E，以及 `make claude-hooks-canary`。Canary 使用隔离 HOME、显式临时 settings、dummy API key、loopback failure backend 和本机已安装 Claude CLI，只证明 SessionStart 最终接线，不代表完整事件/版本/平台矩阵。

### 11.3 OpenCode Plugins

OpenCode adapter 使用 observer `opencode/plugin`。本地 JavaScript bridge 从官方插件上下文取得精确 `directory`，把 native 事件裁剪成有界 identity/state 字段，再调用同一个 `latte-lens hook`：

- `session.created`、`session.updated`、`session.deleted` 映射 Session/Lifecycle；
- `session.status` 的 busy/retry/idle 映射权威 Activity；`session.idle` 因为紧随 status idle 发布而不重复转发；
- user `message.updated` 建立 Turn，status idle 完成当前关联 turn，`session.error` 只失败当前 turn；
- `permission.asked`/`permission.replied` 映射 Requested、Granted、Denied；
- `tool.execute.before`/`tool.execute.after` 映射 Started/Completed，tool error `message.part.updated` 补齐 Failed；
- child session 的 `parentID` 映射父 session 下的 subagent Observed/Released。

| Domain | Support | 证据边界 |
|---|---|---|
| Session | Confirmed | 支持的 native event 都携带 `sessionID` |
| Lifecycle | Confirmed | session.created/deleted 是 native boundary |
| Activity | Confirmed | session.status 明确区分 busy/retry/idle |
| Turn | Partial | 完成/失败依赖插件进程内的 user message correlation，无 turn snapshot |
| Permission | Partial | interactive asked/replied 可见，规则自动决策不可见 |
| Tool | Confirmed | before/after 与 error part 覆盖开始和成功/失败终态 |
| AgentTopology | Partial | parentID 是权威关系，但没有现存 topology snapshot |

插件不会转发 prompt/message、title、model、tool args/output、permission patterns/metadata、error、diff/file path 或 raw directory/worktree。`session.diff` 表达 current snapshot，而当前 Hook reducer 的 Change 是增量语义；为避免重复 snapshot 错误累加，Change 保持 Unsupported，等待 OpenCode read-only provider/snapshot slice。Artifact、Snapshot 和可重放 stream 同样 Unsupported。

插件使用无 shell 的参数数组启动 Hook、丢弃 stdout/stderr、1 秒超时并始终 fail-open。完整安装、exact-workspace、多 session/多 Lens 行为与数据边界见 [OpenCode 插件集成](../integrations/opencode-plugins.md)。验证包含 adapter UT、production registry contract、最终二进制 exact-workspace offline/live E2E，以及 `make opencode-plugin-canary`；真实 canary 使用隔离 HOME、临时本地插件和 loopback server 创建空 session，不读取用户配置、不调用模型、不访问公共网络。

### 11.4 TraeX Hooks

TraeX adapter 使用独立 observer `bytedance/traex-hook` 与 subject `bytedance/traex`。部分 Hook 形状与 Codex 具有共同来源不构成 identity 等价证明；两者的 native session 不能跨 namespace 合并，TraeX 的 contract、authority、instance 和 epoch 均独立声明。

- `SessionStart` / `SessionEnd` 映射 Lifecycle Open / Ended；
- `UserPromptSubmit` / `Stop` 映射 Turn Started / Completed；
- `PreToolUse` / `PostToolUse` / `PostToolUseFailure` 映射 Tool Started / Completed / Failed；
- `PermissionRequest` 只映射 Requested，不推断最终 decision；
- `SubagentStart` / `SubagentStop` 映射 Agent Observed / Released；
- `Notification`、`PreCompact`、`PostCompact` 经过有界 JSON 校验后忽略。

| Domain | Support | 证据边界 |
|---|---|---|
| Session | Confirmed | 所有支持事件都有 `session_id` |
| Lifecycle | Confirmed | SessionStart/SessionEnd 是 native lifecycle boundary |
| Activity | Partial | 只有 30 秒 lease 的事件状态，没有 current-state snapshot |
| Turn | Partial | turn-scoped Hook 有 `turn_id`，但无法恢复漏事件与历史 turn |
| Permission | Partial | 只看到 Requested，不暴露 allow/deny/cancel resolution |
| Tool | Confirmed | Pre、成功 Post、失败 Post 覆盖开始和两种终态 |
| AgentTopology | Partial | 只有增量 start/stop，没有 topology snapshot |

Adapter 只选择性读取有界 identity/state 字段；prompt、tool input/output、error、assistant message、transcript、thread name、model、permission mode 与 raw cwd 均不会进入 core、IPC 或 metadata。当前接口稳定性声明为 `PrivateExperimental`，Change、Artifact、Snapshot 和可重放 stream 为 Unsupported。

项目级配置使用 `.trae/hooks.json`，每个 handler 在 session 工作区运行 `latte-lens hook --observer bytedance/traex-hook --event <Event> --workspace .`。完整安装、信任、exact-workspace 与版本差异见 [TraeX Hooks 集成](../integrations/traex-hooks.md)。验证包含 adapter UT、production registry contract、最终二进制 offline/live exact-workspace E2E，以及显式选择 binary 的 `make traex-hooks-canary TRAEX_BIN=/path/to/traex`；canary 只证明当前 TraeX binary 的 SessionStart 接线。

## 12. Latte Lens 模块实现

~~~text
src/
  main.rs                  latte-lens CLI、fail-open hook 子命令与 TUI 启动分流
  agent/
    model.rs               AgentObservation 与 Session/Agent/Turn/Change/Artifact
    envelope.rs            SnapshotEnvelope/EventEnvelope/StreamRef
    adapter.rs             CodeAgentAdapter/AdapterRegistry contract
    codex.rs               Codex command-hook bounded decode 与 normalize
    claude.rs              Claude Code command-hook bounded decode 与 normalize
    opencode.rs            OpenCode plugin-event bounded decode 与 normalize
    traex.rs               TraeX command-hook bounded decode 与 normalize
    hook_json.rs           vendor Hook 共用的有界 JSON 选择性读取器
    provider.rs            ObservationProvider 与 provider instance lifecycle
    contract.rs            InstanceContract/CapabilityClaim
    identity.rs            IdentityKeyer 与 SessionKey/AgentKey/TurnKey
    crypto.rs              SHA-256/HMAC 与 install-scoped identity
    workspace.rs           canonical exact-directory identity
    live.rs                receiver registry、lease/TTL 与多 Lens fan-out
    hook.rs                通用 Hook invocation → adapter → dispatcher
    bootstrap.rs           production AgentRuntime/IPC 启动接线
    dispatcher.rs          live-first 与 metadata fallback 路由
    state.rs               snapshot/event reconcile 与 evidence arbitration
    explain.rs             bounded DecisionTrace
    runtime.rs             bounded request/completion、provider、expiry 与 generation
    metadata.rs            metadata model、state root、store 与 retention
    transport.rs           frame/ACK、in-memory、Unix socket 与 Windows named pipe
  bin/
    agent_observability_harness.rs
                            required-feature synthetic PTY binary
scripts/
  agent_e2e_tui.py         metadata-only → live 的 POSIX current-screen journey
~~~

接入规则：

- AgentRuntime 独立于现有 serialized WorkerRuntime。
- 使用 bounded channel，不复制 SearchRuntime unbounded channel。
- App::poll_background 是唯一 reducer mutation seam。
- workspace switch 增加 generation；旧 IPC/runtime result 在 mutation 前拒绝。
- provider instance 使用独立 epoch；Gap/Reset/reconnect 必须 reconcile。
- src/ui.rs 只消费 AgentViewState。
- RepoPath 负责 nested repo 中的安全相对路径。
- FakeAdapter、FakeProvider、InMemoryMetadataStore 和 FakePublisher 只放在 tests/support。
- 当前不创建 adapters/ 或 vendors/ 生产目录。

## 13. 安全与隐私

- 默认 metadata-only。
- install-scoped secret 只用于 HMAC identity，0600/current-user ACL。
- state root 统一为 LATTE_LENS_STATE_DIR → LATTE_HOME/lens/state → user-home/.latte/lens/state。
- Hook CLI 与 Lens 对同一 install ID 必须解析到同一 root identity；不一致时拒绝合并并报 StateRootMismatch。
- runtime root 统一为 LATTE_LENS_RUNTIME_DIR → XDG_RUNTIME_DIR/latte-lens → current-user TMPDIR；receiver manifest 和 endpoint 不进入 durable state。
- IPC 验证 current-user peer identity、protocol、receiver generation 和 membership。
- Message 硬上限为 4 KiB；stdin 硬上限为 64 KiB。
- Provider 单 item decode 输入硬上限为 64 KiB；snapshot 默认 256 items/256 KiB，超限显式 Truncated。
- Provider 使用 read-only method allowlist，不调用 start/send/focus/resume/config mutation。
- Metadata 文件硬上限为 4 KiB；startup scan/count 有界。
- 拒绝 symlink/reparse/network share/special file。
- 不记录 raw cwd/native IDs。
- Hook CLI 日志和 stdout/stderr 严格为空。
- Lens TUI 不直接执行文件或 IPC I/O。

## 14. 分阶段实施

### C0：核心契约

实现：

- AgentObservation、ObservationEnvelope、SubjectNamespace/ObserverId 与所有引用类型；
- CodeAgentAdapter、ObservationProvider、InstanceContract、IdentityKeyer、SessionMetadataStore 和 live transport traits；
- ObservationDispatcher、AgentRuntime request/completion、AgentState/AgentViewState 签名；
- tests/support 中的 fake 实现与 synthetic fixtures；
- production `agent-observability` 默认启用；synthetic harness 仍只在 required test feature 下构建。

验收：

- FakeAdapter 只能输出 bounded AgentObservation facts 或 Ignore；FakeProvider 只能输出 bounded raw snapshot/event；
- AdapterRegistry 拒绝重复 ObserverId，未知 ObserverId 不会回退到默认 decoder；
- InstanceContract 限制 per-instance support/authority，EvidenceClaim 不能越权；
- SubjectNamespace 与 ObserverId 分离，同一 native session 只有在 identity namespace/authority 可证明一致时合并；
- adapter 无法绕过 IdentityKeyer 把 raw identity 放入 core model；
- AgentState 不依赖 adapter、filesystem、IPC 或 UI；
- UI 入口只接受 AgentViewState；
- 生产 registry 只包含显式批准的 Codex、Claude Code、OpenCode 与 TraeX adapter，不包含 fake 或默认 decoder。

### C1：领域状态与 metadata

实现：

- identity/HMAC；
- SessionMetadata/WorkspaceMetadata filesystem store；
- project_metadata 与 ObservationDispatcher；
- any-event session upsert 与 monotonic merge；
- bounded caps/retention model；
- MetadataOnly → LiveObserved；
- lifecycle/activity/freshness/turn/session 分离；
- per-domain evidence candidates、tombstone、DecisionTrace 与 expiry arbitration。

验收：

- synthetic SessionStart、Prompt、Tool、Stop 和 SubagentStart 任一类 observation 都可独立建立 session；
- Mid-session 不伪造 started_at；
- accepted live path 不由 dispatcher 写 metadata；
- unavailable/busy/incompatible path 降级更新 metadata；
- 同 authority conflict 回退 Unknown/Partial，不按 observer 名称 last-write-wins；
- concurrent/out-of-order merge、32-agent truncation、corruption/lock-timeout fail-open；
- metadata privacy byte scan 通过。

### C2：Live transport、provider runtime 与 Agents UI

实现：

- current-user-only IPC；
- ObservationFrame、bounded queue 与 ACK；
- provider discover/probe/snapshot/subscribe/reconcile；
- AgentRuntime generation/epoch/backpressure/expiry/draining；
- AgentState 与 App::poll_background 接入；
- Agents list/detail。

验收：

- in-memory publisher/receiver 和 loopback IPC 都通过同一 contract suite；
- synthetic observation 在 Lens 运行时进入 UI，Lens 不在时只写 metadata；
- Busy/drop 变 LivePartial；
- Complete snapshot 可以 tombstone 自身 scope 内缺失 evidence，Partial/Truncated 不删除；
- Gap/Reset/epoch change 进入 Reconciling，完成 snapshot 后恢复 Current；
- 无新 event 时 EvidenceExpired 仍驱动 Freshness/Activity 降级；
- Explain 显示 winning observer、authority、provenance、expiry、gap/reconcile，且不含 raw payload；
- workspace switch 拒绝旧 generation；
- Lens crash 后 dispatcher 恢复 metadata 降级路径；
- 全部测试不启动或修改任何真实 Code Agent。

C0–C2 完成后，`openai/codex-hook`、`anthropic/claude-code-hook`、`opencode/plugin` 与 `bytedance/traex-hook` 已分别作为独立 slice 注册；它们不改变 core 的 vendor-neutral contract。后续真实集成仍需要单独设计、权限确认和验证计划。建议验证顺序为：四个 adapter 的完整 turn/tool/subagent compatibility matrix → OpenCode read-only provider/snapshot vertical slice → Codex app-server read-only 补全 → 聚合型终端 read-only bridge（验证 observer/subject 分离与动态 authority）。OpenCode plugin-event slice 不冒充 provider snapshot，聚合型 bridge 也不是单 Agent adapter 的替代品。

### 历史增强方案：可选 durable event history

如果需要离线 changes/artifacts、完整 replay 或跨重启 timeline，在历史增强方案中另行设计：

- Hook offline event spool；
- Lens-online batch spool；
- event checkpoint/snapshot；
- sealed compression；
- retention/quota/GC；
- crash recovery；
- durable multi-consumer history API。

历史增强方案必须另立协议与存储设计，不能把首期 Session Metadata 文件直接扩展为无界 event log。

## 15. 测试矩阵

项目级阻断门禁以 [Latte Lens 项目测试卡点设计](../testing/test-gates.md) 为准，其中 Files 和 Git Changes 的 production E2E 始终保留。本节列出 Agent 领域覆盖面；其分层归属、fake-only E2E harness、分阶段进入条件和测试卡模板以 [Code Agent 可观测性测试卡点设计](../testing/code-agent-observability-test-gates.md) 为准。C0–C2 不启动或修改任何真实 Code Agent。

### 15.1 接口契约

- FakeAdapter、FakeProvider、InMemoryMetadataStore、FakePublisher 通过 contract tests。
- AdapterRegistry 的 register/resolve、duplicate/unknown observer 行为确定。
- InstanceRegistry 的 instance revision、epoch change、contract downgrade 行为确定。
- AdapterError/IgnoreReason 无 raw payload 泄漏。
- AgentObservation 不含 transport 或 source-specific 字段。
- SubjectNamespace/ObserverId 分离；同 subject native ID 的可证明跨 observer 合并与不可证明不合并均有测试。
- ObservationDispatcher 的 Accepted/fallback/no-session 路由表。
- AgentState 和 project_metadata 在相同输入下产生 deterministic 结果。
- 生产构建只注册显式批准的 `openai/codex-hook` adapter。

### 15.2 Metadata

- 任意带稳定 SessionRef 的 synthetic observation 建立 session。
- SessionStart confirmed 与 mid-session discovery。
- first=min、last=max、Unknown 不覆盖 Known。
- terminal 后 revival。
- AgentKey merge、parent correction、32-agent/4-observer truncation。
- 2 秒写入间隔、结构事件强制更新。
- 2 ms lock timeout fail-open。
- temp/replace crash、checksum corruption。
- 4 KiB/256/4096 caps。
- ended/non-terminal retention。
- exact workspace 隔离、canonical alias 去重、不同父子目录不互相发现。

### 15.3 IPC

- test-only emitter 以真实子进程执行 exact command，覆盖 stdin/stdout/stderr/exit 与 5 ms 总预算。
- endpoint absent。
- partial/oversize frame、connect/ACK timeout、missing/malformed ACK。
- Accepted/NotMember/Busy/VersionMismatch/Invalid。
- queue 256 backpressure。
- ACK 丢失与 metadata 降级写入保持幂等。
- session identity report 先于 activity；EventId、receiver generation 和 sequence correlation 一致。
- Lens draining/crash。
- peer identity/ACL。
- workspace membership。
- A→B→A generation race。

### 15.4 Provider 与 reconcile

- subscribe-before-snapshot、watermark-then-subscribe 和 atomic handshake contract fixtures。
- Complete/Partial/Truncated snapshot scope 与 tombstone。
- provider snapshot 64 items/64 KiB per chunk、256 items/256 KiB aggregate caps、chunk ordering 和 cancellation。
- reconnect/epoch change、Reset、Gap、sequence discontinuity、decode/version failure。
- provider queue drop 必须触发 Reconciling，不能继续声明 Complete。
- per-instance capability/authority upgrade 与 downgrade。
- aggregate terminal-runtime fixture：presence、optional session identity、per-pane activity authority。
- read-only method allowlist 拒绝 start/send/focus/resume/config mutation。

### 15.5 Reducer/UI

- MetadataOnly 不显示 Working。
- Live event 升级 LiveObserved。
- Lifecycle、Activity、Freshness 独立；TTL 只使 Freshness Stale/Activity Unknown。
- idle/stale/revival 与无新 event 的 expiry timer。
- Stop turn-only。
- unattributed Stop 不关闭 arbitrary turn。
- 同 authority conflict 显示 Unknown/Partial 与 DecisionTrace。
- known/live/visible/completeness/truncated。
- per-observer observing_since/snapshot/reconcile/gap/drop coverage 聚合。
- live changes/artifacts 在退出后不伪装可恢复。
- dropped counter 显示 LivePartial。

### 15.6 Privacy

在 metadata、IPC/provider fixture、DecisionTrace、logs 中搜索 canary：

- prompt；
- response；
- tool input/output；
- command；
- transcript；
- source/diff；
- raw cwd/native ID；
- auth/env/token。

必须不存在。

### 15.7 平台

| Target | Metadata | IPC | Provider contracts | Core contracts | Package |
|---|---|---|---|---|---|
| Linux x86_64 | required | Unix required | required | required | required |
| Linux arm64 | required | Unix required | required | required | required |
| macOS x86_64 | required | Unix required | required | required | required |
| macOS arm64 | required | Unix required | required | required | required |
| Windows x86_64 | required | named pipe required | required | required | required |

每阶段运行 make ci；生产逻辑运行 make coverage；CLI/打包变化运行 make package-smoke。

### 15.8 E2E 证据与隔离

- Hook process、loopback ingress、headless reducer/view 与 PTY presentation 分层验证；PTY 文本不作为 Hook receipt 证据。
- 每个 runner 先通过 sandbox/recorder/watchdog/cleanup `--self-test`。
- HOME、XDG、state/runtime root 和 workspace 全部隔离；真实用户 Lens/Agent 配置在前后保持相同 digest 或相同“不存在”状态。
- 每次 CI 执行生成 bounded structured summary；失败额外保存 sanitized events、screen/terminal tail 和 cleanup receipt。
- 未来真实 integration 只在隔离 HOME、本地 mock backend 和 first-party structured lifecycle surface 下运行 compatibility canary；不使用真实账号、token 或开发者日常配置。

## 16. 决策日志

1. Hook/plugin payload 和 provider snapshot/event 都必须通过 CodeAgentAdapter 规范化为 AgentObservation；Lens 不启动 Agent、不 attach，也不调用 provider 控制 API。
2. SubjectNamespace 表示被观察产品，ObserverId 表示证据入口；两者不能折叠。
3. Session identity 只有在 subject/authority/native identity 可证明一致时跨 observer 合并，pane/cwd/最近活动不参与猜测合并。
4. SessionLifecycle、ActivityState、ObservationFreshness、Discovery 和 ObservationMode 分离；TTL 不生成终态。
5. 首期方案只持久化 bounded Session Metadata，不持久化完整 AgentObservation、SnapshotEnvelope、EventEnvelope 或 DecisionTrace。
6. Hook live event 走 local IPC；read-only provider 在 AgentRuntime 内执行 snapshot-first + event reconcile；两者只进入 bounded memory reducer。
7. Lens 不在线或 Hook IPC 失败时，ObservationDispatcher 只更新 metadata；provider 在 Lens 不在线时不运行。
8. SessionStart 是增强证据，不是创建 session 的唯一入口；任何包含稳定 SessionRef 的 Upsert 都可以创建 DiscoveredMidSession。
9. MetadataOnly 不能证明 Working/Open，也不能提供 turn/change/artifact history。
10. Provider 以 InstanceContract 声明 per-instance capability；单条 EvidenceClaim 只能降级，不能超过 contract authority。
11. Complete snapshot 只在声明 scope 内 tombstone 该 observer 的缺失 evidence；Partial/Truncated 不删除。
12. Gap、Reset、epoch change、provider queue drop 或 reconnect 强制 Reconciling；新 snapshot 前不声称 Current/Complete。
13. Session/agent native ID 只以 install-scoped HMAC stable key 落盘。
14. 每 session 一个覆盖写 metadata 文件，最大 4 KiB/32 agents/4 observers。
15. 高频 activity 按两秒间隔合并写入；结构事件立即尝试更新。
16. Metadata advisory、fail-open；不以 per-update fsync 阻塞 Agent。
17. 每个 Lens 拥有独立 receiver lease；Hook 按 keyed workspace membership 对最多 16 个匹配 receiver fan-out。同一工作区多个 Code Agent 仍按 SessionKey/AgentKey 独立归并。
18. Stop 只结束 turn；SessionEnd、failure terminal 与 lease evidence 单独处理。
19. Changes/artifacts 仅 live memory，使用 Exact/Observed/Inferred，并记录 observer provenance。
20. PreviewProvider 只处理安全 workspace file preview，不承载 Agent/provider live state。
21. Durable spool、compression、checkpoint 和 history 纳入历史增强方案，不能让 metadata index 演变为无界日志。
22. 当前生产 registry 只注册 `openai/codex-hook`、`anthropic/claude-code-hook` 与 `opencode/plugin`；test fakes、默认 decoder 与其他 production adapter/provider 均不得进入。

## 17. 风险与开放问题

### 17.1 主要风险

- Metadata file stat/lock/replace 可能在高频或慢磁盘上抖动。
- 两秒写入间隔可能让 Lens 启动时看到稍旧的 last_observed_at。
- MetadataOnly 无法判断 idle session 是否仍运行。
- Lens 在线但 queue 满时 live changes/artifacts 会丢失。
- 多 Lens fan-out 共享五毫秒总预算；慢 receiver 可能只获得后续 metadata 摘要，不能恢复丢失的 live detail。
- Synthetic contract tests 只能验证 core，不能证明任何真实 Code Agent 已兼容。
- 未来 adapter/provider 可能无法提供稳定 session/agent/turn identity，必须通过 InstanceContract 保持 Partial 或 Unknown，并可能产生同一真实 session 的未合并 rows。
- Provider 的 list 与 subscribe 可能没有 atomic cursor，错误握手会造成漏事件或错误 tombstone。
- 动态 authority/contract 频繁变化可能引发状态抖动，需要 revision、expiry 与 reconcile 去抖。
- Provider read-only allowlist 或 endpoint authentication 实现错误会扩大 Lens 权限边界。
- Agent list bounded/truncated，不能声称完整 topology。
- Workspace fingerprint 或 canonicalization 实现错误会造成精确目录误匹配或漏 session。

### 17.2 待验证参数

- 2 ms lock、2 ms connect、5 ms total ACK 是否适配五平台？
- Session metadata 使用哪种紧凑编码？
- Metadata atomic replace 不做每次 fsync 的实际 crash 行为如何？
- Windows named pipe peer identity/ACL 使用什么 API？
- receiver 数量超过 16、单个 receiver 长期 Busy 或 lease heartbeat 抖动时，UI 如何进一步解释 partial fan-out？
- Stale TTL 如何按 observer/activity/evidence 类型标定？
- 是否需要把 first-event metadata create 从 ACK 前移到 ACK 后？
- Provider connect/read/reconcile deadline、snapshot 256 items/256 KiB 起点是否适配真实聚合型终端 runtime/OpenCode/Codex？
- 无 atomic snapshot+cursor 的 provider 使用多长周期 reconcile？
- 聚合型终端 runtime 的 screen-derived 与 full-lifecycle-hook-derived activity 在 InstanceContract 中如何安全表达动态 authority？

### 17.3 未覆盖的验证项

以下结论尚无验证证据，不能作为 core 的当前能力声明：

- 除当前 Codex、Claude Code、OpenCode 三个已验证 slice 外，其他真实 Code Agent 已通过 CodeAgentAdapter 接入。
- 聚合型终端 runtime/OpenCode/Codex 任一真实 ObservationProvider 已通过 snapshot/event contract。
- 任何真实 Agent 的 event 都有 SessionRef。
- MetadataOnly 能证明 session 进程仍在。
- Live subagent/change/artifact 覆盖完整。
- 五平台 timeout/atomic replace/ACL 性能。
- 五平台下 16 个 live Lens 同时 fan-out 的时延分布；当前已有 receiver cap、membership 过滤、双 Lens Unix fan-out 和 Windows named-pipe loopback 的 synthetic contract tests。
- Durable history 的必要性和实现。

## 18. Core 里程碑完成标准

以下全部满足，core 里程碑才能认定完成：

- CodeAgentAdapter、ObservationProvider、IdentityKeyer、SessionMetadataStore、LiveObservationPublisher 和 LiveObservationReceiver 具有 fake contract tests。
- AdapterRegistry 以 ObserverId 显式注册，拒绝 duplicate/unknown observer，不使用 observer/subject enum 或默认 decoder。
- SubjectNamespace 与 ObserverId 分离；可证明的跨 observer identity merge 和不可证明的不合并都有 deterministic tests。
- AgentObservation 不含 raw payload、observer-specific enum 或 transport 字段；缺少 SessionRef 时不猜测绑定。
- SnapshotEnvelope/EventEnvelope、epoch/sequence/watermark、Complete/Partial/Truncated、tombstone、Reset/Gap/reconcile 具有 deterministic tests。
- InstanceContract 是 per-instance/revision，EvidenceClaim 不能越权；contract downgrade 会重新仲裁。
- SessionMetadata、WorkspaceMetadata 的编码形状、4 KiB/32-agent/4-observer bounds、HMAC identity、checksum、merge、truncation、retention 有 deterministic tests。
- ObservationDispatcher 的 Accepted 路径不写 metadata；Unavailable/Busy/Incompatible/timeout 路径有界降级更新 metadata。
- AgentState 是无 I/O 的 deterministic reducer；App::poll_background 拒绝旧 generation/epoch。
- AgentRuntime 的 bounded expiry scheduler 在无新 event 时仍能驱动 evidence 过期。
- MetadataOnly、LiveObserved、Discovery、Lifecycle、Activity、Freshness、Coverage 在 reducer/UI 中互相独立。
- MetadataOnly 不显示 Working/Open；session count 显示 known/live/visible/completeness。
- Equal-authority conflict 回退 Unknown/Partial，并产生不含 raw data 的 DecisionTrace。
- Stop 不结束 session；无 TurnKey terminal evidence 不关闭 arbitrary turn。
- Live changes/artifacts 不落盘，attribution 不把 Workspace/Inferred 冒充 Session/Exact。
- IPC 在五 target 通过 ACL、peer、version、membership、timeout、backpressure、draining/crash 和 generation race。
- Provider path 在五 target 通过 cancellation、read-only allowlist、byte/item cap、Gap/reconnect 和 snapshot reconcile contract；平台不支持的真实 provider 显式 Unsupported。
- State root 统一为 LATTE_LENS_STATE_DIR → LATTE_HOME/lens/state → user-home/.latte/lens/state，no-follow/reparse/network-share tests 通过。
- Privacy byte scan 证明 metadata、IPC fixture 和日志没有 prompt/tool body/transcript/raw ID/token。
- Synthetic envelopes 覆盖 mid-session discovery、turn/tool/subagent、change/artifact、stale/revival、tombstone、Gap 和 LivePartial。
- 生产 registry 只包含四个显式批准的真实 adapter，CLI 不暴露 synthetic observer，默认构建不包含测试 harness。
- make ci、make coverage 通过；CLI/打包变化时 make package-smoke 通过。
- 接口依赖方向、可靠性、隐私和五平台边界的验证项全部通过，且没有未解决的阻塞项。

## 19. 主要资料

- [Codex Hooks](https://learn.chatgpt.com/docs/hooks)
- [Claude Code Hooks](https://code.claude.com/docs/en/hooks)
- [OpenCode Plugins](https://opencode.ai/docs/zh-cn/plugins/)
- [TraeX Hooks 使用手册](https://bytedance.larkoffice.com/wiki/VPDVwJZxgiDcU1kkUsxc1Iq3n4b)
- [Latte Lens runtime](/Users/bytedance/projects/latte-co/latte-lens/src/runtime.rs)
- [Latte Lens App reducer seam](/Users/bytedance/projects/latte-co/latte-lens/src/app.rs:2642)
- [Latte Lens nested repositories](/Users/bytedance/projects/latte-co/latte-lens/src/repo_graph.rs:19)
- [Latte Lens Preview Provider 安全边界](./preview-providers.md)
