# Latte Lens Code Agent 可观测性测试卡点设计

状态：设计基线，尚未实现本文新增的测试 target 或 E2E harness。

本文是 [Latte Lens 项目测试卡点设计](./test-gates.md) 的 Code Agent 专项补充。Files、Git Changes、Search/Preview 的 production E2E 由项目级文档定义，并且不会因为新增 Agent 功能而降级或被替代。

本文定义 Code Agent 可观测性从 C0 core 到 C2 Agents UI 的阻断性测试门禁。它回答的是“什么测试通过后才能继续实现或合入”，不是重复描述产品协议。领域语义以 [Code Agent 可观测性设计](../design/code-agent-observability.md) 为准。

## 1. 核心决策

1. C0–C2 的 UT、contract test 和 E2E 全部使用 synthetic fixture，不启动、不配置、不修改任何真实 Code Agent。
2. UT 验证单个纯逻辑不变量；contract test 验证 trait/registry/contract 组合边界；E2E 验证进程、runtime、App reducer 和最终 UI 的真实链路。不能用大量 UI E2E 代替 reducer UT。
3. 现有 `scripts/e2e_tui.py` 继续作为 Files、Git Changes、Search/Preview 的阻断级 production E2E。Agent 场景使用独立 harness 和脚本，不把测试入口藏进 production CLI、环境变量或 registry。
4. production registry 在 C0–C2 始终为空。FakeAdapter、FakeProvider、FakePublisher、FakeIdentityKeyer 和故障注入只存在于 `tests/` 或专用测试 binary。
5. 相关门禁单次失败即阻断，不自动 retry。重跑只用于诊断，不能把第二次通过当成原失败已解决。
6. 测试使用显式 Timestamp、sequence、epoch 和 generation；除 PTY 等待屏幕收敛外禁止依赖 wall-clock sleep。
7. 每个缺陷必须在能够复现它的最低测试层增加回归用例；只有跨进程或终端行为才进入 E2E。
8. Hook 是否触发、IPC 是否接收和 UI 是否正确是三份独立证据。结构化 headless 证据先于 PTY；在终端历史输出中看到某个状态不能证明 Hook 链路正确。
9. 任何会启动 emitter、receiver 或测试 binary 的 E2E 都必须隔离 HOME、XDG、state root、runtime root 和临时 workspace，并证明真实用户配置在测试前后未变化。

## 2. 门禁流水线

~~~mermaid
flowchart LR
    G0["G0 Static / Build"] --> G1["G1 Deterministic UT"]
    G1 --> G2["G2 Contract Tests"]
    G2 --> G3["G3 Headless E2E"]
    G3 --> G4["G4 PTY / UI E2E"]
    G4 --> M["C0 / C1 / C2 milestone"]
    G2 -. "future integration only" .-> G5["G5 Real-agent Compatibility"]
~~~

| Gate | 验证内容 | C0 | C1 | C2 | 当前状态 |
|---|---|---:|---:|---:|---|
| G0 | format、all-features check、Clippy、MSRV、默认 feature/production guard | required | required | required | 已有基础门禁 |
| G1 | bounded model、identity、envelope、reducer、metadata、runtime 的确定性 UT | required | required | required | C0 部分已有 |
| G2 | fake adapter/provider/store/publisher 的组合 contract suite | required | required | required | C0 基线已有 |
| G3 | 无 PTY 的进程内或子进程 synthetic vertical slice | not required | metadata slice | required | 待设计实现 |
| G4 | 真实 terminal loop、Agent list/detail、键鼠与退出行为 | not required | not required | required | 待设计实现 |
| G5 | 某个真实 Code Agent 的版本兼容和权限验证 | forbidden | forbidden | forbidden | 未来独立工作项 |

G5 不属于当前 core 完成条件。没有真实集成时，G5 的“未执行”不能降低 G0–G4 的结果，也不能把 synthetic 通过表述为真实兼容。

### 2.1 当前基线与缺口

当前已经具备 all-features `make test`、`make ci`、两个独立的 coverage gate（Q1 直接单测责任 surface 保持 93% line floor，production binary + PTY 交互 surface 保持 85% line floor）、十个 C0 contract tests，以及 Linux/macOS 的仓库浏览 PTY E2E。它们是测试卡点的基础，但还不能称为 Agent observability E2E。

设计落地前仍有以下门禁缺口：

- MSRV job 和 Windows job 尚未用 all-features 编译/执行 agent core；
- 尚未执行 compile-fail doctest；
- C0 contract tests 尚未覆盖 5.2 的完整矩阵；
- 尚无 AgentState/metadata headless vertical slice；
- 尚无独立 Agent PTY harness；
- package job 尚未检查 synthetic/test binary 和 fixture 标记。

## 3. 按改动类型选择阻断门禁

| 改动范围 | 必须通过 | 额外要求 |
|---|---|---|
| identity/model/envelope/contract | G0 + G1 + G2 | privacy canary；默认 registry 为空 |
| AgentState、metadata projection/merge | G0 + G1 + G2 + G3 | `make coverage`；deterministic permutation tests |
| metadata filesystem store | G0 + G1 + G2 + G3 | 平台安全、crash/corruption、no-follow tests |
| IPC、provider runtime、expiry scheduler | G0 + G1 + G2 + G3 | Linux/macOS/Windows 对应 transport matrix |
| Agents App/UI | G0–G4 | Ratatui TestBackend 先于 PTY；当前屏幕断言 |
| CLI、测试 binary、打包内容 | G0–G4 | `make package-smoke`；production package negative check |
| 未来真实 adapter/provider | G0–G5 | 单独设计、单独权限确认，不复用 fake 结论 |

## 4. 测试可注入性边界

### 4.1 允许的 seam

- Adapter/Provider/Identity/Metadata/Publisher 使用已经定义的 trait 和 registry 注入 fake。
- AgentState 保持纯 reducer，直接接收 Timestamp、ValidatedEnvelope、expiry key，不为测试增加 I/O trait。
- AgentRuntime 的 timer/scheduler 需要一个最小 clock/wakeup seam；只有 scheduler 使用 FakeClock，领域状态仍使用显式 Timestamp。
- 故障通过 fake 的脚本化结果注入，例如 `MetadataFault::Contended`、`ProviderStep::Gap`、`PublishOutcome::Busy`，不在 production 分支中读取测试环境变量。
- 所有 synthetic input 先经过与 production 相同的 AdapterRegistry、InstanceContract 和 envelope validation。

### 4.2 禁止的 seam

- 默认 binary 中的隐藏 `--test-agent` 参数、测试 socket、fixture 路径或自动注册 fake。
- `cfg!(test)` 改变 reducer、contract、metadata merge 或 runtime backpressure 语义。
- E2E 直接修改 App 内部字段、跳过 AdapterRegistry 或构造“已验证”状态。
- 为了稳定测试而把 bounded queue 改成 unbounded，或把 timeout/ACL/no-follow 检查关闭。
- 测试通过 observer 名称、cwd、pane label 或“唯一 session”实现生产代码中不存在的 identity merge。

### 4.3 C2 测试 binary

PTY E2E 需要专用 binary，但不能污染 production registry。建议后续新增：

~~~text
tests/
  fixtures/
    agent_observability_harness.rs   Cargo required-feature 测试 binary
    agent_hook_emitter.rs            单次 synthetic Hook 子进程
    agent_recording_receiver.rs      记录 frame/ACK/时序的本地 receiver
    agent_scenarios/
      metadata_to_live.json
      provider_gap_reconcile.json
      workspace_generation.json
      privacy_canary.json
  support/
    agent/
      adapter.rs
      provider.rs
      metadata.rs
      publisher.rs
      scenario.rs
  agent_observability_contract.rs
  agent_state_integration.rs
  agent_runtime_e2e.rs
scripts/
  e2e_agent_headless.py
  e2e_agent_tui.py
~~~

测试 binary 与 production binary 必须复用同一个 terminal loop、App、AgentRuntime、AgentState 和 UI renderer，只替换 service construction。它只能在显式非默认 test feature 下构建；release archive 和 installer tests 必须断言其中不存在测试 binary、fixture、observer 或控制入口。

### 4.4 Hermetic harness 与自测

E2E runner 本身也属于被测试对象，不能因为它位于 `scripts/` 就默认可信。Headless 与 PTY runner 都必须提供快速 `--self-test`，至少证明：

- 测试依赖、临时目录、socket/named-pipe 和测试 binary 可用；
- HOME、XDG_DATA_HOME、XDG_CONFIG_HOME、XDG_STATE_HOME、XDG_CACHE_HOME、LATTE_HOME、LATTE_LENS_STATE_DIR 和 runtime root 全部指向同一个隔离 sandbox；
- sandbox 在正常退出、assertion failure、timeout 和 child crash 后都被清理；
- listener、emitter、Lens 和 PTY child 的 PID 均被登记，并在 teardown 后确认不存在；
- 真实用户的 Lens state root 和待观察 Agent 配置文件在测试前后保持相同 digest 或相同“不存在”状态；
- 本地 recorder 能返回 Accepted/Busy/NotMember/VersionMismatch/Invalid、延迟 ACK、丢失 ACK 和 malformed ACK；
- deadline watchdog 能终止故意挂起的 child，同时保留有界 failure evidence；
- privacy canary 不进入 stdout、stderr、trace、screen 或持久化文件。

测试应通过 child-specific environment 传递隔离变量，不在可并行用例中修改进程全局环境。确实只能修改全局环境的 contract test 必须使用串行锁，并在 teardown 中恢复原值。每次执行都生成 cleanup receipt，列出已终止 PID、已关闭 endpoint、已删除临时目录和未变化的外部 oracle。

## 5. G1：UT 卡点

### 5.1 通用规则

- 使用 table-driven cases；同一状态机的正向、拒绝和降级路径放在同一张 case table 中。
- 无序输入使用固定 permutation 集合或固定 seed，不使用 thread scheduling 作为乱序来源。
- channel/backpressure 使用 `try_send` 和 barrier，不使用 `sleep` 等待“应该已经满了”。
- expiry 使用 FakeClock 或显式 EvidenceExpired，不等待真实 TTL。
- 所有错误断言检查安全枚举和 disposition；不得通过匹配包含 raw payload 的错误字符串完成测试。
- `SensitiveId`、`SensitiveWorkspaceLocator` 等负向 API 契约使用 compile-fail doctest；未来 gate 需要显式执行 `cargo test --doc --all-features`。

### 5.2 C0 core 必须覆盖

| ID | 不变量 | 必须包含的 case |
|---|---|---|
| UT-BND-001 | 所有 protocol 容器 fail-closed | `N-1/N/N+1`；不静默 truncate；aggregate byte overflow |
| UT-ID-001 | namespace 有界且 vendor-neutral | 空值、大小写、非法分隔符、65 bytes、合法 organization/name |
| UT-ID-002 | raw identity 不进入 core | Sensitive 类型无 Debug/Display；safe digest Debug redacted |
| UT-ID-003 | merge 只由 subject/authority/native identity 决定 | 同 observer、跨 observer、不同 subject、不同 authority、不同 install |
| UT-MOD-001 | observation shape 自洽 | missing session/presence/agent/turn；workspace/session/parent mismatch |
| UT-MOD-002 | TTL 不伪造 terminal state | 允许的 Presence/Activity/Presentation/Open lease；其他 kind 拒绝 |
| UT-CON-001 | template 只能被 probe 收窄 | support、authority、provenance、lease、snapshot/stream semantics 各自 downgrade/upgrade |
| UT-CON-002 | destructive operation 必须有 authority | Clear/Delete/Released 与 observational/authoritative 矩阵 |
| UT-REG-001 | registry 无默认 decoder | duplicate、unknown、descriptor mismatch、empty production registry |
| UT-REG-002 | instance revision/epoch 单调 | insert、same revision same body、same revision mutation、stale revision、epoch switch |
| UT-ENV-001 | event 原子且有界 | empty Upsert、1/8/9 facts、empty Delete domains、mixed safe facts |
| UT-ENV-002 | snapshot item 必须落在 scope 内 | subject/workspace/entity/domain 四维正反例 |
| UT-RAW-001 | provider raw 数据有界 | item 64 KiB、aggregate 256 KiB、cursor/event-name bounds |
| UT-DSP-001 | dispatcher 路由表固定 | Accepted 不写 metadata；Unavailable/Busy/Incompatible fallback；no-session ignore；invalid reject |
| UT-RUN-001 | UI/runtime channel 非阻塞有界 | empty/full/disconnected、completion full、shutdown flag |
| UT-PRIV-001 | safe error 无原始数据 | payload、cwd、native ID、token canary 不出现在 Debug/Display |

现有 `tests/agent_observability_contract.rs` 是这张表的起点，不代表 C0 表格已经全部覆盖。进入 C1 前必须先补齐 UT-BND、UT-MOD、UT-CON、UT-ENV、UT-DSP 和 UT-PRIV 的缺口。

### 5.3 C1 reducer 与 metadata 必须覆盖

| ID | 不变量 | 必须包含的 case |
|---|---|---|
| UT-STATE-001 | any-event session upsert | SessionStart、Prompt、Tool、Permission、Stop、SubagentStart 单独作为首事件 |
| UT-STATE-002 | lifecycle/activity/freshness/turn 分离 | Stop 不等于 SessionEnd；TTL 只降 freshness/activity；terminal/revival |
| UT-STATE-003 | evidence arbitration deterministic | observer 输入全排列结果一致；authority/freshness/sequence/expiry/conflict 矩阵 |
| UT-STATE-004 | sequence/epoch/gap 固定 | duplicate、stale、unsequenced-after-sequenced、Reset、Gap、awaiting snapshot |
| UT-STATE-005 | snapshot tombstone 有 scope | Complete 删除自身缺失 evidence；Partial/Truncated 不删除；其他 observer 不受影响 |
| UT-STATE-006 | topology 与数量有界 | parent correction、32 agents、4 observers、known/live/visible/truncated |
| UT-META-001 | projection metadata-only | 不出现 prompt/response/tool body/diff；同 session facts 折叠为一个 delta |
| UT-META-002 | monotonic merge | first=min、last=max、Known 不被 Unknown 覆盖、terminal 后 revival |
| UT-META-003 | filesystem fail-open | contention、permission、corruption、checksum、temp/replace crash、capacity |
| UT-META-004 | retention bounded | workspace/session cap、稳定排序、ended/non-terminal retention、prune budget |
| UT-EXP-001 | 无新 event 仍可到期 | schedule、cancel/reschedule、同 deadline、多 generation、shutdown |
| UT-TRACE-001 | DecisionTrace 可解释且安全 | winner/conflict/gap/expiry/reconcile；无 raw input |

Reducer 的 permutation tests 至少覆盖：同一组 evidence 的顺序全排列、snapshot 与 event 的合法交错、同 authority conflict 的两种到达顺序。若组合数过大，使用固定 seed 生成并在失败信息中输出 seed。

### 5.4 C2 runtime 与 view 必须覆盖

| ID | 不变量 | 必须包含的 case |
|---|---|---|
| UT-RT-001 | workspace generation 隔离 | A→B、A→B→A、旧 metadata/provider/expiry completion |
| UT-RT-002 | provider failure 强制 reconcile | reconnect、epoch change、Reset、Gap、queue drop、decode/version failure |
| UT-RT-003 | draining 有界 | 不接收新 live event、drain 已接受项、provider cancel、completion queue full |
| UT-VIEW-001 | UI 只消费 AgentViewState | MetadataOnly、LiveObserved、Partial、Unknown、Unattributed presence |
| UT-VIEW-002 | count/coverage 不夸大 | known/live/visible、truncated、per-observer gap/drop/reconcile |
| UT-VIEW-003 | Explain 与状态一致 | Working/Unknown/Partial 的 visible reason 与 reducer DecisionTrace 一致 |

## 6. G2：Contract tests

Contract tests 对同一组行为运行多个 fake 实现，防止“trait 存在但每个实现语义不同”。

### 6.1 必需 suite

1. `AdapterContractSuite`
   - input 64 KiB、输出最多八 facts；Ignore 和 safe error；template/probe narrowing。
2. `ProviderContractSuite`
   - discover/probe/snapshot/event 都有 deadline 和 hard cap；Reset/Gap；没有任何写控制方法。
3. `MetadataStoreContractSuite`
   - in-memory 与 filesystem store 共享 load/merge/prune、排序、truncation、fail-open cases。
4. `PublisherReceiverContractSuite`
   - in-memory 与 loopback IPC 共享 Accepted/Busy/NotMember/Incompatible/draining cases；loopback 额外覆盖 partial frame、oversize frame、slow/missing/malformed ACK 和 duplicate EventId。
5. `IdentityKeyerContractSuite`
   - fake 与 production HMAC 共享稳定性、namespace/authority 隔离和 raw-value disposal cases；只比较语义，不比较具体 digest。
6. `RuntimeChannelContractSuite`
   - fake endpoint 与未来 runtime endpoint 共享 bounded/non-blocking/shutdown cases。

### 6.2 Contract suite 规则

- Suite 接收 factory，不复制 assertion。
- Fake 可以比 production 简单，但不能放宽 hard cap、authority 或 privacy contract。
- Provider contract 只证明读取接口行为，不证明任何外部产品兼容。
- OS-specific implementation 只有通过同一 suite 才能标为 available。
- Recorder 必须保存已经安全化的结构化 frame、ACK、连接次数和单调时间，不得只返回“某段日志出现过”。

## 7. G3：Headless E2E 卡点

### 7.1 Hook 链路证据阶梯

Hook 类集成必须逐层提供证据，不能用更外层的 smoke 代替内层契约：

| 层级 | 入口与边界 | 证明内容 | 不能证明 |
|---|---|---|---|
| L0 Emitter process contract | 精确执行生成的 hook command；真实 stdin/stdout/stderr/exit | payload decode、输出纪律、deadline、fail-open | Lens 已接收或 UI 已更新 |
| L1 Loopback ingress | emitter 子进程 → 真实 socket/named pipe → recorder/receiver | frame、peer/version/membership、ACK、EventId 幂等 | reducer/view 已收敛 |
| L2 Headless vertical slice | emitter/provider → validation → runtime → `App::poll_background` → reducer/view | 结构化状态、fallback、reconcile、privacy | 终端布局与输入行为 |
| L3 PTY presentation | 同一 scenario service → 真实 Lens terminal loop | 用户可见 row/detail、键鼠、退出恢复 | 某个真实 Agent 版本确实触发 Hook |
| L4 Compatibility canary | 隔离安装的真实 Agent + 本地 mock backend + first-party event stream | 某个受支持版本的安装与 live wiring | 其他版本、平台或未声明能力 |

L0–L3 使用 synthetic fixture，属于 C0–C2。L4 属于未来 G5，不能作为当前 core 合入条件，也不能反过来替代 L0–L3。

结构化事件必须至少关联 scenario ID、ObserverInstanceId、receiver generation、EventId、SessionKey 和 sequence。只按 event type 或终端文本做 `grep` 会把其他 session、陈旧事件或重复 delivery 当成成功，不是可阻断证据。ACK 丢失时 live event 可能已经被接收，因此 fallback metadata 与 live delivery 必须通过 EventId/SessionKey 幂等收敛。

### 7.2 Headless 场景卡点

Headless E2E 不使用 PTY，但必须跨越 Adapter/Provider → validation → runtime → App::poll_background → AgentState → AgentViewState。它是状态机和故障链路的主要 E2E，运行在 Linux、macOS 和 Windows。

| ID | 场景 | 最终断言 | 明确禁止 |
|---|---|---|---|
| E2E-H-001 | metadata bootstrap | row=MetadataOnly，activity=Unknown | 显示 Working/Open |
| E2E-H-002 | mid-session live event | 建立 DiscoveredMidSession 并升级 LiveObserved | 伪造 started_at |
| E2E-H-003 | live Accepted | UI 更新；emitter 不写 metadata | duplicate fallback write |
| E2E-H-004 | live Busy/Unavailable | metadata fallback；Agent path fail-open | panic/retry storm |
| E2E-H-005 | provider snapshot→events | Current snapshot 后按 sequence 增量更新 | event-first 声称 Complete |
| E2E-H-006 | Gap/Reset/reconnect | Reconciling/Partial，Complete snapshot 后恢复 | gap 后继续声称 Current |
| E2E-H-007 | Complete vs Partial tombstone | 只有 Complete scope 删除自身 evidence | 删除其他 observer/超出 scope |
| E2E-H-008 | expiry without new event | activity→Unknown/freshness→Stale | 合成 SessionEnd |
| E2E-H-009 | A→B→A generation race | 只接受当前 generation | 旧 completion 污染当前 view |
| E2E-H-010 | queue drop/backpressure | dropped/gap 可见且进入 reconcile | 只增加隐藏 counter |
| E2E-H-011 | identity merge | 可证明 identity 合并；presence-only 独立 | cwd/最近活动猜测合并 |
| E2E-H-012 | graceful drain/crash | drain 有界；后续 Hook 走 metadata fallback | 修改 Agent/provider 配置 |
| E2E-H-013 | privacy canary | state、metadata、trace、log 均无 canary | raw payload/native ID 泄漏 |
| E2E-H-014 | read-only invariant | workspace Git/config bytes 前后完全一致 | stage/reset/config mutation |
| E2E-H-015 | emitter stdio/exit contract | exact command 在预算内 exit 0，stdout/stderr 为空 | warning、阻塞或读取真实 Agent HOME |
| E2E-H-016 | ACK ambiguity | receiver 已收但 ACK 丢失时 live+metadata 幂等收敛 | duplicate row/evidence/count |
| E2E-H-017 | session/activity ordering | session identity report 先于 Working；相关 ID/generation 一致 | 短暂 unattributed Working |
| E2E-H-018 | hermetic cleanup | 真实 HOME/config/state digest 不变；child/endpoint/temp 全清理 | orphan、stale socket、外部写入 |

Headless E2E 的每个 scenario 使用临时 workspace、临时 HOME、临时 state/runtime root 和显式 synthetic timeline。测试结束后比较 Git status、相关配置文件 digest、state-root allowlist、真实用户配置 oracle 和 cleanup receipt。

### 7.3 Emitter/receiver 故障矩阵

L0/L1 使用真实子进程与真实本地 transport，但不启动真实 Code Agent。每个 transport implementation 必须覆盖：

- valid frame → Accepted；Accepted 路径不写 metadata；
- endpoint absent、connection refused、Busy、NotMember、VersionMismatch、Invalid；
- peer accepted 后不读、partial read、读完不 ACK、ACK 延迟超过总 deadline、malformed ACK、ACK 写出前断开；
- 4 KiB 边界的 N-1/N/N+1 frame，oversize 在 decode 前拒绝；
- 同一 EventId 的 duplicate delivery，以及 ACK-lost 后 metadata fallback 与 live delivery 的幂等合并；
- Session identity 与 activity 的发送顺序；activity 不能越过尚未完成的 session identity report；
- receiver crash、restart、generation 改变和 stale ready marker；
- stdout/stderr 纪律、exit 0、单次连接、无重试、5 ms publish 总预算和 2 ms connect 预算。

Readiness 使用 endpoint/health condition polling，不使用固定 sleep。只有 connect/EOF 等明确 transient 的测试 driver 操作可以在 scenario deadline 内重试；production Hook publisher 仍严格执行单次、无重试语义。结构化 parse/version/authority failure 立即失败，不能等待到 timeout 后统一报“未看到事件”。

## 8. G4：PTY / UI E2E 卡点

PTY E2E 只保留用户可见的关键旅程，边界组合仍由 G1–G3 覆盖。对应 L2 structured scenario 必须先通过，PTY 才能作为 presentation evidence。脚本必须复用现有 `TerminalScreen` 的“当前屏幕”模型，不能在历史 ANSI stream 中找到旧文本就算通过，也不能用 screen marker 代替 Hook receipt/ACK/EventId 证据。

### 8.1 首批必须场景

| ID | 用户旅程 | 当前屏幕断言 |
|---|---|---|
| E2E-T-001 | 打开 Agents 视图 | tab、known/live count、MetadataOnly row、无虚假 Working |
| E2E-T-002 | synthetic live event | row 升级 LiveObserved，Activity 与 Freshness 分列正确 |
| E2E-T-003 | 打开 detail/Explain | observer、authority、provenance、gap/reconcile reason 可见；无 raw canary |
| E2E-T-004 | provider Gap→snapshot | Partial/Reconciling 可见，恢复后状态和 count 收敛 |
| E2E-T-005 | workspace switch | A/B rows 不串台，切回 A 后旧 generation 不闪回 |
| E2E-T-006 | keyboard/mouse/resize | selection、hitbox、clipping、detail 与当前 row 一致 |
| E2E-T-007 | graceful quit | terminal 恢复、runtime draining、进程退出且 PTY 完整 drain |

### 8.2 Harness 执行顺序

1. 先运行 harness `--self-test`，确认 sandbox、recorder、deadline watchdog 和 cleanup oracle 可用。
2. 构建专用 test binary，不启用任何 production adapter。
3. 创建隔离 HOME/XDG、临时 Git workspace、state/runtime root 和 scenario 文件，并记录外部 oracle digest。
4. 启动 recorder/test services，再启动 PTY；等待 semantic readiness、mouse mode 与 Agents 初始 screen marker。
5. 喂入带 correlation ID 的 scenario step；先等待结构化 receipt/view checkpoint，再等待当前 screen 收敛，不使用固定 sleep。
6. 执行键盘/鼠标动作，断言当前 screen 的正向和 absent markers。
7. 发送退出，持续 drain PTY 直到 child exit；沿用现有 E2E 的防死锁方式。
8. 检查退出码、terminal restore、sanitized trace、state root、read-only invariant、外部 oracle 和 cleanup receipt。

Linux/macOS 执行 PTY suite。Windows 在 C2 先以 Ratatui TestBackend + headless E2E 阻断；若后续引入可靠 ConPTY harness，再升级为真实终端门禁，不能用 POSIX PTY 结果替代 Windows named-pipe/ACL 验证。

## 9. Production negative gate

默认和 release 构建必须满足：

- `agent-observability` 默认关闭时正常编译和运行；
- production AdapterRegistry 为空；
- CLI help、配置 schema 和环境变量中不存在 synthetic/test observer 入口；
- release archive 只有正式 binary、README、LICENSE 和约定资产；
- binary/string scan 不出现 fixture canary、scenario 名称或测试 socket 标记；
- package smoke 使用默认 features，测试 binary 使用 required test feature，二者不能复用同一产物。

该 gate 与 fake E2E 同等重要：fake vertical slice 通过不能以 production 包含 fake 为代价。

## 10. G5：未来真实集成兼容 E2E

G5 只在某个 production integration 独立立项后启用。它验证的是“声明支持的 Agent 版本确实安装、触发并到达 Lens”，不参与 C0–C2 core 完成判断。

### 10.1 安装管理 contract

每个 integration manager 必须在隔离 HOME/config root 中运行以下 matrix：

- 首次 install 写入正确 executable asset、hook manifest 和 feature flag；
- 连续 install 两次保持幂等，不产生 duplicate hook entry；
- 从已知旧 integration version 迁移到当前版本；未知新版本不被静默覆盖；
- remove 只删除自身 asset/entry，保留无关用户 hooks、配置、注释和 profile scope；
- custom home/config env、缺失目录、只读目录、symlink/reparse 和错误 schema fail-closed；
- status/doctor 区分 absent、current、outdated、modified、unsupported 和 permission denied；
- install/remove 前后生成 plan、变更摘要和可逆备份证据，不访问真实用户配置。

安装测试必须执行最终生成的 hook command，而不是只比较 JSON/TOML 字符串或直接调用内部函数。L0 process contract 对该命令重新运行 empty、malformed、valid、N-1/N/N+1、timeout 和输出纪律用例。

### 10.2 First-party live wiring proof

如果目标 Agent 提供结构化 app-server、server event stream 或等价 first-party surface，compatibility E2E 应：

1. 在隔离 HOME 中安装本地 integration asset；
2. 使用本机 loopback mock model/backend，禁止真实 API key、真实模型调用和公网访问；
3. 启动目标 Agent 的最终 binary，并通过其公开协议驱动最小 session/turn；
4. 捕获目标 Agent 的 hook started/completed 或等价 lifecycle event；
5. 同时捕获 Lens receiver receipt、ValidatedEnvelope 和最终 AgentViewState；
6. 以 session/event correlation 证明两端属于同一次触发，并检查真实用户配置 oracle 与 cleanup receipt。

只看到目标 Agent 的 structured event，只能证明它“会触发 Hook”；只有同一次触发继续到达 Lens receiver/reducer，才证明 integration wiring 完整。真实 TUI/tmux 场景只用于 boot/render/input smoke，不承担 hook correctness 断言。

如果某个 Agent 无法使用本地 mock backend、无法隔离用户配置或没有结构化 lifecycle evidence，该 canary 只能进入手工或 nightly non-blocking job，并在支持矩阵中明确降级。不能用真实账号、真实 token 或开发者日常 HOME 强行补齐 merge gate。

### 10.3 版本与启用策略

- 每个 integration 固定一组 minimum/current/next-unknown 版本 fixture；只对实际执行过的版本声明兼容。
- G5 先以 `draft` 落地，再进入 `shadow` 收集 flake/耗时，最后才可成为该 integration 的 `required` gate。
- 平台、版本或 feature flag 未覆盖时显示 Unknown/Unsupported，不从其他平台通过推断成功。
- 真实 compatibility canary 失败不能通过 PTY smoke、unit probe 或手工重跑覆盖。

## 11. 分阶段卡点

### C0 hardening gate

进入 C1 前：

1. 补齐 5.2 中尚未覆盖的 C0 case。
2. 把重复 fixture builder 收敛到 `tests/support/agent/`，但 production `src/agent` 不出现 fake。
3. 添加 compile-fail privacy/API tests。
4. 当前 `make ci`、`make coverage`、默认 feature check 全部通过。

G3/G4 在 C0 不阻断，因为还没有真实 AgentState vertical slice 或 Agents UI。

### C1 slice gate

每个 C1 slice 的合入顺序固定为：

1. 在本地先写能复现目标不变量的失败 UT/contract case；失败原因必须是“能力未实现”，不是 fixture 编译失败。
2. 实现最小 production 逻辑，使该 slice 的 G1/G2 通过。
3. metadata store 或 dispatcher slice 同时增加最小 G3 scenario。
4. 运行 `make ci` 和 `make coverage`，不降低当前两个独立门槛：Q1 直接单测责任 surface 的 93% line floor 和 production binary + PTY 交互 surface 的 85% line floor，分母以 Makefile 的两个 coverage filter 为准。

C1 完成需要 E2E-H-001 至 E2E-H-004、E2E-H-008、E2E-H-011、E2E-H-013 以及 E2E-H-015 至 E2E-H-018 通过。

### C2 slice gate

1. provider/runtime 先通过 E2E-H-005 至 E2E-H-010、E2E-H-012、E2E-H-014。
2. App reducer 接入先通过 TestBackend，再允许增加 PTY assertion。
3. Agents UI 完成时 E2E-T-001 至 E2E-T-007 全部阻断。
4. package negative gate 和三平台 headless matrix 必须通过。

任何真实 Code Agent 集成都不能用来填补 C1/C2 fake core 测试缺口。

## 12. 计划中的命令与 CI 映射

以下 target 是后续测试实现任务的预期接口，当前尚未加入 Makefile：

| Planned target | 内容 | 预算 |
|---|---|---:|
| `make agent-ut` | agent module UT + compile-fail doctest | 15 s |
| `make agent-contract` | fake contract suites | 15 s |
| `make agent-harness-self-test` | sandbox、recorder、watchdog、cleanup oracle 自测 | 10 s |
| `make agent-e2e-hook` | L0/L1 emitter process + loopback ingress | 30 s |
| `make agent-e2e` | L0–L2 all-platform headless scenarios | 60 s |
| `make agent-e2e-tui` | POSIX test-binary PTY scenarios | 180 s |
| `make agent-ci` | G0–G4 中当前阶段适用项 | 240 s |
| `make agent-compat-<integration>` | future G5 isolated compatibility canary | integration-specific |

映射建议：

- Linux quality：G0 + G1 + G2。
- Linux/macOS agent-headless：harness self-test + G3。
- Windows：G0 + G1 + G2 + G3，并运行 named-pipe/ACL contract cases。
- Linux/macOS agent-pty：G4。
- coverage：G1–G3；项目当前保持两个独立门槛，即 Q1 直接单测责任 surface 的 93% line floor 和 production binary + PTY 交互 surface 的 85% line floor，各自分母以 Makefile filter 为准。Agent coverage 落地时必须把新模块纳入适用的责任 surface，或增加独立阻断 gate；不得仅为维持数字而继续排除 agent 模块。
- package：production negative gate。
- Future integration job：只有达到 required 状态的 G5 canary 才阻断对应 integration，不替代主 agent-headless/PTY job。

预算是回归报警线，不是通过提高 timeout 掩盖阻塞或死锁的许可。单个 PTY screen wait 保持 10 秒上限；失败时保存 bounded terminal tail、最终 screen、scenario ID 和 sanitized trace。

## 13. Flake 与失败证据

- CI 不配置自动 retry。
- 用例失败必须输出 scenario/test ID、seed、generation、epoch、sequence、contract revision 和安全 disposition。
- PTY 失败保存最多 200 KiB raw terminal tail 与当前 screen；不能上传完整 prompt/payload fixture。
- Headless E2E 输出 bounded transition trace，不输出 raw cwd/native ID。
- CI 每次运行都生成 bounded `summary.json`；失败时额外保存 sanitized `events.ndjson`、`screen.txt`、terminal tail、stdout/stderr tail 和 `cleanup.json`。
- `summary.json` 记录入口命令、production/test binary digest、scenario ID、协议/contract revision、correlation IDs、step timing、外部 oracle 结果和最终断言；不记录 raw prompt、token、native ID 或绝对用户路径。
- `cleanup.json` 必须证明 child PID 已退出、endpoint 已关闭、temp root 已删除、真实配置 digest 未变化。cleanup 失败与业务断言失败同样阻断。
- Runner 要区分 readiness timeout、scenario timeout、screen convergence timeout 和 child-exit timeout；不能统一报告“未看到 marker”。
- 若只在某平台失败，该平台 gate 保持失败；不能用其他平台通过覆盖。
- 修复 flake 时保留原语义断言，优先移除 sleep、共享全局状态、未 drain channel/PTY 和非隔离 state root。

## 14. 测试卡模板

新增测试前先填写：

~~~text
ID:
Milestone: C0 / C1 / C2 / future integration
Invariant:
Layer: UT / Contract / Headless E2E / PTY E2E
Fixture:
Entrypoint / exact command:
Input timeline:
Fault injection:
Structured correlation:
Expected state/view:
Negative assertions:
Privacy assertions:
External isolation oracle:
Platforms:
Timeout budget:
Failure evidence:
Cleanup receipt:
~~~

如果一个测试卡无法写出明确的 negative assertion 或失败证据，它还不是可阻断的卡点。

## 15. 当前下一步

下一轮测试实现按以下顺序推进：

1. C0 UT/contract gap analysis，对照 5.2 给当前十个 contract tests 建覆盖表。
2. 补齐 C0 hardening tests 和 compile-fail privacy gate。
3. 先实现 hermetic sandbox、recording receiver、deadline watchdog 和 `--self-test`，再让业务 scenario 依赖它们。
4. 在实现 C1 AgentState 前，先落 UT-STATE-001 至 UT-STATE-005 的 table fixtures。
5. C1 metadata/dispatcher slice 可运行后建立 L0–L2 headless harness，不等待 Agents UI。
6. C2 UI view model 稳定后再建立独立 PTY harness，并复用已经通过的 structured scenario。

当前阶段不新增真实 adapter/provider，不把现有仓库浏览 PTY smoke 当作 Agent observability E2E 已完成。
