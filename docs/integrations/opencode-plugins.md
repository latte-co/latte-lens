# OpenCode 插件集成

Latte Lens 通过 OpenCode 官方本地插件 API 接收 session、activity、turn、permission、tool 和子 session 状态。插件只是 `latte-lens hook` 的 JavaScript 桥接层；adapter、传输、metadata fallback 和多 Lens fan-out 都编译在 `latte-lens` 中，没有额外守护进程。

## 安装

安装器经用户确认后会运行 `latte-lens hooks setup`，把内置插件安装到 `~/.config/opencode/plugins/latte-lens.js`；设置 `XDG_CONFIG_HOME` 时使用该配置根。现有同名文件只有在能识别为 Latte Lens 插件时才会升级，否则 Setup 拒绝覆盖。事务备份、失败回滚和恢复方法见 [Code Agent Hooks 安装与恢复](./hook-setup.md)。

需要只对单个项目启用时，把项目级插件放在 OpenCode 工作区的 `.opencode/plugins/`：

```sh
mkdir -p .opencode/plugins
cp /path/to/latte-lens/integrations/opencode/latte-lens.js \
  .opencode/plugins/latte-lens.js
```

OpenCode 启动时会自动加载用户级或项目级插件目录中的 JavaScript/TypeScript 文件。Latte Lens binary 必须位于 OpenCode 继承的 `PATH`；也可以显式指定：

```sh
LATTE_LENS_BIN=/absolute/path/to/latte-lens opencode
```

插件不依赖 npm 包，不读取或修改 `opencode.json`。复制文件时必须保留现有插件，不能覆盖整个 `.opencode` 目录。

## 工作区归属

插件使用 OpenCode 初始化上下文中的 `directory` 调用：

```text
latte-lens hook \
  --observer opencode/plugin \
  --event <native-event> \
  --workspace <OpenCode directory>
```

`latte-lens hook` 会 canonicalize 该目录。只有从相同 canonical 目录启动的 Lens 才接收事件：

- Lens 在仓库根目录、OpenCode 在子目录启动时，不归入同一个工作区；
- 同一目录中的多个 OpenCode session 使用各自 native `sessionID` 独立展示；
- 同一目录中的多个 Lens receiver 都能收到实时事件；
- 没有 Lens 运行时，事件降级写入私有 metadata，之后启动 Lens 仍能发现 session。

插件调用没有 shell 拼接，stdout/stderr 被丢弃，并在 1 秒后终止异常卡住的 Hook 进程。任何启动、解析或投递失败都 fail-open，不改变 OpenCode 的正常行为。

## 事件映射

| OpenCode 入口 | Latte Lens 状态 | 说明 |
| --- | --- | --- |
| `session.created` / `session.updated` / `session.deleted` | Session、Lifecycle | created/deleted 是 native lifecycle 边界 |
| `session.status` | Activity | `busy`、`retry`、`idle` 是权威状态 |
| user `message.updated` | Turn Started | 只转发 session/message ID |
| `session.status: idle` | Turn Completed | 使用插件内存中当前 user message ID 关联 |
| `session.error` | Turn Failed | 不把一次 turn error 误标成整个 session failed |
| `permission.asked` / `permission.replied` | Requested、Granted、Denied | `once`/`always` 为 Granted，`reject` 为 Denied |
| `tool.execute.before` / `tool.execute.after` | Tool Started、Completed | after 只表示成功完成 |
| tool error `message.part.updated` | Tool Failed | 补齐 direct after hook 不覆盖的失败终态 |
| child session `parentID` | Agent Observed、Released | 子 session 作为父 session 的 subagent 展示 |

`session.idle` 不重复转发，因为 OpenCode 源码在发布 `session.status: idle` 后紧接着发布该兼容事件。`session.diff` 也暂不转发：它表达当前 diff snapshot，而现有 Hook event reducer 只支持增量 Change 计数；强行映射会在重复 snapshot 时累加错误。Lens 的 Git Changes 视图仍独立读取实际工作区，OpenCode Change 能力保持 Unsupported，等待 snapshot/provider 契约接线。

## 数据边界

插件只向 adapter 发送这些有界字段：

- `session_id`、可选 `parent_session_id`；
- `turn_id`；
- `tool_call_id`、`tool_name`；
- `permission_id`、`reply`；
- `status`、`hook_event_name` 和单次 bridge `event_id`。

以下原始内容不会进入 Hook stdin、IPC、metadata、日志或 TUI 状态：prompt/message 文本、session title、模型、tool arguments/output、permission patterns/metadata、error 内容、diff/file path、原始 directory/worktree。

## 能力声明

| Domain | Support | 边界 |
| --- | --- | --- |
| Session | Confirmed | 支持的 native 事件都携带 `sessionID` |
| Lifecycle | Confirmed | created/deleted 明确覆盖生命周期边界 |
| Activity | Confirmed | native status 明确区分 busy/retry/idle |
| Tool | Confirmed | before/after 与 error part 覆盖开始和终态 |
| Turn | Partial | 完成/失败依赖当前插件进程中的 message correlation，无历史 snapshot |
| Permission | Partial | 可见交互式 asked/replied；规则自动放行/拒绝不可见 |
| Agent Topology | Partial | parentID 是权威关系，但没有现存拓扑 snapshot |
| Change、Artifact | Unsupported | 不从当前事件桥推断或累加不可靠事实 |

## 验证

```sh
# Adapter UT、registry contract 和最终二进制 Hook E2E
make agent-ut
make agent-contract
make agent-e2e-hook

# 本机已安装 OpenCode 时运行真实插件兼容性 canary
make opencode-plugin-canary
```

真实 canary 使用临时 HOME、临时插件目录、loopback OpenCode server 和空 session.create 请求。它不读取用户 OpenCode 配置、不携带 provider key、不发送 prompt、不调用模型，并阻断公共网络访问。
