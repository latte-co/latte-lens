# 常见文件预览 V1 实现设计

状态：Implemented and locally validated（2026-07-22）
目标分支：`feat/file-preview-providers`
基线：`main@bedf9e1`

后续已批准的[统一文件激活与系统打开 V1 设计](./external-file-open.md)会把当前仅用于图片的
系统查看器入口提升为 App/Runtime 级通用动作。本文记录 common-file provider 与当前图片
fallback 的已实现基线；外部打开的下一阶段交互和安全契约以新设计为准。

## 1. 背景与目标

Latte Lens 已经通过 `PreviewRegistry` / `PreviewProvider` 将文件选择、内容安全、
格式提取与 Content 面板渲染分离。当前内置实现只接受 UTF-8 文本；二进制图片、
PDF 和 Word 文档会落入 unsupported 提示。

V1 新增一个内置的 `CommonFilePreviewProvider`，支持：

- 图片：PNG、JPEG、GIF、WebP；
- 文档：PDF、DOCX。

PDF/DOCX 继续输出有界文本。图片默认只输出 metadata 和显式操作提示；用户主动按 `o`
时，桌面环境使用系统默认查看器。无桌面环境不静默降级，而是提示用户再次按 `i`，随后
才在 Content 面板生成按当前面板尺寸缩放的 TrueColor 半块像素。所有解析和终端图像生成
继续运行在后台 worker，并沿用 generation / stale-result rejection。

V1 不引入 Kitty、Sixel 或 iTerm 私有图像协议，不执行 OCR，不使用 shell 拼接命令，也不
把解析交给网络服务。原生终端图像协议可以在后续能力探测层中加入，不改变本次确认流程。

## 2. 用户可见行为

### 2.1 图片

- 首次选择 PNG、JPEG、GIF、WebP 时只读取真实内容的 header，显示格式、尺寸、颜色类型、
  文件大小和 `Press o to open with the system default app.`；不自动完整解码；
- 用户按 `o` 后，后台再次经过安全句柄、magic 和格式校验，再调用宿主系统默认查看器；
- macOS 调用 `open -- <absolute-path>`，桌面 Linux 调用 `xdg-open <absolute-path>`，
  Windows 调用 `ShellExecuteW`；参数直接交给进程/API，不经过 shell；
- Linux 没有 `DISPLAY`/`WAYLAND_DISPLAY` 等桌面会话，或 opener 不存在/启动失败时，不自动
  渲染图片，而是显示 `按 i 在终端内预览（TrueColor，效果取决于终端），Esc 取消`；
- 只有用户在该提示状态下再次按 `i`，后台才完整解码第一帧，并按当前 Content 面板宽高
  保持比例缩小；每个 `▀` 单元以前景表示上像素、背景表示下像素；
- `Esc` 取消终端预览并恢复 metadata；选择其他文件、切换 scope 或打开搜索都会取消提示；
- GIF/WebP 只预览第一帧；只有能用有界、低成本方式确认时才显示动画信息；
- 关闭行号、folding 和 LSP navigation。

终端渲染不是系统查看器的无提示替代品。若终端无法确认/正确呈现 24-bit color，提示中必须
说明效果取决于终端；若明确为 `TERM=dumb`，则拒绝生成图片，只保留 metadata。V1 不再
提供灰阶 ASCII 图片，因为它丢失颜色和细节，不能被描述为正确图片预览。

### 2.2 PDF

- 先读取页树并显示准确总页数和可安全取得的 Title、Author、Subject、Keywords；
- 正常 PDF 按文档顺序提取全部页面文本，不设置“只预览前 N 页”的产品限制；
- 每页以 `Page X / N` 分隔；
- 单页没有文本层时显示 `[No extractable text on this page]`；
- 只有全部页面都没有文本层时，额外提示文档可能是扫描件且 V1 不提供 OCR；
- 混合文本/扫描 PDF 保留所有可提取文本，并逐页诚实标记空缺；
- 加密、损坏文档返回静态错误，超预算文档返回 partial 信息；主动内容只显示安全警告并
  提取静态文本，不执行任何 action。

安全预算触发时可以停止在 `X / N` 页，但必须同时设置 `truncated` 并显示原因；
不得静默把“前几页”冒充完整预览。

### 2.3 DOCX

- 读取标准 OOXML DOCX container；
- 按文档顺序提取标题、段落、列表、换行和表格文本；
- 表格单元格以 ` | ` 分隔，列表项使用稳定的文本前缀；
- 显示 core properties 中可安全取得的 Title、Creator、Subject、Created、Modified；
- 不还原字体、页面布局、内嵌图片、页眉页脚、批注或修订记录；
- `.docm` 和 macro-enabled content type 明确拒绝；external relationship 永不访问。

### 2.4 环境能力与交互状态机

环境判断以实际能力为准，不把 `Linux == headless` 或 `macOS/Windows == desktop` 写死：

```text
image metadata
  -- o --> background revalidation
              | desktop opener succeeds --> keep metadata + transient success status
              | no desktop / opener failure --> explicit terminal-preview prompt
                                                   | i --> bounded TrueColor render
                                                   | Esc / selection change --> metadata/cancel
```

- Linux 桌面能力至少要求非空 `DISPLAY` 或 `WAYLAND_DISPLAY`，避免 SSH/容器中的 `xdg-open`
  错误选择文本浏览器或长时间失败；X11/Wayland forwarding 仍可正常命中；
- macOS/Windows 仍以实际 API 启动结果为准；启动错误进入同一提示状态；
- `SSH_CONNECTION` 只作为诊断信息，不能单独决定 headless，因为远程会话可能有 X11 forwarding；
- opener 和图片解码都不在 event handler 或 render 中执行；它们使用有界后台请求；
- 同一时刻只保留最新 external/terminal preview generation，过期结果不得覆盖新选择。

## 3. 非目标

- OCR、扫描页识别；
- SVG、HEIC、TIFF、BMP、PPTX、XLSX、ODT；
- PDF/DOCX 原版式或分页渲染；
- GIF/WebP 多帧播放；
- Kitty/Sixel/iTerm 等终端私有图片协议及自动探测；
- 打开内嵌附件、URI、外部 relationship 或远程资源；
- 自动打开系统应用、无确认的终端降级、脚本引擎、外部转换器或临时解压目录；
- 本 provider 自行启动系统应用。统一系统打开由 App/Runtime 层负责，并在
  [统一文件激活与系统打开 V1 设计](./external-file-open.md)中单独定义；当前已实现代码在该
  方案落地前仍只允许已验证图片使用外部打开。

## 4. 威胁模型

输入文件、扩展名、metadata、压缩条目、XML、PDF 对象和提取文本全部不可信。
V1 必须抵御以下类别：

| 威胁 | 约束 |
| --- | --- |
| 脚本/可执行文件伪装成 `.pdf`/`.docx`/图片 | 扩展名只表示 claimed format；必须校验 magic/container。未知内容返回格式不匹配，不交给解释器。 |
| polyglot | PDF 必须从 byte 0 开始为 `%PDF-`；DOCX ZIP offset 必须为 0；图片必须由启用 decoder 的 magic 命中。 |
| PDF JavaScript、Launch、URI、表单、附件 | `lopdf` 只读取对象、页树、metadata 和 content stream；代码不遍历/执行 action，不打开附件或 URL。 |
| DOCX 宏、外链、XXE | 只接受非 macro DOCX content type；不读取 relationship target；遇到 `DOCTYPE` 直接拒绝；不启用 entity resolver。 |
| Zip Slip / 临时文件覆盖 | 不调用 extract，不创建输出路径；条目名只用于精确匹配已知 OOXML part。 |
| ZIP/PDF/image 解压炸弹 | 解压/尺寸/对象/条目/分配预算在高成本解析前检查，并使用库提供的解压上限。 |
| 路径替换、symlink、FIFO/device | Provider 只能消费 `PreviewRequest::open_regular()` 返回的句柄；Registry 的路径安全策略保持权威。 |
| ANSI/OSC/终端注入 | 所有外部字符串统一经过 `sanitize_terminal_text`，替换 C0/C1、ESC、DEL、bidi 控制符与不可见方向标记。 |
| CPU/内存 DoS | 文件、像素、页、对象、条目、XML event、输出和 cooperative deadline 均有显式预算。 |
| 缓存陈旧或碰撞 | 缓存 key 包含进程随机 SipHash、输入长度、格式、路径和输出 limits；全量预览哈希当前字节，metadata-only 图片只哈希决定输出的有界 header/probe。 |
| 系统 opener 注入 | 只接受当前成功图片 Preview 绑定的 `ContentTarget`；打开前在 worker 中重新通过安全句柄与 magic 校验；只传绝对路径参数，永不使用 `sh -c`、`cmd /C start` 或字符串命令。 |
| opener TOCTOU | 校验后立即解析 final path 并启动；若对象已变成 link/special file 或格式不再是图片则拒绝。系统应用打开原文件仍存在宿主 OS 的最终路径竞争边界，状态中不得宣称提供进程级沙箱。 |
| 终端颜色污染 | RGB 背景只作用于明确确认后的图片 glyph，不设置全局/面板背景；离开图片内容后 Ratatui 正常重绘清除。 |

解析错误必须变成有界的 Preview 错误行；不得把原始 parser diagnostic、控制字符或
未截断 metadata 直接写入终端。

## 5. 格式识别

Provider 首先从安全句柄读取最多 1 KiB probe：

- PDF：byte 0 开始为 `%PDF-`；
- ZIP/DOCX：标准 ZIP local/empty/spanned magic，且 archive offset 为 0；
- 图片：`image::guess_format` 命中启用的 PNG/JPEG/GIF/WebP decoder。

随后采用“内容优先、claimed format 负责报错”的规则：

1. 内容命中支持格式，即使扩展名错误，也按实际内容预览，并显示 extension mismatch；
2. 扩展名声称支持格式但内容未知，返回 `Format mismatch`，不向 text provider 回退；
3. 内容和扩展名都未知，返回 `Ok(None)`，允许后续 provider 处理；
4. ZIP 只有在 `[Content_Types].xml`、`word/document.xml` 和 main content type 校验通过后
   才能识别为 DOCX；普通 ZIP 不属于 V1。

## 6. 预算

预算是安全常量而不是用户配置。后续若要扩大，必须用 benchmark 和攻击 fixture 证明。

| 预算 | V1 值 | 处理方式 |
| --- | ---: | --- |
| binary input | 32 MiB | 在分配/读取前拒绝；显示实际大小与上限 |
| probe | 1 KiB | 未命中且无 claimed format 时立即 decline |
| output | `request.max_bytes` / `max_lines` | `BoundedPreview` 统一计数并设置 truncated |
| cooperative parse deadline | 5 s | 在页、entry、XML event 批次之间检查并 partial |
| image width/height | 16,384 | 解码前用 header dimensions 拒绝 |
| image pixels | 24,000,000 | checked multiply；超限拒绝 |
| image decoder allocation | 128 MiB | 配置 `image::Limits::max_alloc` |
| terminal image viewport | 当前 Content 面板，硬上限 160 x 80 cells | 一格表示纵向两个像素；只缓存最终 glyph/highlight，不缓存原始 DynamicImage |
| PDF pages | 10,000 safety ceiling | 超限拒绝；正常文件仍全页解析 |
| PDF objects | 200,000 | load 后、逐页提取前拒绝 |
| PDF stream decompression | 16 MiB per stream/page | `LoadOptions::max_decompressed_size` + `extract_text_with_limit` |
| ZIP entries | 4,096 | central directory 读取后拒绝 |
| ZIP single entry | 16 MiB | 读取前检查 uncompressed size |
| ZIP total uncompressed | 64 MiB | checked sum，读取前拒绝 |
| XML depth | 128 | start/end event 维护深度 |
| XML events | 1,000,000 | 每批检查 deadline |
| preview cache | 8 entries / 2 MiB output | LRU，registry 更新时自然替换 provider/cache |

`image::Limits::max_alloc` 并非所有 decoder 都严格支持，因此尺寸、像素和输入上限仍是
独立前置门。5 秒 deadline 是 cooperative：第三方 parser 单次调用不能被 Rust 安全地
强制终止，因此选择具备内部解压上限的 API，并用结构预算缩小单次调用的最坏输入。

## 7. 依赖与兼容性

所有解析依赖关闭不需要的默认 feature，保持 MSRV 1.88 和跨平台路径：

```toml
image = { version = "0.25.10", default-features = false, features = ["gif", "jpeg", "png", "webp"] }
lopdf = { version = "0.44.0", default-features = false }
quick-xml = "0.41.0"
zip = { version = "=8.6.0", default-features = false, features = ["deflate-flate2-zlib-rs"] }
```

选择依据：

- `image` 声明 Rust 1.88，提供 magic detection、dimensions、decoder limits；
- `lopdf` 声明 Rust 1.88，提供页树、metadata、逐页文本提取，以及加载/页面 content stream
  的 decompression limit；
- `zip` 8.6.0 声明 Rust 1.88，可在解压前读取 entry count、compressed/uncompressed size、
  encryption 和 archive offset；
- `quick-xml` 声明 Rust 1.79，不默认解析外部实体，并能显式拒绝 `DOCTYPE`。

不启用 `image` 的 AVIF/TIFF/EXR/rayon，不启用 `lopdf` 的默认 rayon/time/image，
不启用 ZIP 的 encryption、bzip2、lzma、zstd 等 V1 不需要的格式。

系统打开不新增 crate：macOS/Linux 使用 `std::process::Command` 的参数接口；Windows 使用
现有精确固定版本 `windows-sys` 的 `Win32_UI_Shell` / `Win32_UI_WindowsAndMessaging`
feature 调用 `ShellExecuteW`。这三条路径都不经过 shell 文本解析。

## 8. 代码结构

```text
src/preview.rs                  公开契约、Registry、built-in 注册
src/preview/common.rs           probe、预算、bounded output、terminal sanitization、LRU
src/preview/common_files.rs     CommonFilePreviewProvider 与格式路由
src/preview/image_preview.rs    图片 metadata、limits、首帧、TrueColor 半块图
src/preview/pdf_preview.rs      PDF load limits、metadata、全页文本
src/preview/docx_preview.rs     OOXML container、core properties、document XML
src/system_preview.rs           桌面能力判断与 macOS/Linux/Windows 默认查看器边界
src/runtime.rs                  external-open queue、terminal viewport 请求与 stale result gate
src/app.rs                      `o`/提示/`i`/`Esc` 状态机
src/ui.rs                       局部 RGB half-block span 渲染
```

`CommonFilePreviewProvider` 注册在 built-in text provider 之后。Registry 逆序查询，
因此 binary provider 先检查真实格式；未知普通 UTF-8 仍由 text provider 处理。

Provider 内部流程：

```text
open_regular -> probe/claimed format -> input-size gate -> bounded read
  -> keyed cache lookup -> format-specific parse -> sanitize/bounded output
  -> cache insert -> PreviewContent
```

`PreviewContent` / `Preview` 增加类型化 `PreviewKind::Image`，避免 App 解析扩展名或文本行来
决定是否可打开；`PreviewRequest` 增加可选且有上限的 terminal image viewport。自定义 provider
默认仍为 `PreviewKind::Text`，不受影响。`HighlightKind` 增加 RGB image-cell 变体；该变体只
允许内置图片 provider 产生。格式特定 provider id 暂统一为 `common-file`。

## 9. 性能与缓存

- 只有当前选择触发 header/metadata；不扫描、预热或自动完整解码整个 workspace；
- runtime 现有 dedicated preview worker 继续承担所有 I/O 和解析；UI/render 不增加 I/O；
- image metadata 只读 header；完整解码只发生在用户确认的 terminal render；先读 dimensions，
  再受限解码和缩小；GIF/WebP 不遍历全部帧；
- external open 在同一后台 worker 中重新校验当前文件；event handler 只提交请求；
- macOS/Linux opener 启动后在后台最多观察 1 秒：期间非零退出进入明确提示；超过观察窗的
  长生命周期进程按已交接处理并由独立 reaper 回收。该等待不阻塞 UI；
- PDF 主动内容对象扫描和每页文本提取共用 cooperative budget；每页调用有 decompression
  limit 的文本提取，页间检查 output/deadline；
- DOCX 检测完成后复用同一个已 preflight 的 ZIP archive 进入正文预览，不重复打开或检查
  container；只读取 `[Content_Types].xml`、`docProps/core.xml`、`word/document.xml`，不解压媒体；
- 全量预览从安全句柄读取当前字节并计算进程随机 SipHash；metadata-only 图片缓存有界 header
  解析结果，动画 marker 最多扫描前 64 KiB。命中 8-entry/2-MiB LRU 后跳过 parser/decode；
  缓存只保存最终 bounded `PreviewContent`，不保存原始图片/PDF/ZIP；
- registry 被替换时旧 provider 与 cache 一起释放；limits 是 cache key 的一部分；
- output weight 按 UTF-8 line bytes 与 highlight 数量保守估算。

性能验收记录 cold parse、warm cache、输入字节、输出字节和 wall time。测试不使用脆弱的
毫秒级绝对断言；确定性预算测试负责阻止无界行为，benchmark 用于调参和回归观察。

## 10. 错误与 partial 语义

- unsupported：内容和 claimed format 都未知，`Ok(None)`；
- mismatch：返回 `common-file` Preview，明确实际/claimed 格式，不展示伪装内容；
- corrupt/encrypted/active/macro：返回静态错误行，不回退到 text；
- budget exceeded：返回 metadata + 原因，`truncated = true`；
- external opener unavailable/failed：保留失败原因的净化摘要并进入显式 terminal prompt；
- terminal prompt：只有 `i` 确认才解码；`Esc` 或内容 identity 改变立即失效；
- terminal render 超预算/损坏：回到 metadata + 有界错误，不自动尝试 ASCII；
- PDF 页级解析错误：标记该页错误并继续后续页；若 deadline/output budget 触发则停止，显示
  `Parsed X / N pages`；
- DOCX 可选 metadata part 错误不阻塞正文；main document/content type 错误阻塞；
- 所有 parser error 先净化并限制到单行 512 bytes。

## 11. 测试矩阵

### Unit

- magic 与扩展名组合：正确、错误扩展名、伪装脚本、支持格式互相改名、polyglot 前缀；
- sanitization：ESC、OSC、C0/C1、DEL、bidi isolate/override、超长错误；
- bounded output、deadline、checked arithmetic、cache 命中/淘汰/limits 隔离；
- 图片：四种格式 metadata 不含 ASCII、透明上下像素、面板尺寸比例、viewport hard cap、
  超宽高/像素/分配、损坏、首帧；
- opener：Linux desktop/headless 纯判断、路径参数不经过 shell、打开前格式变化/伪装脚本拒绝、
  unavailable/failed outcome；Windows/macOS cfg 路径至少通过 locked check；
- PDF：多页、metadata、混合空白页、全部无文本、stream limit、对象/页/输出预算、主动 action；
- DOCX：标题/段落/列表/表格/core properties、错误 content type、`.docm`、external rel、
  `DOCTYPE`、encrypted entry、Zip Slip 名称、entry/size/depth/event budget。

测试 fixture 必须在测试内用库或最小确定性 bytes 生成，不提交大二进制样本。

### App integration

- built-in registry 自动接受 PNG、PDF、DOCX；
- binary mismatch 不落入 text provider；
- `content_provider == "common-file"`，无行号/folding/navigation；
- 图片 snapshot 带类型化 `PreviewKind::Image`；普通文本/PDF/DOCX 不获得图片打开能力；
- `o` 成功保留 metadata；headless/失败进入提示；非提示状态的 `i` 不解码；提示状态 `Esc`
  恢复 metadata，`i` 用当前 Content 面板尺寸提交后台请求；
- RGB half-block 在 TestBackend 中验证 foreground/background，不把颜色泄漏到相邻 cell；
- 真实 PTY 使用隔离的失败 opener 覆盖 metadata -> `o` -> 明确提示 -> `Esc`/`i` 两条分支，
  且不得启动宿主桌面应用；
- 快速切换后旧 preview 不覆盖新选择；
- All Files 的 follow-final-symlink 与 Git Changes no-follow 语义保持现状。

### Gates

1. format-specific unit tests；
2. `make fmt-check`、`make check`、`make lint`、`make test`；
3. `make ci`；
4. `make coverage`（生产逻辑变化，必须执行）。

## 12. 文档与交付边界

- 更新 `docs/design/preview-providers.md`：built-in 与 output/input 预算语义；
- 更新 `docs/testing/test-gates.md`：二进制 preview 的阻塞用例；
- 更新 `README.md`：支持格式、限制和安全行为；
- 不自动 commit、push 或创建 PR；这些外部动作需用户另行明确授权。

V1 完成必须同时满足：六类格式行为、全页 PDF 语义、安全测试和性能预算。`make ci` /
`make coverage` 若被非本功能门禁阻塞，必须保留首轮失败、隔离复跑和组件级结果，不能把
组件级通过改写为聚合门禁全绿。只完成解析 happy path 或只写设计文档均不算完成。

## 13. 本地验证记录

验证日期：2026-07-22；worktree：`latte-lens/feat-file-preview-providers`。

以下记录属于灰阶 ASCII 方案，已因截图验收失败而作废，只保留为历史证据；新方案必须重新
执行全部门禁，不能沿用这些结果宣称完成：

- `make ci`：通过；包含 format/check/clippy、全量 Rust/integration、脚本、27 个
  production PTY 场景、Agent TUI 与 package-negative 门禁；
- preview 聚焦单测：38/38；App common-file 集成覆盖 PNG、全页 PDF、DOCX 和伪装脚本；
- `coverage-unit`：93.06% lines（门槛 93%）；
- `coverage-e2e`：85.57% lines（门槛 85%），27/27 PTY、10/10 CLI、1/1 Agent E2E；
- `coverage-agent`：86.59% lines（门槛 80%）。

首次 `coverage-e2e` 的 PTY 27/27 已通过，但随后既有 OpenCode live-event CLI 用例发生
一次 2 秒接收超时；同一门禁隔离复跑时 CLI 10/10 和完整 coverage 均通过。该波动不在
common-file 解析路径，仍在交付说明中保留，避免把一次重试描述成首轮全绿。

### 修订后的统一系统打开 / TrueColor 方案

- `make fmt-check`、`make check`、`make lint`：通过；Clippy 使用 `-D warnings`；
- 串行完整 Rust suites：327 lib tests passed / 1 ignored，97/97 App TUI integration、10/10
  CLI，以及 Git/tree/repository/LSP/Agent contract suites 均通过；
- `coverage-unit`：93.24% lines（门槛 93%）；`system_preview.rs` 的常见被动格式、主动后缀、
  executable signature、unknown confirmation 和 disabled adapter 已纳入 Q1 分母；
- `coverage-e2e`：85.50% lines（门槛 85%）；29/29 production PTY、10/10 CLI、1/1
  Agent TUI。`unified-system-open` 为 20 条断言，包含 Tree 双击打开解析验证后的被动 DOCX；
  另有 `image-preview-fallback` 11 条；
- `coverage-agent`：86.47% lines（门槛 80%）；三个独立 coverage target 均通过；
- `make script-test`：34 tests、4 skipped；`make e2e`、`make agent-e2e-tui`、
  `make agent-package-negative` 均通过，默认包不含 synthetic harness；
- `make install`：通过；`/Users/bytedance/.cargo/bin/latte-lens` 报告 `0.1.0`，与本 worktree
  `target/release/latte-lens` 的 SHA-256 均为
  `5cdf9cf127bff38851c7a0e1d0777eabe5c73310e8e5b57cd733d9a3286f3f2e`；
- 真实二进制场景使用隔离 stub opener，验证 PDF `o`、clickable `[Open]` dedupe、unknown
  二次确认、`Enter` 不得确认、脚本调用计数为 0，以及图片失败后的显式 `i` / `Esc`；
- PR review 修订覆盖：动画 marker 只扫描前 64 KiB、metadata-only 图片进入 LRU、DOCX
  inspection/archive 复用、OOXML content type 使用有界 XML parser、PDF active-object scan 共用
  cooperative budget；`.js` 继续按已批准的脚本阻止策略处理并有显式测试；
- All Files 与 Git Changes 的目录/容器行恢复为任意位置单击展开/折叠；文件仍是单击选择、
  双击系统打开。App integration 与两轮 29/29 production PTY 均覆盖该交互；
- opener 继续在后台最多观察 1 秒，用于区分快速非零退出与成功 handoff；不会在 UI thread
  读取文件、分类或等待进程；
- Windows/Linux native build 本轮未在对应宿主/交叉工具链上执行；本机完成 macOS
  `all-targets/all-features/locked`，Linux headless/desktop 与 Windows adapter 由确定性边界测试
  覆盖。该记录不把 adapter unit test 表述为对应平台 native CI。

聚合门禁的历史波动与最终结果：

- 本次 review 修订完成后的最终原样 `make ci` 复跑以 exit 0 完整通过；同一次运行包含
  fmt/check/clippy、并行全量 Rust/integration、34 个 script tests（4 skipped）、29/29
  production PTY、1/1 Agent TUI 与 package-negative 门禁；

- 原样并行 `make ci` 首轮暴露并修复了 Tree footer 对 `Ctrl+C` / scope 提示和既有 E2E
  单击折叠假设的回归；修复后的相关用例与完整 PTY 已通过；
- 后续原样 `make ci` 在高负载下仍出现既有 FIFO 3 秒墙钟用例，以及 Claude/Codex/OpenCode/
  TraeX 5 ms live-ACK 的 `Idle` 波动；这些用例隔离或串行完整 suite 通过；
- 本次 review 修订的并行 `make test` 仅出现一次既有 `quickly_failing_opener...` 1 秒调度
  时序失败；该项隔离复跑通过，随后串行全量 Rust suite 完整通过；
- 串行 `make ci` 在运行约 4 分钟后仍有一次 Claude live-ACK `Idle`，因此不声明聚合命令全绿；
  其组成的 fmt/check/lint、完整 Rust suites、script、29/29 PTY、Agent TUI 和 package-negative
  已分别完成并通过；
- `make coverage` 首轮因新增分类器把 unit coverage 拉到 92.55% 而失败；补充真实分支测试后，
  `coverage-unit` / `coverage-e2e` / `coverage-agent` 三个原始门槛 target 分别通过。本次 review
  修订再次运行聚合 `make coverage`，三个 target 一次完成并通过；未把历史失败改写成首轮全绿。
- PR review 修订首次远端 Linux `coverage-e2e` 的 29/29 场景全部通过，但 line coverage 为
  84.99%，低于 85% 门槛；随后给统一系统打开场景增加 Tree 双击打开解析验证后的被动 DOCX
  真实 PTY 路径；没有降低门槛或扩大 ignore 列表。

因此本记录明确区分：本次统一交互、真实 PTY、格式/安全/性能边界、覆盖率、本机安装与最终
原样聚合 `make ci` 均已通过；此前短时限并发波动仍保留为历史证据，不能改写成所有尝试均
一次通过。
