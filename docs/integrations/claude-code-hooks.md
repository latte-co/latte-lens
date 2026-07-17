# Claude Code Hooks 集成

Latte Lens 通过 Claude Code 官方 command Hooks 接收 session、turn、tool、permission 和 subagent 的状态证据。Adapter 已编译进 `latte-lens`，没有额外守护进程或独立 adapter executable。

## 1. 前置条件

- 安装包含 `anthropic/claude-code-hook` adapter 的 `latte-lens`。
- 获取 `latte-lens` binary 的绝对路径。
- Lens 与 Claude Code 使用同一个 canonical project 目录启动。

Latte Lens 不会自动修改 Claude Code 配置。下面的设置需要由用户显式加入 `~/.claude/settings.json`、项目 `.claude/settings.json` 或 `.claude/settings.local.json`。选择哪一层决定 Hook 的作用域；现有其他设置和 Hooks 必须保留并合并，不能整文件覆盖。

## 2. 命令契约

每个 Hook 都执行同一个 binary，只通过 `--event` 选择事件：

~~~text
/absolute/path/to/latte-lens hook \
  --observer anthropic/claude-code-hook \
  --event <Event> \
  --workspace "${CLAUDE_PROJECT_DIR}"
~~~

使用 exec-form `args` 后，`${CLAUDE_PROJECT_DIR}` 由 Claude Code 作为单个参数替换，路径中的空格和特殊字符不会经过 shell 分词。这个值是 project root；即使 Claude 在 session 中执行 `cd`，Hook 仍归属于最初选择的工作区。

Hook 始终静默 fail-open：成功、未知事件、畸形输入、receiver 不可用和 metadata fallback 失败都以 exit 0 结束，stdout/stderr 为空。外层 timeout 建议为 1 秒；Lens 内部 live 和 metadata budget 分别为 5 ms 和 2 ms。

## 3. Settings 示例

把所有 `/absolute/path/to/latte-lens` 替换为实际 binary 绝对路径。下面配置覆盖当前 adapter 声明的完整事件集合；省略 matcher 表示匹配该事件的所有合法值。

~~~json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "SessionStart", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "UserPromptSubmit", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "PreToolUse", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ],
    "PermissionRequest": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "PermissionRequest", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ],
    "PermissionDenied": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "PermissionDenied", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "PostToolUse", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ],
    "PostToolUseFailure": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "PostToolUseFailure", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ],
    "SubagentStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "SubagentStart", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ],
    "SubagentStop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "SubagentStop", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "Stop", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ],
    "StopFailure": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "StopFailure", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/latte-lens",
            "args": ["hook", "--observer", "anthropic/claude-code-hook", "--event", "SessionEnd", "--workspace", "${CLAUDE_PROJECT_DIR}"],
            "timeout": 1
          }
        ]
      }
    ]
  }
}
~~~

如果同一事件已经配置其他 Hooks，应把新的 handler 或 matcher group 合并进现有数组，而不是替换原数组。

## 4. 工作区与多实例语义

- Lens 与 `${CLAUDE_PROJECT_DIR}` canonicalize 后完全相同才实时匹配。
- 父目录 Lens 不接收从子目录 project 启动的 Claude Code。
- 同一工作区多个 Claude session 通过各自 SessionKey 独立展示。
- 同一工作区多个 Lens receiver 都会收到同一个事件。
- 没有 Lens 时只写入 exact-workspace metadata 摘要；不持久化 raw cwd、prompt、tool payload、transcript 或 native ID。

## 5. 验证

仓库内验证命令：

~~~bash
make agent-e2e-hook
make claude-hooks-canary
~~~

`make agent-e2e-hook` 使用最终 `latte-lens` binary、隔离目录和真实本地 transport，验证 offline/live/exact-workspace/privacy 契约。`make claude-hooks-canary` 仅在本机已安装 Claude Code 时运行；它使用临时 settings、隔离 HOME、dummy API key 和 loopback failure backend 验证真实 CLI 能触发 SessionStart，不调用模型，也不读取或修改用户 Claude 配置。

完整工程交付仍需运行 `make ci`、`make coverage` 和 `make package-smoke`。

## 6. 当前能力边界

- Lifecycle 和 Tool 为 Confirmed。
- Activity、Turn、Permission 和 AgentTopology 为 Partial。
- Change、Artifact、Snapshot 和可重放 stream 为 Unsupported。
- 兼容性 canary 只覆盖当前安装版本的 SessionStart，不代表所有事件、版本或平台已经验证。
