# 统一文件激活与系统打开 V1 设计

状态：Implemented in feature worktree（2026-07-21）
目标分支：`feat/file-preview-providers`
基线：`main@bedf9e1`

## 1. 决策摘要

Latte Lens 将“查看文件”拆成两个稳定动作：

- **内部预览（Preview）**：选中文件即在 Content 面板中显示有界、只读的 Lens 预览；
- **系统打开（Open Externally）**：只有用户显式激活文件时，才把经过安全分类的文件交给
  操作系统的默认应用。

系统打开是 App/Runtime 层的统一动作，不属于某个 `PreviewProvider`。PDF 即使在 Lens 内部
以文本形式预览，也应与图片、DOCX、文本、音视频和其他普通文件使用同一套系统打开入口。
新增 preview provider 不需要再实现自己的快捷键、鼠标逻辑或平台 opener。

V1 的核心用户心智是：

> 选中就是 Lens 内部预览；`Enter`、双击、`o` 或 `[Open]` 就是请求系统打开。

“统一入口”不等于“无条件执行”。所有入口进入同一安全状态机：可确认的被动文件直接
打开，未知但不可执行的普通文件需要二次确认，脚本、可执行文件、启动器和明显伪装内容
始终阻止。

## 2. 设计原则

1. **动作与格式解耦**：系统打开依据安全分类和文件 identity，不依据 Content 面板当前
   使用图片还是文本 renderer。
2. **显式激活**：移动选择、单击和刷新永不启动宿主应用。
3. **入口一致**：键盘、鼠标和标题栏按钮不得各自实现安全例外。
4. **默认安全**：扩展名只表示 claimed type；主动文件和明显 mismatch 不提供“继续打开”。
5. **能力驱动**：Linux 是否可打开由桌面会话与 opener 能力决定，不由操作系统名称或
   是否存在 SSH 环境变量粗略决定。
6. **后台有界**：分类、重验和 opener 启动不在 event handler / render 中执行；重复请求
   合并，所有探测都有输入和时间上限。
7. **诚实边界**：Lens 可以阻止路径、类型和伪装攻击，但不能把宿主默认应用变成沙箱，
   也不能消除把路径交给另一个进程后的全部 TOCTOU 竞争。

## 3. 用户交互契约

### 3.1 主交互矩阵

| 输入 | 普通文件 | 目录/容器行 | 无可打开目标 |
| --- | --- | --- | --- |
| 单击文件树行 / 方向键 | 选中并触发 Lens 内部预览 | 只选中 | 无副作用 |
| 单击 disclosure 图标 | 同单击行 | 立即展开/折叠 | 无副作用 |
| `Enter`（Tree 焦点） | 请求 Open Externally | 展开/折叠 | 显示短提示 |
| 双击文件树行 | 请求 Open Externally | 展开/折叠 | 无副作用 |
| `o` | 打开当前焦点对应的文件 | 不处理 | 显示短提示 |
| Content 标题栏 `[Open]` | 请求 Open Externally | 不显示 | 不显示 |

当前焦点对应的文件必须确定且可解释：

- Tree 焦点：使用当前选中的 All Files / Git Changes 文件行；已删除 change、issue、仓库、
  目录和 submodule pointer 不是可打开目标；
- Content 焦点：使用当前已成功加载的 `content_source_target`，包括工作区文件和受支持的
  dependency source；
- Search、Find 和 Navigation popup 打开时，由 popup 捕获 `Enter` 和普通字符，避免输入
  过程中意外启动外部应用。用户接受结果并返回主界面后再使用统一入口。

Git Changes 仍会在选择时自动加载内部 diff；`Enter` 激活文件，`Right` / `l` 进入 Content
阅读 diff。Content 焦点下的 `Enter` 继续用于折叠，不能被全局覆盖。

### 3.2 鼠标细节

- 文件树单击从当前“单击目录即展开”调整为“单击只选择”；否则双击目录会连续切换两次；
- 双击由同一 scope、同一稳定 row identity、相同主按键且间隔不超过 400 ms 判定；仅比较
  屏幕行号不够，因为展开和异步刷新会移动行；
- 第一次单击只选择，第二次才提交激活动作；双击本身永远不能完成未知格式的二次确认；
- Content 区域的双击保留给文本选择，不绑定系统打开；
- disclosure glyph 有独立 hit region，点击它不参与 row double-click 计数。

### 3.3 可发现性

- 有可打开目标时，Content 标题栏右侧显示 `[Open]`；空间足够时同时显示
  `Enter · double-click · o`，窄屏只保留 `o Open`；
- footer 在 Tree 焦点显示 `Enter/double-click/o open`，在 Content 焦点保留 folding、find、
  navigation 帮助；
- `[Open]` 必须是真实 mouse hit region，不能只是文本提示；
- `Opened`、`Needs confirmation`、`Blocked`、`Unavailable`、`Failed` 都通过单行状态或有界
  Info 内容反馈，不使用只在日志中可见的错误。

### 3.4 未知格式确认

未知但已证明是普通、不可执行文件时，第一次激活进入 `AwaitingConfirmation`：

```text
Unknown file type: example.data. Press o again or click [Open anyway].
```

- 只有 `o` 或明确的 `[Open anyway]` 可以确认；`Enter`、双击和按键 repeat 不能确认；
- token 绑定 canonical target、文件 fingerprint 和 selection generation，15 秒后失效；
- 选择变化、刷新、文件变化、popup 打开或 scope 切换立即取消；
- 确认后仍执行一次完整重验；主动类型和 mismatch 没有确认通道。

## 4. 文件分类策略

### 4.1 结果类型

内部 `ExternalOpenClassifier` 返回以下 typed result，不让 UI 从错误文本反推：

| 结果 | 行为 |
| --- | --- |
| `EligiblePassive` | 直接调用平台 opener |
| `ConfirmationRequired` | 建立短期确认 token，不调用 opener |
| `BlockedActive` | 永久阻止，显示主动类型原因 |
| `BlockedMismatch` | 永久阻止，显示 claimed / detected 类型 |
| `BlockedUnsafePath` | 永久阻止，显示路径或对象类型原因 |
| `Unavailable` | 文件可打开，但当前主机没有可用桌面/opener |

`EligiblePassive` 表示可以交给系统默认应用，不表示 Lens 已证明文件内容无漏洞。合法 PDF、
Office、媒体或压缩容器仍由用户配置的宿主应用负责解析；这是必须在错误文案和文档中保留
的信任边界。

### 4.2 第一波直接打开范围

| 类别 | 资格判断 | 备注 |
| --- | --- | --- |
| PNG/JPEG/GIF/WebP | magic、decoder header 与 suffix 一致 | 保留系统失败后的 `i` 图片 fallback |
| PDF | byte 0 为 `%PDF-`，结构探测未发现明显 mismatch | 内部仍提取全部页文本；无 OCR |
| DOCX/XLSX/PPTX | ZIP offset 为 0，OOXML content type 与 suffix 一致且非 macro | 不要求存在内部 XLSX/PPTX renderer |
| UTF-8 文本/源码 | 无 NUL、无 executable bit、无 shebang、后缀不在主动列表 | `.sh` 等脚本即使是文本也阻止 |
| 常见音频/视频 | bounded magic 与被动媒体 suffix 一致 | mismatch 转确认或阻止，不猜测执行方式 |
| 常见压缩包 | bounded magic 与 archive suffix 一致 | 只交给默认应用，Lens 不自动解压 |

无法在有界 probe 中确认的其他普通文件进入 `ConfirmationRequired`。V1 不依赖系统 `file`
命令或 MIME database，因为它们的存在、版本和规则跨平台不一致；分类器使用内置、可测试的
最小 signature 与 extension policy。

### 4.3 永久阻止的主动类型

匹配不区分大小写，并同时检查扩展名、Unix executable bit、shebang 和常见二进制 magic：

- 可执行/安装器：`.exe`、`.com`、`.msi`、`.msix`、`.dll`、`.elf`、Mach-O、PE；
- shell/脚本：`.sh`、`.bash`、`.zsh`、`.fish`、`.command`、`.bat`、`.cmd`、`.ps1`、
  `.vbs`、`.vbe`、`.js`、`.jse`、`.wsf`、`.wsh`、`.hta`、`.py`、`.rb`、`.pl`；
- 启动器/快捷方式：`.desktop`、`.app`、`.lnk`、`.url`、`.scf`、`.workflow`；
- macro-capable Office：`.docm`、`.dotm`、`.xlsm`、`.xltm`、`.xlam`、`.pptm`、`.potm`、
  `.ppam`、`.sldm`；
- 带脚本/可执行 signature 却使用被动 suffix 的伪装文件。

列表是最低安全基线，不是穷举。平台特定 classifier 可以增加阻止项，但不能放宽共同基线。
V1 不提供“仍然执行”开关；用户可以离开 Lens 后自行处理主动文件。

### 4.4 内容与后缀不一致

- 对内置可识别格式，detected type 必须与 claimed type 一致；`script.pdf`、
  `elf.png`、`jpeg.docx` 一律 `BlockedMismatch`；
- 无扩展名的已验证被动内容可以按 detected type 直接打开，但状态提示必须显示检测类型；
- 多后缀只看最后 suffix 不够；主动 signature、executable bit 或 shebang 优先级最高；
- polyglot 使用最严格结果：任何被识别出的主动载荷都阻止；
- 合法容器中的 viewer-level action（例如 PDF URI/JavaScript）若已被有界 classifier 发现，
  至少进入确认状态；Lens 不执行 action，也不宣称能够发现所有宿主应用级恶意载荷。

### 4.5 路径与对象类型

- 只接受 `content_safety` 闸门返回的普通文件；目录、FIFO、socket、device 和 Windows
  reparse point 永不交给 opener；
- Repository / Dependency 读取保留 no-follow 语义；Git Changes 中的 symlink 只显示链接
  文本，不能系统打开 target；
- All Files 保留现有显式 follow-final-symlink 体验，但必须先 canonicalize 到普通文件 target，
  按 target 重新分类和重验；传给 opener 的是 canonical target，不是 link alias；
- 若现有安全策略不允许 target、canonicalization 失败或 target 在检查期间变化，则阻止；
- 平台命令使用绝对路径、独立参数和 `--`（适用时），从不通过 shell 字符串拼接。

## 5. 状态机

```text
Idle
  -- Enter / double-click / o / [Open] --> Classifying(generation, target)
       | unsafe/active/mismatch ----------> Blocked(message) --> Idle
       | unknown regular -----------------> AwaitingConfirmation(token)
       | eligible + no desktop -----------> Unavailable(message) --> Idle
       | eligible ------------------------> Launching(fingerprint)
                                               | opened --> Opened(status) --> Idle
                                               | failed --> Failed(status) --> Idle

AwaitingConfirmation
  -- o / [Open anyway] + same identity --> Revalidate --> Launching
  -- Enter / double-click / repeat ------> keep prompt; never confirm
  -- selection/file/time changes --------> Idle
```

不变量：

- UI 只提交 intent；`Classifying`、`Revalidate` 和 `Launching` 全在 background runtime；
- 每个请求携带 generation；旧 completion 不能覆盖新选择或新提示；
- 同一 fingerprint 在请求 active 或成功后的 500 ms 内去重，防止 key repeat / 连续双击启动
  多个应用；
- opener `Opened` 只表示系统接受了交接，不表示应用成功渲染文件；
- 选择文件不会自动进入状态机。

## 6. 文件 identity 与 TOCTOU

分类开始时通过安全句柄采集 `FileFingerprint`：

- Unix：device、inode、file type、length、mtime/ctime（平台可用精度）；
- Windows：volume serial、file index、attributes、length、last-write time，并拒绝 reparse；
- 其他平台：保守 metadata fingerprint；无法证明稳定 identity 时至少进入确认或阻止。

调用 opener 前重新以同一安全策略打开目标并比较 fingerprint；任何变化都取消请求并提示
“file changed while opening”。对需要 signature/container 判断的格式，重验必须重新读取
有界 probe，不能只比较扩展名。

系统 opener 最终仍会按路径重新打开文件，因此另一个进程可以在 Lens 重验完成后替换路径。
V1 不通过临时副本改变用户看到的文件，也不假设 `/proc/self/fd` 跨平台可用；这个最后窗口
不能被描述为原子安全。Lens 的保证是缩小并检测常见竞争、拒绝主动类型，而不是替宿主应用
提供强制沙箱。

## 7. 跨平台 opener

| 平台 | Adapter | 能力与失败语义 |
| --- | --- | --- |
| macOS | `open -- <absolute-path>` | 不经过 shell；启动后短暂观察快速非零退出 |
| Windows | `ShellExecuteW("open", path)` | 不使用 `cmd /c start`；返回值 `<= 32` 为失败 |
| Linux desktop | `xdg-open <path>`；仅 NotFound 时尝试 `gio open <path>` | 非空 `DISPLAY` 或 `WAYLAND_DISPLAY` 才尝试；非零退出不继续换 adapter |
| 其他 | 无 | 返回 `Unavailable`，保留 Lens 内部预览 |

Linux 的 `SSH_CONNECTION` 只是诊断信息：有 X11/Wayland forwarding 时可以打开；没有
`DISPLAY`/`WAYLAND_DISPLAY` 时即使安装了 `xdg-open` 也不启动，避免落到文本浏览器或
长时间挂起。macOS/Windows 仍以实际 API 结果为准。

opener 运行在 worker 中。POSIX adapter 最多观察 1 秒快速失败；超过窗口视为已交接，由
独立 reaper 回收 child。stdout/stderr/stdin 都关闭或定向到 null，不污染 TUI。

## 8. 无桌面与 fallback

- 系统打开不可用时，原有 Lens 内部预览保持不变；
- 已验证图片显示明确提示，只有用户再按 `i` 才生成有界 TrueColor 半块像素；
- PDF、DOCX、文本继续使用现有 Content 预览，不渲染字符画，不自动调用转换器；
- 没有内部 provider 的文件显示“系统 opener 不可用且 Lens 暂无内部预览”，并可复制真实
  路径；
- `TERM=dumb` 时图片 fallback 也明确不可用；不把灰阶 ASCII 描述成正确图片预览。

## 9. 性能与并发预算

- 系统打开不得为了确认资格再次完整提取 PDF 全页文本或完整渲染 DOCX；分类器最多读取
  固定 probe，并对 OOXML 中央目录/必要 content type 使用 entry、总字节和 deadline 上限；
- 图片只读 header；媒体/archive 只读 signature 所需前后小块；未知大文件不读完整内容；
- 分类结果可以复用当前 preview snapshot 的 typed evidence，但必须在打开前重验文件
  fingerprint；显示文本不能作为安全 evidence；
- App 使用独立 `external_open_requests` generation；worker queue 只保留最新 pending 请求；
- 同目标并发 intent 合并，selection 变化取消 pending confirmation；
- UI thread 不做 metadata、canonicalize、文件读取、进程启动或等待；
- 每类 probe 的字节、entry、嵌套深度、分配和 wall-time 上限必须是常量并有确定性单测；
- 性能验收记录 cold classification、warm evidence reuse、输入字节和 wall time，但不使用脆弱的
  毫秒级 CI hard assertion。

## 10. 代码职责

```text
src/app.rs
  统一 intent、焦点/选中目标、confirmation token、generation、状态反馈

src/ui.rs
  disclosure / row double-click hit region、[Open] / [Open anyway]、footer 提示

src/runtime.rs
  ExternalOpenRequest queue、target resolve、background classify/revalidate、stale reject

src/system_preview.rs（当前承载通用 external-open policy 与 platform adapters）
  ExternalOpenClassifier、FileFingerprint、risk policy、platform opener adapters

src/preview/*
  继续负责内部有界预览；可提供 internal typed evidence，但不直接启动系统应用
```

当前内部 completion 使用 typed outcome；策略/重验拒绝通过外层 `Result::Err` 传递并统一显示
为 `System open blocked`：

```rust,ignore
enum ExternalOpenOutcome {
    Opened,
    ConfirmationRequired { token: ConfirmationToken, detected: Option<FileType> },
    Unavailable { reason: String, image_fallback: bool },
    Failed { reason: String, image_fallback: bool },
}
```

安全决策使用 enum；只有展示层把它净化并格式化为字符串。V1 不要求修改公开的
`PreviewProvider` API，避免把宿主进程启动 authority 暴露给第三方 provider。

## 11. 第一波实现顺序

1. 将现有 image-only `request_system_image_preview` / external queue 重命名并重构成通用
   External Open intent 与 typed outcome；保留图片 `i` fallback；
2. 实现 classifier、主动类型基线、常见被动格式 probe、fingerprint 和重验；
3. 接入 `o`，验证 PDF/DOCX/文本与图片使用同一 runtime path；
4. 修改 Tree `Enter` / 单击 / disclosure / 双击语义并增加稳定 identity 防抖；
5. 增加 Content `[Open]` / `[Open anyway]` hit region 与窄屏文案；
6. 实现 macOS、Linux、Windows adapter 与 headless 分支；
7. 更新 README、Preview Provider 文档、测试门禁和 PTY E2E；
8. 运行 `make fmt-check`、`make check`、`make lint`、`make test`、`make ci`、`make coverage`，
   再执行用户要求的 `make install`。

## 12. 测试矩阵

### Unit

- classifier：直接、确认、active、mismatch、unsafe path 六类结果；
- 主动后缀大小写、多后缀、shebang、Unix executable bit、PE/ELF/Mach-O magic；
- PDF/image/OOXML 正确与伪装、polyglot、macro content type、损坏/超预算 container；
- fingerprint 相同/变化、确认 token identity/expiry/invalidation；
- Linux desktop/headless、adapter NotFound fallback、非零退出不 fallback；
- shell-free 参数、以 `-` 开头和包含空格/引号/换行的路径；
- request dedupe、generation stale completion、快速失败窗口。

### App / UI integration

- Tree 单击只选择，disclosure 单击展开，目录双击只切换一次；
- 文件 `Enter`、双击、`o`、`[Open]` 产生相同 request；
- Content `Enter` 仍切换 fold，popup `Enter` 不触发系统打开；
- Git Changes 选择仍加载 diff，Tree `Enter` 打开存在文件，deleted/pointer 被禁用；
- 未知格式的双击/Enter 不确认，只有 `o` / `[Open anyway]` 能确认；
- 标题栏按钮与 mouse hitbox 在窄屏、resize、滚动后保持一致；
- 选择变化、文件变化和超时取消确认；重复 Enter/double-click 只启动一次；
- opener unavailable：图片提供显式 `i`，PDF/DOCX 保留内部预览且不提供字符画。

### Production PTY / platform

- 使用隔离 stub opener，绝不在测试中启动真实桌面应用；
- 完整场景覆盖 PDF：选择 -> 内部预览 -> `o` -> opened status；
- 图片 headless/failure：metadata -> `o` -> prompt -> `i` / `Esc`；
- 未知文件：第一次激活提示，`Enter`/双击不确认，`o` 再确认；
- 伪装脚本：所有入口均 blocked，stub opener 的调用计数保持 0；
- Linux/macOS 跑 PTY；Windows 通过 adapter unit/integration、locked check 和 package gate。

## 13. 非目标

- OCR、PDF/DOCX 原版式分页渲染、Office 编辑能力；
- `Open With…` 应用选择器、最近使用应用、文件关联管理；
- 执行脚本、安装包、应用 bundle、快捷方式或 macro 文档的 override；
- 为宿主默认应用提供沙箱、恶意文档查杀或漏洞检测保证；
- Kitty/Sixel/iTerm 等私有图像协议自动探测；
- 通过临时副本、上传或网络服务打开文件；
- 在 Search popup 内增加另一套系统打开快捷键。

## 14. 完成标准

V1 只有同时满足以下条件才算完成：

- PDF、图片、DOCX 和普通文本从 `o`、Tree `Enter`、双击与 `[Open]` 进入同一 runtime 动作；
- Tree 单击/双击/目录展开语义无二次 toggle 回归；
- 主动类型、伪装脚本、mismatch、特殊文件和路径变化不会到达 platform opener；
- 未知格式有不可被双击/按键 repeat 越过的显式确认；
- macOS、Windows、Linux desktop/headless 的结果与错误可见；
- 分类和重验有界、后台执行、重复请求合并、stale completion 被拒绝；
- 文档、unit、App integration、真实 PTY 和 coverage 同步更新；
- 原样门禁失败与组件级通过分别报告，不把隔离复跑改写成聚合门禁全绿；
- 未经用户明确授权不 commit、push 或创建 PR。
