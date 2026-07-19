# Code Agent Hooks 安装与恢复

`latte-lens hooks setup` 把已安装 binary 的绝对路径接入现有的用户级 Code Agent 配置。安装脚本在 binary 校验并安装成功后询问是否执行该命令；`-y` / `--yes` 用于 POSIX 自动化安装，PowerShell 保存后的脚本支持 `-y` / `-Yes`，管道执行可设置 `LATTE_LENS_YES=1`。

Setup 只处理已经存在的 Agent 配置目录，不通过创建目录猜测某个 Agent 已安装：

| Agent | 用户级目标 |
|---|---|
| Codex | `$CODEX_HOME/hooks.json`，未设置时为 `$HOME/.codex/hooks.json` |
| Claude Code | `$CLAUDE_CONFIG_DIR/settings.json`，未设置时为 `$HOME/.claude/settings.json` |
| OpenCode | `$XDG_CONFIG_HOME/opencode/plugins/latte-lens.js`，未设置时为 `$HOME/.config/opencode/plugins/latte-lens.js` |
| TraeX | `$HOME/.trae/hooks.json` |

Codex、Claude Code 与 TraeX 使用 JSON 语义合并，只替换带对应 Latte Lens observer 的 command Hook，保留其他字段、事件和 handler。OpenCode 使用独立的 `latte-lens.js`；如果同名文件不是 Latte Lens 插件，Setup 会拒绝覆盖。重复 Setup 不会追加重复条目。

Codex command Hook 显式传递 `--workspace .`。Codex 在 session 工作目录中运行 Hook，因此 `.` 对应 `codex -C <path>` 选择的精确目录；本地兼容性 canary 会验证生成的 metadata workspace 与该目录一致。

## 事务和备份

Setup 在修改前解析并验证所有目标。任意配置畸形、超过 1 MiB、是符号链接/reparse point 或不是普通文件时，整个事务在写入前失败。

需要写入时，Setup 创建两份原始字节备份：

- 私有临时目录 `TMPDIR/latte-lens-hooks-<transaction-id>` 用于本次失败的即时回滚；
- Latte Lens state 下的 `hook-backups/<transaction-id>` 保存恢复 manifest 与最多五次最近事务。

配置文件通过同目录临时文件原子替换。提交中任一步失败时，已经写入的文件会按逆序自动恢复；binary 保持已安装。若恢复时发现文件已经被其他进程修改，Lens 不会覆盖该文件，并保留持久备份供人工处理。

成功后命令会打印 transaction id 与持久备份目录。需要恢复时执行：

```sh
latte-lens hooks restore <transaction-id>
```

Restore 只在目标文件仍与该事务安装后的摘要一致时执行。用户后续改过配置时会拒绝整文件恢复，避免覆盖新设置。

Setup 不修改项目级 `.claude/`、`.opencode/` 或 `.trae/` 配置，也不绕过 Code Agent 的 Hook 信任确认。项目级接入仍应由用户在对应工作区显式配置。
