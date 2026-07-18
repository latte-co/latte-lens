# Latte Lens 文档

Latte Lens 当前只维护中文版文档。文档按职责分层，不为尚未维护的英文版创建镜像目录。

## 设计

- [Code Agent 可观测性设计](design/code-agent-observability.md)：多 Code Agent 观测模型、协议边界、运行时与分阶段方案。
- [Preview Provider 扩展](design/preview-providers.md)：文件预览扩展点、内容安全契约与适用边界。

## 测试

- [项目测试卡点](testing/test-gates.md)：Files、Git Changes、Search/Preview、终端交互与未来 Agent 功能的项目级 UT、集成测试和 E2E 门禁。
- [Code Agent 可观测性测试卡点](testing/code-agent-observability-test-gates.md)：Code Agent synthetic contract、headless E2E 与 PTY E2E 专项补充。

## 集成

- [Claude Code Hooks 集成](integrations/claude-code-hooks.md)：Claude Code command Hooks 的用户级 Setup、项目级手工配置、exact-workspace 语义与能力边界。
- [OpenCode 插件集成](integrations/opencode-plugins.md)：OpenCode 本地插件的安装、native event 映射、exact-workspace 语义与数据边界。
- [TraeX Hooks 集成](integrations/traex-hooks.md)：TraeX command Hooks 的用户级与项目级配置、事件映射、数据边界与真实 CLI canary。
- [Code Agent Hooks 安装与恢复](integrations/hook-setup.md)：用户级 Setup、安装器交互、事务备份、失败回滚与显式恢复。

## 工程

- [内容搜索性能检查](engineering/search-performance.md)：搜索索引生命周期、Refresh 语义与本地性能检查方法。

产品行为、安装、快捷键和工程命令见 [项目说明](../README.md)。实现行为最终以当前 Rust 源码和已执行测试为准；设计稿中尚未落地的内容必须明确标注状态。
