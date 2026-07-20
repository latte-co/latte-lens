# 快捷键设计规范

状态：已实现。本文定义 Latte Lens 的快捷键设计原则、作用域分组、完整清单与新增流程。
canonical keymap 按作用域分组，不照搬任何桌面 IDE 的整套键位；代码实现、README controls
与 footer help text 必须以本文为唯一事实来源。

## 1. 概述

Latte Lens 的快捷键按"作用域"分组设计，而不是混用不同 IDE 的整套 keymap。每个作用域
对应一种用户意图类别，使用固定的修饰符约定，降低记忆成本并避免终端兼容性问题。

核心原则：

1. **作用域优先**：每个快捷键先归属于一个作用域，修饰符由作用域决定，不由功能优先级决定。
2. **不照搬 IDE keymap**：不引入 VS Code、Vim、Emacs 或 Helix 的整套键位；只采纳符合终端
   约束且助记清晰的少量约定。
3. **终端友好**：不依赖 F 键、Command/Super 键、两段 chord 或多数终端无法稳定上报的组合。
4. **单一事实来源**：代码、README controls 表与 footer help text 必须保持一致，以本文为准。

## 2. 作用域分组与修饰符约定

| 作用域 | 修饰符约定 | 适用场景 | 现有示例 |
| --- | --- | --- | --- |
| 全局命令 | `Ctrl` + 助记字母 | 跨面板、跨视图的全局操作 | `Ctrl+P` 文件搜索、`Ctrl+F` 当前内容查找、`Ctrl+T` 工作区搜索 |
| 代码语义命令 | `Ctrl` + 助记字母 | 基于语言服务器的语义导航 | `Ctrl+D` Definition、`Ctrl+R` References、`Ctrl+O` Implementations、`Ctrl+S` Document Symbols |
| 面板/树/视口移动 | 无修饰方向键或 TUI 单键 | 焦点移动、滚动、树展开折叠 | `↑/↓/←/→`、`j/k`、`h/l`、`Tab`、`Enter`、`[/]`、`{/}` |
| 当前视图操作 | 无修饰小写单键 | 切换当前视图内容或刷新 | `p` Preview、`d` Diff、`r` Refresh、`q` Quit、`y/Y` 复制路径、`n/N` 更改文件 |
| 历史方向 | `Alt` + 方向键 | 导航历史回退与前进 | `Alt+Left` Back、`Alt+Right` Forward |
| 鼠标语义提示 | `Alt` + 鼠标 | 鼠标悬停 token 高亮与点击跳转 | `Alt+Moved` token 下划线、`Alt+左键` Definition |

全局命令与代码语义命令均使用 `Ctrl` + 助记字母，但二者在不同的焦点与视图状态下生效：
全局命令在任意焦点可用；代码语义命令只在 Content 焦点、`ContentMode::Preview`、无
Search/Find/navigation results popup 时生效。

## 3. 完整快捷键清单

### 3.1 全局命令

| 按键 | 功能 |
| --- | --- |
| `Ctrl+P` | 打开文件搜索 popup |
| `Ctrl+F` | 在当前 Preview 或 Diff 中查找 |
| `Ctrl+Shift+F` / `Ctrl+T` | 打开工作区文本搜索 popup；`Ctrl+T` 用于无法区分 `Ctrl+Shift+F` 与 `Ctrl+F` 的终端 |
| `Ctrl+C` | 无内容选择时立即退出；有选择时复制当前选择 |

### 3.2 代码语义命令

| 按键 | 功能 |
| --- | --- |
| `Ctrl+D` | 请求 Definition；单个结果直接跳转，多个结果打开 navigation popup |
| `Ctrl+R` | 查询 References；有结果时始终打开 results popup |
| `Ctrl+O` | 查询 Implementations；有结果时始终打开 results popup，`O` 表示 Open implementations |
| `Ctrl+S` | 打开 Document Symbols results popup |

### 3.3 面板/树/视口移动

| 按键 | 功能 |
| --- | --- |
| `↑` / `↓` | 移动焦点树或滚动焦点内容；`↑` 在首行/空树行时焦点 scope tabs |
| `←` / `→` | 焦点 Tree 或 Content；scope tabs 聚焦时选择 All Files 或刷新/选择 Git Changes |
| `Shift+←` / `Shift+→` | 水平滚动 Diff/Info；Preview 自动换行 |
| `j` / `k` | 焦点树中移动，或滚动焦点内容窗格 |
| `h` / `l` | 焦点树或内容窗格 |
| `Tab` / `Shift+Tab` | 切换左树 scope 并保持焦点 |
| `Enter` | 展开/折叠选定仓库/目录，或对选定文件/pointer diff 焦点 Content |
| `1` / `2` | 显示所有文件，或刷新并仅显示 Git 更改，保持焦点 |
| `[` / `]` | 在焦点 Preview 内容中跳转到上一个/下一个可见折叠标记 |
| `Enter` / `Space` | 在焦点 Preview 内容中切换当前标记处的折叠 |
| `{` / `}` | 在焦点 Preview 内容中折叠或展开所有折叠 |

### 3.4 当前视图操作

| 按键 | 功能 |
| --- | --- |
| `p` | 在右窗格显示 Preview |
| `d` | 在右窗格显示 Diff |
| `r` | 刷新仓库状态 |
| `q` | 1.5 秒内按两次退出；`Esc` 先关闭活动搜索 |
| `y` | 复制选定路径的相对路径（符号链接取 link path）；目录加尾部 `/` |
| `Y` | 复制选定路径的真实/绝对路径（符号链接取 resolved target）；目录加尾部 `/` |
| `n` | Diff 中下一个更改文件 |
| `N` | Diff 中上一个更改文件 |
| `Space` | 标记显示的文件 diff 已审阅；再按清除标记 |

### 3.5 历史方向

| 按键 | 功能 |
| --- | --- |
| `Alt+Left` | 回退到上一个成功导航位置 |
| `Alt+Right` | 前进到下一个成功导航位置 |

### 3.6 鼠标语义提示

| 按键 | 功能 |
| --- | --- |
| `Alt` + 鼠标移动 | 在精确命中可导航 token 时给整个 token 加下划线 |
| `Alt` + 左键 | 在精确鼠标 point 请求 Definition |

## 4. 设计规范

### 4.1 `Ctrl` + 助记字母规范

- 字母取功能英文助记：`D` = Definition、`R` = References、`O` = Open implementations、
  `S` = Document Symbols、`P` = Project file search、`F` = Find、`T` = Text search。
- 同时接受大小写：`Char('d' | 'D')` + modifiers 恰为 `CONTROL` 均触发同一行为。用户无需
  主动按 `Shift`；`Ctrl+D` 与 `Ctrl+Shift+D` 等效。
- 修饰符必须恰为 `CONTROL`；任何额外 `ALT/SUPER/SHIFT` 组合均忽略，不做近似匹配。

### 4.2 无修饰小写单键规范

- 小写字母执行常规操作：`p` = Preview、`d` = Diff、`r` = Refresh、`q` = Quit、`y` = 复制
  相对路径、`n` = 下一个更改文件。
- 大写字母执行增强或反向操作：`Y` = 复制真实/绝对路径、`N` = 上一个更改文件。
- 无修饰单键只在当前视图操作作用域内生效，不与 `Ctrl` + 助记字母冲突。

### 4.3 大小写变体规范

- 大小写变体共享同一字母键，小写为正向/常规，大写为反向/增强：`n/N`（下一个/上一个）、
  `y/Y`（相对路径/绝对路径）。
- 变体必须在助记上语义相关，不借用无关字母。
- `Ctrl` + 助记字母不区分大小写，因此不存在大小写变体（见 4.1）。

### 4.4 终端兼容性原则

- `Ctrl+T` 替代 `Ctrl+Shift+F`：多数终端无法稳定区分 `Ctrl+Shift+F` 与 `Ctrl+F`，
  因此工作区文本搜索同时提供 `Ctrl+T`。
- `Ctrl+O` 替代 `Ctrl+I`：`Ctrl+I` 在传统终端与 `Tab` 编码相同，无法稳定区分；
  Implementations 使用可稳定区分的 `Ctrl+O`。
- 不依赖 F 键：搜索栏的 `F2/F3/F4/F5` 切换项提供等效点击入口，键盘操作不强制使用 F 键。
- 不依赖 Command/Super：macOS 顶排 Command 键在多数终端无法稳定上报，不纳入 canonical keymap。
- 不使用两段 chord：不引入 `g d`、`Ctrl+K Ctrl+S` 等需要先按一个键再按第二个键的组合。

### 4.5 旧 alias 兼容承诺

- 功能尚未发布时，不为旧 alias 保留兼容承诺。
- 代码实现、README controls 与 footer 只把本文清单作为主入口，不宣传 IDE 兼容 alias。
- 已废弃的 alias（如 `Ctrl+D` 兼任翻页）不保留回退路径。

### 4.6 Footer 展示优先级

Footer help text 按以下优先级展示，高优先级状态覆盖低优先级：

1. 退出确认消息（`quit_confirmation_message`）
2. 错误消息（`last_error`）
3. 加载状态（refreshing / directory loading / content loading）
4. 正常帮助文本

正常帮助文本根据视口宽度和当前内容模式选择不同密度：

| 条件 | 展示策略 |
| --- | --- |
| 宽度 < 96 且 Preview | 精简：滚动、复制/退出、折叠、导航、符号、查找、scope、复制路径、退出 |
| 宽度 < 96 且非 Preview | 精简：移动、焦点、拖拽复制、退出、scope、刷新、复制路径、退出 |
| 宽度 ≥ 96 且 Preview | 完整：滚动、复制/退出、折叠、回车切换、导航、符号、Alt 点击、历史、查找、scope、复制路径、退出 |
| 宽度 ≥ 96 且 Diff | 完整：滚动、焦点、空格审阅、n/N 文件、查找、scope、预览/差异、刷新、复制路径、退出 |
| 宽度 ≥ 96 且其他 | 完整：移动、焦点、拖拽复制、退出/复制、Shift 滚动、scope、预览/差异、刷新、复制路径、退出 |

Footer 只展示当前上下文可操作的快捷键，不列出全部清单；完整清单以 README controls 表
和本文为准。

## 5. 设计红线（禁止事项）

1. **不用 `Ctrl+D` 兼任翻页**：`Ctrl+D` 专用于 Definition；翻页保留 `PageDown`、`PageUp`
   和鼠标滚轮。
2. **不用两段 chord**：不引入 `g d`、`Ctrl+K` 等需要两段输入的组合。
3. **不依赖 Command/Super 键**：macOS Command 键和 Super 键在终端环境无法稳定上报，
   不纳入 canonical keymap。
4. **不用 `Ctrl+I`**：`Ctrl+I` 与 `Tab` 在传统终端编码相同，无法稳定区分；
   Implementations 使用 `Ctrl+O`。
5. **不在多个作用域定义同一按键的不同行为**：每个按键在同一焦点/视图状态下只能有
   一个行为；跨作用域的同一按键必须在不同状态下互斥生效。

## 6. 新增快捷键检查清单

当需要新增快捷键时，按以下清单逐项检查：

- [ ] 属于哪个作用域？（全局命令 / 代码语义命令 / 面板树视口移动 / 当前视图操作 /
      历史方向 / 鼠标语义提示）
- [ ] 修饰符是否符合该作用域的约定？（全局/语义用 `Ctrl`，移动无修饰，视图操作用
      小写单键，历史用 `Alt`+方向，鼠标用 `Alt`+鼠标）
- [ ] 键位是否已被占用？（在同一焦点/视图状态下不能与现有按键冲突）
- [ ] 是否有终端兼容性问题？（避开 `Ctrl+I`、`Ctrl+Shift+字母`、F 键、Command/Super、
      两段 chord）
- [ ] 是否需要大小写变体？（小写=常规，大写=增强/反向；仅无修饰单键支持变体）
- [ ] README 控件表是否已更新？（`README.md` 的 Inside the TUI 表格）
- [ ] Footer help text 是否已更新？（`src/ui.rs` 的 footer 帮助文本，含各宽度/模式分支）
- [ ] 代码、README、footer 三方是否一致？（以本文为单一事实来源）

## 7. 参考

- 快捷键交互设计与语义导航生效条件：
  [`docs/design/code-navigation.md`](code-navigation.md) 第 9.1 节。
- 控件清单与鼠标操作说明：
  [`README.md`](../../README.md) "Inside the TUI" 控件表。
- Footer help text 实现：
  [`src/ui.rs`](../../src/ui.rs) footer 帮助文本构造逻辑。
