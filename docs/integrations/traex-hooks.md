# TraeX Hooks 集成

Latte Lens 通过 TraeX command Hooks 接收 session、turn、tool、permission 和 subagent 状态。Adapter 已编译进 `latte-lens`，observer 为 `bytedance/traex-hook`，subject 为 `bytedance/traex`；即使部分 Hook 形状与 Codex 相似，TraeX 仍使用独立身份、authority 和能力声明。

## 安装

推荐在 Lens 与 TraeX 共同使用的工作区安装项目级配置：

```sh
mkdir -p .trae
cp /path/to/latte-lens/integrations/traex/hooks.json .trae/hooks.json
```

如果项目已经有 `.trae/hooks.json`，必须把 `integrations/traex/hooks.json` 中各事件的 handler 合并进现有 `hooks` 对象，不能覆盖其他 Hook。配置中的 `latte-lens` 必须位于 TraeX 继承的 `PATH`；也可以把每条 `command` 的开头替换为 binary 绝对路径。

用户级 Hook 位置可能随 TraeX 版本变化。项目级 `.trae/hooks.json` 直接跟随工作区，因此是本集成唯一维护和推荐的配置方式。TraeX 会对 Hook 来源执行信任检查；首次启用后应审阅配置并按 TraeX 提示确认，不要在日常使用中关闭信任校验。

Latte Lens 不会自动创建或修改 TraeX 配置，也不会保存 Hook 信任状态。

## 命令与工作区契约

配置文件为每个事件执行：

```text
latte-lens hook \
  --observer bytedance/traex-hook \
  --event <Event> \
  --workspace .
```

TraeX 在 session 工作区中运行 command Hook，因此 `.` 表示 TraeX 启动时选择的精确工作区。`latte-lens hook` 会 canonicalize 该目录，只有从相同 canonical 目录启动的 Lens 才接收事件：

- Lens 在仓库根目录、TraeX 在子目录启动时，不归入同一个工作区；
- 同一目录中的多个 TraeX session 使用各自 `session_id` 独立展示；
- 同一目录中的多个 Lens receiver 都能收到实时事件；
- 没有 Lens 运行时，Hook 降级写入私有 metadata，之后启动 Lens 仍能发现 session。

Hook 配置使用 1 秒外层 timeout。Lens 内部 live 和 metadata budget 分别为 5 ms 与 2 ms；成功、未知事件、畸形输入、receiver 不可用和 fallback 失败均静默 fail-open，stdout/stderr 为空，不改变 TraeX 的正常执行结果。

## 事件映射

| TraeX Hook | Latte Lens 状态 | 说明 |
| --- | --- | --- |
| `SessionStart` | Session Observed、Lifecycle Open | native session 开始边界 |
| `UserPromptSubmit` | Turn Started、Activity Working | 使用 `turn_id` 关联 turn |
| `PreToolUse` | Tool Started、Activity Working | 使用 `tool_use_id`，不读取 tool input |
| `PermissionRequest` | Permission Requested、Activity WaitingPermission | 当前 Hook 不暴露用户最终决策 |
| `PostToolUse` | Tool Completed、Activity Working | native 成功完成边界 |
| `PostToolUseFailure` | Tool Failed、Activity Working | native 失败完成边界 |
| `SubagentStart` / `SubagentStop` | Agent Observed / Released | 增量拓扑，没有现存 snapshot |
| `Stop` | Turn Completed、Activity Idle | 只结束当前 turn，不结束 session |
| `SessionEnd` | Lifecycle Ended、Activity Clear | native session 结束边界 |

`Notification`、`PreCompact` 和 `PostCompact` 会经过有界 JSON 校验后忽略，因为它们本身不证明新的可观测领域事实。

## 数据边界

Adapter 只选择性读取有界的 `session_id`、`turn_id`、`tool_use_id`、`agent_id`、`agent_type`、`source`、`trigger`、`tool_name` 和 `hook_event_name`。native ID 仅在 IdentityKeyer 边界内用于 install-scoped HMAC，core、IPC、metadata 和 UI 只接收 digest。

以下内容不会进入 Lens 状态或持久化：prompt、tool input/output、error、last assistant message、transcript path、thread name、model、permission mode 和 raw cwd。

## 能力边界

| Domain | Support | 边界 |
| --- | --- | --- |
| Session | Confirmed | 所有已支持 Hook 都携带 `session_id` |
| Lifecycle | Confirmed | SessionStart/SessionEnd 覆盖生命周期边界 |
| Activity | Partial | 事件状态使用 30 秒 lease，没有 current-state snapshot |
| Turn | Partial | turn-scoped Hook 携带 `turn_id`，漏事件和历史 turn 无法恢复 |
| Permission | Partial | 只证明 Requested，不暴露 allow/deny/cancel resolution |
| Tool | Confirmed | Pre、成功 Post、失败 Post 覆盖开始和两种终态 |
| Agent Topology | Partial | 只有增量 start/stop，没有 topology snapshot |

Change、Artifact、Snapshot 与可重放 stream 当前为 Unsupported。当前 TraeX Hook 接口按 `PrivateExperimental` 管理；升级 TraeX 后应重新运行真实兼容性 canary。

## 验证

```sh
# Adapter UT、registry contract 和最终二进制 exact-workspace Hook E2E
make agent-ut
make agent-contract
make agent-e2e-hook

# 用与当前 Hooks 契约匹配的 TraeX 运行真实 SessionStart canary
make traex-hooks-canary TRAEX_BIN=/absolute/path/to/traex
```

真实 canary 使用隔离的 HOME、TraeX config、Lens state/runtime 和临时 Git 工作区；模型请求只会到 loopback failure server，不读取用户配置、不携带 provider key，也不访问公共网络。测试只证明所选 TraeX binary 能从隔离配置触发 `SessionStart`，不代表完整事件、版本或平台矩阵。

`TRAEX_BIN` 只用于选择待验证的 TraeX executable；Latte Lens 不检测、适配或声明支持其他产品。事件契约以 [TraeX Hooks 使用手册](https://bytedance.larkoffice.com/wiki/VPDVwJZxgiDcU1kkUsxc1Iq3n4b) 为准。
