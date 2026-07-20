# Preview Provider 扩展

Latte Lens 将文件选择和渲染与格式提取分离。`PreviewRegistry` 向已注册的
`PreviewProvider` 请求内容，再由现有内容面板负责滚动、标题和回退提示。

## 契约

Provider 必须遵守以下规则：

1. 不支持某个文件时返回 `Ok(None)`。
2. 能将文件渲染为终端文本时返回 `PreviewContent`。
3. 遵守 `request.max_bytes` 和 `request.max_lines` 上限。
4. 输出被截断时设置 `truncated`。
5. 可以通过 `PreviewContent::with_highlights` 附加语义化的
   `HighlightSpan` 字节范围；外层向量必须与 `lines` 一一对应。
6. 不得修改选中文件或其仓库。
7. 必须使用 `request.open_regular()` 读取文件字节，不得直接重新打开
   `request.absolute_path`。

Registry 按注册顺序的逆序查询 Provider。专用 Provider 应注册在内置实现之后，
以便优先处理对应格式。Registry 使用不跟随链接的 metadata 检查选中工作区下的每个
路径组件。最终组件是符号链接时，Registry 直接以 `symlink` provider id 返回有界的
target 路径文本，不打开 target，也不把链接交给 Provider；中间路径符号链接仍被拒绝。
FIFO、socket、device、目录和 Windows reparse point 同样不会交给 Provider。

`PreviewRequest::open_regular` 会重复上述检查，以 no-follow 语义打开最终组件，
验证句柄仍指向同一个普通文件，并在返回可读、可 seek 的 `PreviewFile` 前检查其
canonical 位置。在 Unix 上还会使用非阻塞打开，避免竞争产生的 FIFO 卡住 worker；
在 Windows 上打开 reparse point 本身而不是其目标。

这里有一个有意保留的兼容性边界：可选的第三方 Provider 可能忽略
`open_regular()`，并把 `absolute_path` 交给会重新打开路径的库。符号链接和特殊文件
仍不会被 dispatch，但 Latte Lens 无法消除该 Provider 后续从 dispatch 到 open 之间的
竞争，也无法取消任意阻塞代码。安全要求严格的 Provider 必须直接消费
`PreviewFile`，或者为其子进程、第三方库提供等价的 no-follow 和有界 I/O 契约。
在 Unix、Windows 以外的平台，标准库回退实现会执行不跟随链接的 metadata 与
canonical 边界检查，但无法让最终打开具备原子的 no-follow 语义，也无法验证可移植的
文件 identity。Latte Lens 的发布 CI 和安装包当前覆盖 Linux、macOS 与 Windows。

## 最小实现

```rust
use anyhow::Result;
use latte_lens::preview::{
    PreviewContent, PreviewProvider, PreviewRegistry, PreviewRequest,
};

struct PdfPreviewProvider;

impl PreviewProvider for PdfPreviewProvider {
    fn id(&self) -> &'static str {
        "pdf"
    }

    fn preview(
        &self,
        request: &PreviewRequest<'_>,
    ) -> Result<Option<PreviewContent>> {
        let is_pdf = request
            .absolute_path
            .extension()
            .and_then(|extension| extension.to_str())
            == Some("pdf");
        if !is_pdf {
            return Ok(None);
        }

        // 这里可替换为 PDF 库或有界的外部命令适配器；输出必须遵守
        // max_bytes 和 max_lines。
        let lines = vec![format!("PDF 预览：{}", request.display_path.display())];
        Ok(Some(PreviewContent::new(lines)))
    }
}

let mut registry = PreviewRegistry::with_builtins();
registry.register(PdfPreviewProvider);
```

创建 App 时传入 Registry：

```rust,ignore
let app = App::with_preview_registry(repository_path, registry)?;
```

如果 App 构造完成后才添加 Provider，调用
`app.register_preview_provider(provider)`。当前选择会通过后台 worker 刷新：请求立即
提交，结果异步应用。

## 适用的扩展策略

- PDF：封装库或有界的 `pdftotext` 子进程。
- Word：通过 OOXML 库或有界转换器提取段落。
- 图片：生成 metadata、OCR 文本或未来的终端图片 payload。
- 压缩包：只列出条目，不解压到仓库中。

Provider API 中的文本和语义高亮范围与终端无关；Ratatui 样式只在 UI 层应用。
未来可以通过 preview payload enum 增加图片或结构化页面，同时保留 Registry 和 App
集成点。

## 与 Code Agent 可观测性的边界

`PreviewProvider` 是安全的文件内容扩展点，不是 Code Agent runtime、事件或远程服务
适配器。Agent session、activity、Provider snapshot、event stream 和 explain 诊断使用
[Code Agent 可观测性设计](./code-agent-observability.md) 中的独立契约；不得通过注册
live 或 network-backed `PreviewProvider` 绕过本文的文件契约。

只有当 Lens 将 observed artifact 解析为选中工作区内的 `RepoPath`，并通过常规的
no-follow 普通文件检查后，该 artifact 才能进入现有 preview pipeline。Opaque
artifact、URL、远程资源、终端文本、transcript 和 Provider 响应仍然只能作为
metadata，除非未来显式设计新的有界内容契约。可观测性层可以复用
registry/factory/priority 的组织模式，但不能复用 `PreviewRequest`、`PreviewFile` 或
Provider 的文件安全 authority。
