# Preview Provider 扩展

Latte Lens 将文件选择和渲染与格式提取分离。`PreviewRegistry` 向已注册的
`PreviewProvider` 请求内容，再由现有内容面板负责滚动、标题和回退提示。默认 Registry
包含文本/源码 Provider，以及 PNG、JPEG、GIF、WebP、PDF、DOCX 的有界 common-file
Provider；调用方后注册的 Provider 仍可覆盖任一内置格式。

## 契约

Provider 必须遵守以下规则：

1. 不支持某个文件时返回 `Ok(None)`。
2. 能将文件渲染为终端内容时返回 `PreviewContent`；默认 `PreviewKind::Text`，内置图片
   Provider 使用类型化 `PreviewKind::Image(format)`，App 不得从扩展名或显示文本反推类型。
3. 遵守 `request.max_bytes` 和 `request.max_lines` 上限。
4. 输出被截断时设置 `truncated`。
5. 可以通过 `PreviewContent::with_highlights` 附加语义化的
   `HighlightSpan` 字节范围；外层向量必须与 `lines` 一一对应。
6. 不得修改选中文件或其仓库。
7. 必须使用 `request.open_regular()` 读取文件字节，不得直接重新打开
   `request.absolute_path`。
8. 只有用户确认的终端图片 fallback 才会设置 `request.terminal_image_size()`；第三方
   Provider 可以忽略它，但不得据此绕过 bytes/lines 或分配预算。

Registry 按注册顺序的逆序查询 Provider。专用 Provider 应注册在内置实现之后，
以便优先处理对应格式。

Registry 有两种符号链接策略，由请求上的 `following_symlinks` 决定：

- **交互式 All Files（`following_symlinks(true)`）**：把工作区当作文件系统视图，
  跟随符号链接。最终组件是文件软链（或经由目录软链到达的普通文件，例如
  `linked-repo/src/main.rs`）时，Registry 解析到 target 后按普通文件走 Provider
  分发；最终组件是目录软链或特殊文件时仍拒绝，交由调用方展开或提示。目录软链
  在文件树里是可展开目录，但初始扫描不会自动递归它（避免环），仅在展开时懒加载。
- **仓库/依赖等其他读取（默认 no-follow）**：Registry 使用不跟随链接的 metadata
  检查选中工作区下的每个路径组件。最终组件是符号链接时，直接以 `symlink`
  provider id 返回有界的 target 路径文本，不打开 target，也不把链接交给 Provider；
  中间路径符号链接仍被拒绝。Git Changes 因此把被追踪的软链渲染为其 target 路径，
  永远不读取 target 的字节。

两种策略下，FIFO、socket、device、目录（在 no-follow 分支）和 Windows reparse
point 都不会交给 Provider。

## 内置常见文件 Provider

内置 `common-file` Provider 不信任扩展名：它从 `open_regular()` 的安全句柄读取 magic
和当前字节，真实格式优先；扩展名与内容冲突时显示 mismatch，不把伪装内容回退给文本
Provider。输入固定限制为 32 MiB，输出继续受 `request.max_bytes` 和
`request.max_lines` 约束。所有 metadata、提取文本和 parser error 都经过终端控制字符
净化；解析在现有后台 preview worker 运行，并以进程随机内容摘要、输出 limits 和可选
terminal viewport 为 key 使用小型 LRU 缓存。

- 图片默认只从安全句柄读取 header 并输出 metadata；`o` 在后台重新校验当前文件后调用
  shell-free 系统默认查看器。没有桌面/opener 失败时先提示，只有用户再次按 `i` 才按
  Content 面板尺寸生成有硬上限的 TrueColor 半块像素；GIF/WebP 只取首帧；
- PDF 先给出准确页数和 metadata，再按页提取全部文本；只有 safety/output budget 可以
  提前终止，届时必须显示 `Parsed X / N` 并标记 truncated；扫描件明确提示不做 OCR；
- DOCX 只读取 content types、core properties 和 main document XML，提取标题、段落、
  列表与表格；不展开媒体，不访问 relationship；macro-enabled 内容直接拒绝。

图片尺寸/像素/decoder allocation，PDF 页/对象/stream，ZIP entry/展开体积和 XML
depth/event 均有独立上限。PDF action、附件和 URI 不执行，DOCX macro、external
relationship、`DOCTYPE`/entity 不执行。解析不调用 shell、Office、浏览器、网络或临时
解压目录。系统打开是 App/Runtime 层的独立、显式动作，不是 `PreviewProvider` authority；
它面向所有通过分类的普通文件，macOS/Linux 以参数方式调用系统 opener，Windows 调用
`ShellExecuteW`，不拼接命令。Provider 可以提供内部 typed evidence，但不能自行启动宿主
应用。完整解析预算与威胁模型见[常见文件预览 V1 实现设计](./common-file-previews.md)，
统一交互、主动类型阻止、未知格式确认和跨平台 opener 契约见
[统一文件激活与系统打开 V1 设计](./external-file-open.md)。

`PreviewRequest::open_regular` 会重复上述检查后打开文件：no-follow 请求以 no-follow
语义打开最终组件；following 请求先把最终软链 canonical 化到其 target（该 target 已
不含链接，`O_NOFOLLOW` 仍成立），再交给同一道闸门。两条路径都验证句柄仍指向普通
文件、检查 canonical 位置、在 Unix 上使用非阻塞打开避免 FIFO 卡住 worker，并在
Windows 上打开 reparse point 本身而不是其目标。

这里有一个有意保留的兼容性边界：可选的第三方 Provider 可能忽略
`open_regular()`，并把 `absolute_path` 交给会重新打开路径的库。特殊文件仍不会被
dispatch，但 Latte Lens 无法消除该 Provider 后续从 dispatch 到 open 之间的竞争，也
无法取消任意阻塞代码。安全要求严格的 Provider 必须直接消费 `PreviewFile`，或者为其
子进程、第三方库提供等价的有界 I/O 契约。在 Unix、Windows 以外的平台，标准库回退
实现会执行 metadata 与 canonical 边界检查，但无法让最终打开具备原子的 no-follow
语义，也无法验证可移植的文件 identity。Latte Lens 的发布 CI 和安装包当前覆盖
Linux、macOS 与 Windows。

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

## 自定义扩展策略

- PDF/Word：覆盖内置文本提取，提供经过同等安全审计的替代渲染；
- 图片：增加 OCR 文本或未来的 Kitty/Sixel/iTerm capability adapter；
- 压缩包：只列出条目，不解压到仓库中。

Provider API 中的普通文本和语义高亮范围与终端无关；内置图片 fallback 使用专用
`ImagePixel` highlight，RGB 前景/背景只作用于对应 `▀` glyph，Ratatui 样式仍只在 UI 层
应用。未来可以扩展 typed preview payload 增加原生图片或结构化页面，同时保留 Registry
和 App 集成点。

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
