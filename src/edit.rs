//! 编辑会话核心：EditSession、PreviewSnapshot、EditDelta、undo/redo、文件读写。
//!
//! 本模块是纯逻辑层，不依赖渲染。app.rs 负责模态分发、caret 移动（视觉行）、
//! 鼠标交互；本模块负责缓冲区变更、undo/redo 栈、原子写与冲突检测。

use std::{
    collections::HashSet,
    fs,
    ops::Range,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    app::{ContentPoint, ContentSelection, NavigationCaret},
    folding::FoldAnchor,
    preview::HighlightSpan,
};

/// 临时文件计数器，保证同进程内并发写入不冲突。
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// undo 合并窗口：连续同行单字符编辑在此间隔内合并为一个 delta。
const COALESCE_WINDOW: Duration = Duration::from_secs(2);

/// undo 栈上限，防超大文件内存膨胀。
const MAX_UNDO_STACK: usize = 1000;

/// 大文件阈值（行数），超过则跳过重高亮。
const LARGE_FILE_LINES: usize = 50_000;

/// 大文件阈值（字节数），超过则跳过重高亮。
const LARGE_FILE_BYTES: usize = 10 * 1024 * 1024;

// ---------------------------------------------------------------------------
// NewlineStyle
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NewlineStyle {
    Lf,
    Crlf,
}

impl NewlineStyle {
    fn separator(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

// ---------------------------------------------------------------------------
// PreviewSnapshot — 进入编辑时的完整预览快照，取消时恢复
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct PreviewSnapshot {
    pub(crate) lines: Vec<String>,
    pub(crate) highlights: Vec<Vec<HighlightSpan>>,
    pub(crate) scroll: usize,
    pub(crate) horizontal_scroll: usize,
    pub(crate) collapsed_folds: HashSet<FoldAnchor>,
    pub(crate) cursor_line: usize,
    pub(crate) selection: Option<ContentSelection>,
    pub(crate) navigation_caret: NavigationCaret,
}

// ---------------------------------------------------------------------------
// EditDelta — 行范围替换 delta，undo 存 old_lines，redo 存 new_lines
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct EditDelta {
    /// 受影响行范围（编辑前的行索引）。
    line_range: Range<usize>,
    /// 被替换的行（undo 用）。
    old_lines: Vec<String>,
    /// 替换后的行（redo 用）。
    new_lines: Vec<String>,
    /// 编辑前 caret。
    caret_before: ContentPoint,
    /// 编辑后 caret。
    caret_after: ContentPoint,
    /// 时间戳（coalescing 用）。
    timestamp: Instant,
    /// 是否可与后续单字符编辑合并。
    coalescible: bool,
}

impl EditDelta {
    /// 应用 delta（redo 方向）：将 old_lines 替换为 new_lines。
    fn apply(&self, lines: &mut Vec<String>) {
        lines.splice(self.line_range.clone(), self.new_lines.iter().cloned());
    }

    /// 回滚 delta（undo 方向）：将 new_lines 替换回 old_lines。
    fn revert(&self, lines: &mut Vec<String>) {
        let new_range = self.line_range.start..self.line_range.start + self.new_lines.len();
        lines.splice(new_range, self.old_lines.iter().cloned());
    }
}

// ---------------------------------------------------------------------------
// EditSession
// ---------------------------------------------------------------------------

pub(crate) struct EditSession {
    /// 进入编辑时的预览快照（取消时恢复）。
    pub(crate) preview_snapshot: PreviewSnapshot,
    /// 原始文件字节（冲突检测用）。
    original_bytes: Vec<u8>,
    /// 原始文件权限（Unix mode）。
    original_mode: Option<u32>,
    /// 换行符风格。
    newline_style: NewlineStyle,
    /// 编辑光标（(line, byte) 字节偏移）。
    pub(crate) caret: ContentPoint,
    /// Up/Down 移动时的首选显示列。
    pub(crate) preferred_column: usize,
    /// 编辑选区（语义独立于 content.selection）。
    pub(crate) selection: Option<ContentSelection>,
    undo_stack: Vec<EditDelta>,
    redo_stack: Vec<EditDelta>,
    pub(crate) dirty: bool,
    /// 是否已保存到磁盘（区分"未修改"和"已保存"，决定退出时恢复快照还是重载）。
    pub(crate) saved: bool,
    /// debounce 重高亮到期时间。
    pub(crate) highlight_due: Option<Instant>,
    /// 大文件标记（跳过重高亮）。
    pub(crate) large_file: bool,
}

impl EditSession {
    // -- 构造 ---------------------------------------------------------------

    /// 读取完整文件并创建编辑会话。
    ///
    /// 返回的 session 中 `preview_snapshot.lines` 持有完整文件行；
    /// 调用方负责 `mem::swap(&mut content.lines, &mut session.preview_snapshot.lines)`
    /// 将完整文件换入 content.lines、截断预览换入快照。
    pub(crate) fn begin(path: &Path, mut preview_snapshot: PreviewSnapshot) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
        let newline_style = if text.contains("\r\n") {
            NewlineStyle::Crlf
        } else {
            NewlineStyle::Lf
        };
        let lines = split_lines(text, newline_style);
        let original_mode = file_mode(path);
        let large_file = lines.len() > LARGE_FILE_LINES || bytes.len() > LARGE_FILE_BYTES;

        preview_snapshot.lines = lines;

        Ok(Self {
            preview_snapshot,
            original_bytes: bytes,
            original_mode,
            newline_style,
            caret: ContentPoint { line: 0, byte: 0 },
            preferred_column: 0,
            selection: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            dirty: false,
            saved: false,
            highlight_due: None,
            large_file,
        })
    }

    // -- 访问器 --------------------------------------------------------------

    pub(crate) fn dirty(&self) -> bool {
        self.dirty
    }

    #[cfg(test)]
    pub(crate) fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn into_preview_snapshot(self) -> PreviewSnapshot {
        self.preview_snapshot
    }

    // -- caret 移动（仅需行内容的方向；↑/↓ 由 app.rs 算好目标后调 set_caret）----

    /// 左移一个 grapheme。
    #[cfg(test)]
    pub(crate) fn caret_left(&mut self, lines: &[String], extend_selection: bool) {
        let line = &lines[self.caret.line];
        let new_byte = if self.caret.byte > 0 {
            grapheme_start_before(line, self.caret.byte)
        } else if self.caret.line > 0 {
            lines[self.caret.line - 1].len()
        } else {
            return;
        };

        let new_line = if self.caret.byte > 0 {
            self.caret.line
        } else {
            self.caret.line - 1
        };

        self.move_caret_to(
            ContentPoint {
                line: new_line,
                byte: new_byte,
            },
            extend_selection,
        );
    }

    /// 右移一个 grapheme。
    #[cfg(test)]
    pub(crate) fn caret_right(&mut self, lines: &[String], extend_selection: bool) {
        let line = &lines[self.caret.line];
        let (new_line, new_byte) = if self.caret.byte < line.len() {
            (self.caret.line, grapheme_end_after(line, self.caret.byte))
        } else if self.caret.line + 1 < lines.len() {
            (self.caret.line + 1, 0)
        } else {
            return;
        };

        self.move_caret_to(
            ContentPoint {
                line: new_line,
                byte: new_byte,
            },
            extend_selection,
        );
    }

    /// Home：第一次跳到首个非空白字符，再按跳到行首。
    #[cfg(test)]
    pub(crate) fn caret_home(&mut self, lines: &[String], extend_selection: bool) {
        let line = &lines[self.caret.line];
        let first_non_ws = line
            .char_indices()
            .find(|&(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(0);
        let target = if self.caret.byte == first_non_ws && first_non_ws > 0 {
            0
        } else {
            first_non_ws
        };
        self.move_caret_to(
            ContentPoint {
                line: self.caret.line,
                byte: target,
            },
            extend_selection,
        );
    }

    /// End：跳到行尾。
    #[cfg(test)]
    pub(crate) fn caret_end(&mut self, lines: &[String], extend_selection: bool) {
        let byte = lines[self.caret.line].len();
        self.move_caret_to(
            ContentPoint {
                line: self.caret.line,
                byte,
            },
            extend_selection,
        );
    }

    /// 统一的 caret 移动 + 选区扩展逻辑。
    #[cfg(test)]
    fn move_caret_to(&mut self, target: ContentPoint, extend_selection: bool) {
        if extend_selection {
            match &mut self.selection {
                Some(sel) => sel.head = target,
                None => {
                    self.selection = Some(ContentSelection {
                        anchor_before: self.caret,
                        anchor_after: self.caret,
                        head: target,
                        dragging: false,
                        dragged: true,
                    });
                }
            }
        } else {
            self.selection = None;
        }
        self.caret = target;
    }

    // -- 选区工具 -------------------------------------------------------------

    /// 返回指定行上被选中的字节范围（已 clamp 到行长度）。
    #[cfg(test)]
    pub(crate) fn selection_byte_range(
        &self,
        line: usize,
        line_len: usize,
    ) -> Option<Range<usize>> {
        let sel = self.selection?;
        let (start, end) = sel.normalized();
        let end_byte = end.byte.min(line_len);
        let start_byte = start.byte.min(line_len);

        if start.line == end.line {
            if line == start.line && start_byte < end_byte {
                Some(start_byte..end_byte)
            } else {
                None
            }
        } else if line == start.line {
            Some(start_byte..line_len)
        } else if line == end.line {
            if end_byte > 0 {
                Some(0..end_byte)
            } else {
                None
            }
        } else if line > start.line && line < end.line {
            Some(0..line_len)
        } else {
            None
        }
    }

    // -- 编辑操作 -------------------------------------------------------------

    /// 在 caret 处插入字符（有选区则替换选区）。
    pub(crate) fn insert_char(&mut self, lines: &mut Vec<String>, ch: char) {
        let caret_before = self.caret;

        if let Some(sel) = self.selection.take() {
            // 选区替换：删除选区 + 插入字符，一个 delta（不合并）
            let (start, _) = sel.normalized();
            let (line_range, old_lines, mut new_lines) = delete_selection(lines, sel);
            let insert_byte = start.byte;
            if let Some(first) = new_lines.first_mut() {
                first.insert(insert_byte, ch);
            }
            lines.splice(line_range.clone(), new_lines.iter().cloned());
            let caret_after = ContentPoint {
                line: line_range.start,
                byte: insert_byte + ch.len_utf8(),
            };
            self.push_delta(EditDelta {
                line_range,
                old_lines,
                new_lines,
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: false,
            });
            self.caret = caret_after;
        } else {
            // 纯插入
            let line = self.caret.line;
            let byte = self.caret.byte;
            lines[line].insert(byte, ch);
            let caret_after = ContentPoint {
                line,
                byte: byte + ch.len_utf8(),
            };

            if self.try_coalesce(line, lines, caret_before, caret_after) {
                self.caret = caret_after;
                return;
            }

            let old_line = format!(
                "{}{}",
                &lines[line][..byte],
                &lines[line][byte + ch.len_utf8()..]
            );
            self.push_delta(EditDelta {
                line_range: line..line + 1,
                old_lines: vec![old_line],
                new_lines: vec![lines[line].clone()],
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: true,
            });
            self.caret = caret_after;
        }
        self.after_edit();
    }

    /// 粘贴文本（多行不合并）。
    pub(crate) fn insert_text(&mut self, lines: &mut Vec<String>, text: &str) {
        let caret_before = self.caret;

        // 先删选区
        if let Some(sel) = self.selection.take() {
            let (line_range, old_lines, new_lines) = delete_selection(lines, sel);
            lines.splice(line_range.clone(), new_lines.iter().cloned());
            let caret_after = ContentPoint {
                line: line_range.start,
                byte: new_lines.first().map(|l| l.len()).unwrap_or(0),
            };
            // 把删除选区的 delta 先压栈，再压粘贴 delta
            self.push_delta(EditDelta {
                line_range,
                old_lines,
                new_lines,
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: false,
            });
            self.caret = caret_after;
        }

        // 规范化换行
        let text = text.replace("\r\n", "\n");
        let parts: Vec<&str> = text.split('\n').collect();

        if parts.len() == 1 {
            // 单行粘贴
            let line = self.caret.line;
            let byte = self.caret.byte;
            lines[line].insert_str(byte, parts[0]);
            let caret_after = ContentPoint {
                line,
                byte: byte + parts[0].len(),
            };
            let old_line = format!(
                "{}{}",
                &lines[line][..byte],
                &lines[line][byte + parts[0].len()..]
            );
            self.push_delta(EditDelta {
                line_range: line..line + 1,
                old_lines: vec![old_line],
                new_lines: vec![lines[line].clone()],
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: false,
            });
            self.caret = caret_after;
        } else {
            // 多行粘贴：拆分当前行
            let line = self.caret.line;
            let byte = self.caret.byte;
            let old_line = lines[line].clone();
            let before = &old_line[..byte];
            let after = &old_line[byte..];

            let mut new_lines = Vec::with_capacity(parts.len());
            for (i, part) in parts.iter().enumerate() {
                if i == 0 {
                    let mut s = String::with_capacity(before.len() + part.len());
                    s.push_str(before);
                    s.push_str(part);
                    new_lines.push(s);
                } else if i == parts.len() - 1 {
                    let mut s = String::with_capacity(part.len() + after.len());
                    s.push_str(part);
                    s.push_str(after);
                    new_lines.push(s);
                } else {
                    new_lines.push((*part).to_string());
                }
            }

            let caret_after = ContentPoint {
                line: line + parts.len() - 1,
                byte: parts[parts.len() - 1].len(),
            };

            let line_range = line..line + 1;
            lines.splice(line_range.clone(), new_lines.iter().cloned());
            self.push_delta(EditDelta {
                line_range,
                old_lines: vec![old_line],
                new_lines,
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: false,
            });
            self.caret = caret_after;
        }
        self.after_edit();
    }

    /// 插入换行（按 newline_style join）。
    pub(crate) fn insert_newline(&mut self, lines: &mut Vec<String>) {
        let caret_before = self.caret;

        // 先删选区
        if let Some(sel) = self.selection.take() {
            let (line_range, old_lines, new_lines) = delete_selection(lines, sel);
            lines.splice(line_range.clone(), new_lines.iter().cloned());
            let caret_after = ContentPoint {
                line: line_range.start,
                byte: new_lines.first().map(|l| l.len()).unwrap_or(0),
            };
            self.push_delta(EditDelta {
                line_range,
                old_lines,
                new_lines,
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: false,
            });
            self.caret = caret_after;
        }

        let line = self.caret.line;
        let byte = self.caret.byte;
        let old_line = lines[line].clone();
        let before = old_line[..byte].to_string();
        let after = old_line[byte..].to_string();

        let line_range = line..line + 1;
        lines.splice(line_range.clone(), [before.clone(), after.clone()]);

        let caret_after = ContentPoint {
            line: line + 1,
            byte: 0,
        };
        self.push_delta(EditDelta {
            line_range,
            old_lines: vec![old_line],
            new_lines: vec![before, after],
            caret_before,
            caret_after,
            timestamp: Instant::now(),
            coalescible: false,
        });
        self.caret = caret_after;
        self.after_edit();
    }

    /// 向后删除（Backspace）。
    pub(crate) fn backspace(&mut self, lines: &mut Vec<String>) {
        let caret_before = self.caret;

        // 有选区：删除选区
        if let Some(sel) = self.selection.take() {
            let (start, _) = sel.normalized();
            let (line_range, old_lines, new_lines) = delete_selection(lines, sel);
            lines.splice(line_range.clone(), new_lines.iter().cloned());
            let caret_after = ContentPoint {
                line: line_range.start,
                byte: start
                    .byte
                    .min(new_lines.first().map(|l| l.len()).unwrap_or(0)),
            };
            self.push_delta(EditDelta {
                line_range,
                old_lines,
                new_lines,
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: false,
            });
            self.caret = caret_after;
            self.after_edit();
            return;
        }

        let line = self.caret.line;
        let byte = self.caret.byte;

        if byte == 0 {
            if line == 0 {
                return; // 文件开头，无可删
            }
            // 合并到上一行（行数变化，不合并）
            let prev_len = lines[line - 1].len();
            let old_lines = vec![lines[line - 1].clone(), lines[line].clone()];
            let merged = format!("{}{}", lines[line - 1], lines[line]);
            let line_range = line - 1..line + 1;
            lines.splice(line_range.clone(), [merged.clone()]);
            let caret_after = ContentPoint {
                line: line - 1,
                byte: prev_len,
            };
            self.push_delta(EditDelta {
                line_range,
                old_lines,
                new_lines: vec![merged],
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: false,
            });
            self.caret = caret_after;
        } else {
            // 删除前一个 grapheme（可合并）
            let del_start = grapheme_start_before(&lines[line], byte);
            let del_end = byte;
            let old_line = lines[line].clone();
            lines[line].replace_range(del_start..del_end, "");
            let caret_after = ContentPoint {
                line,
                byte: del_start,
            };

            if self.try_coalesce(line, lines, caret_before, caret_after) {
                self.caret = caret_after;
                return;
            }

            self.push_delta(EditDelta {
                line_range: line..line + 1,
                old_lines: vec![old_line],
                new_lines: vec![lines[line].clone()],
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: true,
            });
            self.caret = caret_after;
        }
        self.after_edit();
    }

    /// 向前删除（Delete）。
    pub(crate) fn delete(&mut self, lines: &mut Vec<String>) {
        let caret_before = self.caret;

        // 有选区：删除选区
        if let Some(sel) = self.selection.take() {
            let (start, _) = sel.normalized();
            let (line_range, old_lines, new_lines) = delete_selection(lines, sel);
            lines.splice(line_range.clone(), new_lines.iter().cloned());
            let caret_after = ContentPoint {
                line: line_range.start,
                byte: start
                    .byte
                    .min(new_lines.first().map(|l| l.len()).unwrap_or(0)),
            };
            self.push_delta(EditDelta {
                line_range,
                old_lines,
                new_lines,
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: false,
            });
            self.caret = caret_after;
            self.after_edit();
            return;
        }

        let line = self.caret.line;
        let byte = self.caret.byte;
        let line_len = lines[line].len();

        if byte >= line_len {
            if line + 1 >= lines.len() {
                return; // 最后一行行尾，无可删
            }
            // 合并下一行（行数变化，不合并）
            let old_lines = vec![lines[line].clone(), lines[line + 1].clone()];
            let merged = format!("{}{}", lines[line], lines[line + 1]);
            let line_range = line..line + 2;
            lines.splice(line_range.clone(), [merged.clone()]);
            let caret_after = ContentPoint { line, byte };
            self.push_delta(EditDelta {
                line_range,
                old_lines,
                new_lines: vec![merged],
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: false,
            });
            self.caret = caret_after;
        } else {
            // 删除后一个 grapheme（可合并）
            let del_end = grapheme_end_after(&lines[line], byte);
            let old_line = lines[line].clone();
            lines[line].replace_range(byte..del_end, "");
            let caret_after = ContentPoint { line, byte };

            if self.try_coalesce(line, lines, caret_before, caret_after) {
                self.caret = caret_after;
                return;
            }

            self.push_delta(EditDelta {
                line_range: line..line + 1,
                old_lines: vec![old_line],
                new_lines: vec![lines[line].clone()],
                caret_before,
                caret_after,
                timestamp: Instant::now(),
                coalescible: true,
            });
            self.caret = caret_after;
        }
        self.after_edit();
    }

    // -- undo / redo ----------------------------------------------------------

    pub(crate) fn undo(&mut self, lines: &mut Vec<String>) {
        let Some(delta) = self.undo_stack.pop() else {
            return;
        };
        delta.revert(lines);
        self.caret = delta.caret_before;
        self.selection = None;
        self.redo_stack.push(delta);
        self.dirty = true;
        self.schedule_highlight();
    }

    pub(crate) fn redo(&mut self, lines: &mut Vec<String>) {
        let Some(delta) = self.redo_stack.pop() else {
            return;
        };
        delta.apply(lines);
        self.caret = delta.caret_after;
        self.selection = None;
        self.undo_stack.push(delta);
        self.dirty = true;
        self.schedule_highlight();
    }

    // -- 保存 -----------------------------------------------------------------

    /// 冲突检测 + 原子写。成功后更新 original_bytes 并清除 dirty。
    pub(crate) fn save(&mut self, path: &Path, lines: &[String]) -> Result<()> {
        // 冲突检测：重新读盘比对
        let current_bytes =
            fs::read(path).with_context(|| format!("failed to re-read {}", path.display()))?;
        if current_bytes != self.original_bytes {
            anyhow::bail!(
                "{} changed on disk since editing started; not saving",
                path.display()
            );
        }

        atomic_write(path, lines, self.newline_style, self.original_mode)?;

        // 更新基线
        self.original_bytes = join_lines(lines, self.newline_style).into_bytes();
        self.dirty = false;
        self.saved = true;
        Ok(())
    }

    // -- debounce 重高亮 -------------------------------------------------------

    pub(crate) fn schedule_highlight(&mut self) {
        self.highlight_due = Some(Instant::now() + Duration::from_millis(300));
    }

    // -- 词边界（双击选词）-----------------------------------------------------

    /// 返回 `byte` 所在词的字节范围（UAX#29 词边界）。
    /// 若点击位置不在词上（空白/标点），返回空范围。
    /// 边界处优先匹配右侧段（与 VS Code 双击行为一致）。
    pub(crate) fn word_bounds_at(line: &str, byte: usize) -> Range<usize> {
        let byte = byte.min(line.len());
        let segments: Vec<(usize, &str)> = line.split_word_bound_indices().collect();

        for (idx, &(i, seg)) in segments.iter().enumerate() {
            let seg_end = i + seg.len();
            let inside = i <= byte && byte < seg_end;
            let at_start = i == byte && idx > 0;
            if inside || at_start {
                let is_word = seg.chars().any(|c| c.is_alphanumeric() || c == '_');
                if is_word {
                    return i..seg_end;
                }
                return byte..byte;
            }
        }

        // byte 在行尾：检查最后一段
        if let Some(&(i, seg)) = segments.last()
            && i + seg.len() == byte
        {
            let is_word = seg.chars().any(|c| c.is_alphanumeric() || c == '_');
            if is_word {
                return i..i + seg.len();
            }
        }

        byte..byte
    }

    // -- 内部 -----------------------------------------------------------------

    /// 编辑后公共处理：标记 dirty、调度重高亮、清空 redo 栈、重置 saved。
    fn after_edit(&mut self) {
        self.dirty = true;
        self.saved = false;
        self.redo_stack.clear();
        self.schedule_highlight();
    }

    /// 压栈并维护上限。
    fn push_delta(&mut self, delta: EditDelta) {
        self.undo_stack.push(delta);
        if self.undo_stack.len() > MAX_UNDO_STACK {
            self.undo_stack.remove(0);
        }
    }

    /// 尝试与上一个 delta 合并（同行、2s 内、单字符编辑）。
    /// 成功则更新上一个 delta 的 new_lines/caret_after/timestamp。
    fn try_coalesce(
        &mut self,
        line: usize,
        lines: &[String],
        caret_before: ContentPoint,
        caret_after: ContentPoint,
    ) -> bool {
        let Some(last) = self.undo_stack.last_mut() else {
            return false;
        };
        if !last.coalescible {
            return false;
        }
        if last.line_range.start != line || last.line_range.end != line + 1 {
            return false;
        }
        if last.timestamp.elapsed() > COALESCE_WINDOW {
            return false;
        }
        // caret 必须连续（移动后不合并）
        if caret_before != last.caret_after {
            return false;
        }
        last.new_lines = vec![lines[line].clone()];
        last.caret_after = caret_after;
        last.timestamp = Instant::now();
        true
    }
}

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

/// 将文本拆分为行（按 newline_style 去除行尾 \r）。
fn split_lines(text: &str, newline_style: NewlineStyle) -> Vec<String> {
    text.split('\n')
        .map(|s| {
            if newline_style == NewlineStyle::Crlf {
                s.strip_suffix('\r').unwrap_or(s).to_string()
            } else {
                s.to_string()
            }
        })
        .collect()
}

/// 将行 join 为文件内容。
fn join_lines(lines: &[String], newline_style: NewlineStyle) -> String {
    lines.join(newline_style.separator())
}

/// 获取文件权限（Unix mode）。
fn file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).ok().map(|m| m.mode())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// 原子写：临时文件 + fsync + 权限 + rename + 出错清理。
fn atomic_write(
    path: &Path,
    lines: &[String],
    newline_style: NewlineStyle,
    mode: Option<u32>,
) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let tmp_name = format!(
        ".latte-lens-{}-{}.tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let tmp_path = dir.join(&tmp_name);

    let result = (|| -> Result<()> {
        let content = join_lines(lines, newline_style);
        let mut file = fs::File::create(&tmp_path)
            .with_context(|| format!("failed to create temp file {}", tmp_path.display()))?;
        use std::io::Write;
        file.write_all(content.as_bytes())
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", tmp_path.display()))?;

        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))
                .with_context(|| format!("failed to set permissions on {}", tmp_path.display()))?;
        }

        drop(file);
        fs::rename(&tmp_path, path)
            .with_context(|| format!("failed to rename temp to {}", path.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

/// 删除选区，返回 (line_range, old_lines, new_lines)。
/// 调用方负责将 new_lines splice 进 lines（可在 splice 前修改 new_lines）。
fn delete_selection(
    lines: &[String],
    sel: ContentSelection,
) -> (Range<usize>, Vec<String>, Vec<String>) {
    let (start, end) = sel.normalized();
    let start_line = start.line;
    let end_line = end.line.min(lines.len() - 1);

    let old_lines: Vec<String> = lines[start_line..=end_line].to_vec();

    let new_lines = if start_line == end_line {
        let mut new_line = lines[start_line].clone();
        let s = start.byte.min(new_line.len());
        let e = end.byte.min(new_line.len());
        if s < e {
            new_line.replace_range(s..e, "");
        }
        vec![new_line]
    } else {
        let mut merged = String::new();
        merged.push_str(&lines[start_line][..start.byte.min(lines[start_line].len())]);
        merged.push_str(&lines[end_line][end.byte.min(lines[end_line].len())..]);
        vec![merged]
    };

    let line_range = start_line..end_line + 1;
    (line_range, old_lines, new_lines)
}

/// 返回 `byte` 之前一个 grapheme 的起始字节偏移。
fn grapheme_start_before(line: &str, byte: usize) -> usize {
    line[..byte]
        .grapheme_indices(true)
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// 返回 `byte` 之后一个 grapheme 的结束字节偏移。
fn grapheme_end_after(line: &str, byte: usize) -> usize {
    line[byte..]
        .grapheme_indices(true)
        .next()
        .map(|(i, g)| byte + i + g.len())
        .unwrap_or(line.len())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_test_path(name: &str) -> std::path::PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "latte-lens-test-{}-{}-{}.txt",
            name,
            std::process::id(),
            id
        ))
    }

    fn cp(line: usize, byte: usize) -> ContentPoint {
        ContentPoint { line, byte }
    }

    fn make_session(lines: Vec<String>) -> EditSession {
        let snapshot = PreviewSnapshot {
            lines: vec![],
            highlights: vec![],
            scroll: 0,
            horizontal_scroll: 0,
            collapsed_folds: HashSet::new(),
            cursor_line: 0,
            selection: None,
            navigation_caret: NavigationCaret {
                point: crate::navigation::SourcePosition { line: 0, byte: 0 },
                preferred_display_column: 0,
            },
        };
        let mut session = EditSession {
            preview_snapshot: snapshot,
            original_bytes: vec![],
            original_mode: None,
            newline_style: NewlineStyle::Lf,
            caret: cp(0, 0),
            preferred_column: 0,
            selection: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            dirty: false,
            saved: false,
            highlight_due: None,
            large_file: false,
        };
        session.preview_snapshot.lines = lines;
        // Swap into a working buffer
        session
    }

    /// Helper: create a session and swap lines into a working buffer.
    fn session_with_lines(lines: Vec<String>) -> (EditSession, Vec<String>) {
        let mut session = make_session(lines);
        let mut buf = vec![];
        std::mem::swap(&mut session.preview_snapshot.lines, &mut buf);
        (session, buf)
    }

    #[test]
    fn insert_char_basic() {
        let (mut s, mut lines) = session_with_lines(vec!["hello".to_string()]);
        s.caret = cp(0, 0);
        s.insert_char(&mut lines, 'X');
        assert_eq!(lines, vec!["Xhello".to_string()]);
        assert_eq!(s.caret, cp(0, 1));
        assert!(s.dirty);
        assert!(s.can_undo());
    }

    #[test]
    fn insert_char_coalesces_consecutive() {
        let (mut s, mut lines) = session_with_lines(vec!["".to_string()]);
        s.insert_char(&mut lines, 'a');
        s.insert_char(&mut lines, 'b');
        s.insert_char(&mut lines, 'c');
        assert_eq!(lines, vec!["abc".to_string()]);
        assert_eq!(s.undo_stack.len(), 1); // 合并为一个 delta
        assert_eq!(s.caret, cp(0, 3));
    }

    #[test]
    fn insert_char_does_not_coalesce_across_lines() {
        let (mut s, mut lines) = session_with_lines(vec!["".to_string(), "".to_string()]);
        s.caret = cp(0, 0);
        s.insert_char(&mut lines, 'a');
        s.caret = cp(1, 0);
        s.insert_char(&mut lines, 'b');
        assert_eq!(s.undo_stack.len(), 2);
    }

    #[test]
    fn undo_redo_basic() {
        let (mut s, mut lines) = session_with_lines(vec!["".to_string()]);
        s.insert_char(&mut lines, 'a');
        s.insert_char(&mut lines, 'b');
        assert_eq!(lines, vec!["ab".to_string()]);

        s.undo(&mut lines);
        assert_eq!(lines, vec!["".to_string()]);
        assert_eq!(s.caret, cp(0, 0));

        s.redo(&mut lines);
        assert_eq!(lines, vec!["ab".to_string()]);
        assert_eq!(s.caret, cp(0, 2));
    }

    #[test]
    fn undo_restores_after_multiple_deltas() {
        let (mut s, mut lines) = session_with_lines(vec!["hello".to_string()]);
        s.caret = cp(0, 5);
        s.insert_char(&mut lines, '!');
        s.caret = cp(0, 0);
        s.insert_char(&mut lines, 'X');
        assert_eq!(lines, vec!["Xhello!".to_string()]);

        s.undo(&mut lines);
        assert_eq!(lines, vec!["hello!".to_string()]);
        s.undo(&mut lines);
        assert_eq!(lines, vec!["hello".to_string()]);
    }

    #[test]
    fn backspace_basic() {
        let (mut s, mut lines) = session_with_lines(vec!["abc".to_string()]);
        s.caret = cp(0, 3);
        s.backspace(&mut lines);
        assert_eq!(lines, vec!["ab".to_string()]);
        assert_eq!(s.caret, cp(0, 2));
    }

    #[test]
    fn backspace_merges_lines() {
        let (mut s, mut lines) = session_with_lines(vec!["ab".to_string(), "cd".to_string()]);
        s.caret = cp(1, 0);
        s.backspace(&mut lines);
        assert_eq!(lines, vec!["abcd".to_string()]);
        assert_eq!(s.caret, cp(0, 2));
        assert_eq!(s.undo_stack.len(), 1); // 不合并
    }

    #[test]
    fn backspace_at_start_does_nothing() {
        let (mut s, mut lines) = session_with_lines(vec!["a".to_string()]);
        s.caret = cp(0, 0);
        s.backspace(&mut lines);
        assert_eq!(lines, vec!["a".to_string()]);
        assert!(!s.can_undo());
    }

    #[test]
    fn backspace_coalesces() {
        let (mut s, mut lines) = session_with_lines(vec!["abc".to_string()]);
        s.caret = cp(0, 3);
        s.backspace(&mut lines);
        s.backspace(&mut lines);
        assert_eq!(lines, vec!["a".to_string()]);
        assert_eq!(s.undo_stack.len(), 1);
    }

    #[test]
    fn delete_basic() {
        let (mut s, mut lines) = session_with_lines(vec!["abc".to_string()]);
        s.caret = cp(0, 0);
        s.delete(&mut lines);
        assert_eq!(lines, vec!["bc".to_string()]);
        assert_eq!(s.caret, cp(0, 0));
    }

    #[test]
    fn delete_merges_lines() {
        let (mut s, mut lines) = session_with_lines(vec!["ab".to_string(), "cd".to_string()]);
        s.caret = cp(0, 2);
        s.delete(&mut lines);
        assert_eq!(lines, vec!["abcd".to_string()]);
        assert_eq!(s.caret, cp(0, 2));
    }

    #[test]
    fn delete_at_end_does_nothing() {
        let (mut s, mut lines) = session_with_lines(vec!["a".to_string()]);
        s.caret = cp(0, 1);
        s.delete(&mut lines);
        assert_eq!(lines, vec!["a".to_string()]);
        assert!(!s.can_undo());
    }

    #[test]
    fn insert_newline_basic() {
        let (mut s, mut lines) = session_with_lines(vec!["hello".to_string()]);
        s.caret = cp(0, 2);
        s.insert_newline(&mut lines);
        assert_eq!(lines, vec!["he".to_string(), "llo".to_string()]);
        assert_eq!(s.caret, cp(1, 0));
    }

    #[test]
    fn insert_newline_at_start() {
        let (mut s, mut lines) = session_with_lines(vec!["hello".to_string()]);
        s.caret = cp(0, 0);
        s.insert_newline(&mut lines);
        assert_eq!(lines, vec!["".to_string(), "hello".to_string()]);
        assert_eq!(s.caret, cp(1, 0));
    }

    #[test]
    fn undo_newline() {
        let (mut s, mut lines) = session_with_lines(vec!["hello".to_string()]);
        s.caret = cp(0, 2);
        s.insert_newline(&mut lines);
        s.undo(&mut lines);
        assert_eq!(lines, vec!["hello".to_string()]);
        assert_eq!(s.caret, cp(0, 2));
    }

    #[test]
    fn insert_text_single_line() {
        let (mut s, mut lines) = session_with_lines(vec!["".to_string()]);
        s.insert_text(&mut lines, "abc");
        assert_eq!(lines, vec!["abc".to_string()]);
        assert_eq!(s.caret, cp(0, 3));
    }

    #[test]
    fn insert_text_multi_line() {
        let (mut s, mut lines) = session_with_lines(vec!["hello".to_string()]);
        s.caret = cp(0, 2);
        s.insert_text(&mut lines, "x\ny\nz");
        assert_eq!(
            lines,
            vec!["hex".to_string(), "y".to_string(), "zllo".to_string(),]
        );
        assert_eq!(s.caret, cp(2, 1));
    }

    #[test]
    fn insert_text_crlf_normalized() {
        let (mut s, mut lines) = session_with_lines(vec!["".to_string()]);
        s.insert_text(&mut lines, "a\r\nb");
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn selection_replace_with_char() {
        let (mut s, mut lines) = session_with_lines(vec!["hello world".to_string()]);
        s.caret = cp(0, 0);
        // 选中 "hello"
        s.selection = Some(ContentSelection {
            anchor_before: cp(0, 0),
            anchor_after: cp(0, 0),
            head: cp(0, 5),
            dragging: false,
            dragged: true,
        });
        s.insert_char(&mut lines, 'X');
        assert_eq!(lines, vec!["X world".to_string()]);
        assert_eq!(s.caret, cp(0, 1));
        assert_eq!(s.undo_stack.len(), 1); // 选区替换不合并
    }

    #[test]
    fn selection_delete_multi_line() {
        let (mut s, mut lines) = session_with_lines(vec![
            "abc".to_string(),
            "def".to_string(),
            "ghi".to_string(),
        ]);
        s.caret = cp(0, 0);
        // 选中从 (0,1) 到 (2,1)
        s.selection = Some(ContentSelection {
            anchor_before: cp(0, 1),
            anchor_after: cp(0, 1),
            head: cp(2, 1),
            dragging: false,
            dragged: true,
        });
        s.backspace(&mut lines);
        assert_eq!(lines, vec!["ahi".to_string()]);
        assert_eq!(s.caret, cp(0, 1));
    }

    #[test]
    fn caret_left_right() {
        let (mut s, lines) = session_with_lines(vec!["abc".to_string()]);
        s.caret = cp(0, 3);
        s.caret_left(&lines, false);
        assert_eq!(s.caret, cp(0, 2));
        s.caret_left(&lines, false);
        assert_eq!(s.caret, cp(0, 1));
        s.caret_right(&lines, false);
        assert_eq!(s.caret, cp(0, 2));
    }

    #[test]
    fn caret_left_crosses_lines() {
        let (mut s, lines) = session_with_lines(vec!["ab".to_string(), "cd".to_string()]);
        s.caret = cp(1, 0);
        s.caret_left(&lines, false);
        assert_eq!(s.caret, cp(0, 2));
    }

    #[test]
    fn caret_right_crosses_lines() {
        let (mut s, lines) = session_with_lines(vec!["ab".to_string(), "cd".to_string()]);
        s.caret = cp(0, 2);
        s.caret_right(&lines, false);
        assert_eq!(s.caret, cp(1, 0));
    }

    #[test]
    fn caret_home_smart() {
        let (mut s, lines) = session_with_lines(vec!["  hello".to_string()]);
        s.caret = cp(0, 6);
        s.caret_home(&lines, false);
        assert_eq!(s.caret, cp(0, 2)); // 首个非空白
        s.caret_home(&lines, false);
        assert_eq!(s.caret, cp(0, 0)); // 再按到行首
    }

    #[test]
    fn caret_end() {
        let (mut s, lines) = session_with_lines(vec!["hello".to_string()]);
        s.caret = cp(0, 0);
        s.caret_end(&lines, false);
        assert_eq!(s.caret, cp(0, 5));
    }

    #[test]
    fn selection_extension() {
        let (mut s, lines) = session_with_lines(vec!["hello".to_string()]);
        s.caret = cp(0, 0);
        s.caret_right(&lines, true); // Shift+Right
        assert!(s.selection.is_some());
        let sel = s.selection.unwrap();
        assert_eq!(sel.anchor_before, cp(0, 0));
        assert_eq!(sel.head, cp(0, 1));
    }

    #[test]
    fn selection_byte_range_single_line() {
        let mut s = make_session(vec![]);
        s.selection = Some(ContentSelection {
            anchor_before: cp(0, 2),
            anchor_after: cp(0, 2),
            head: cp(0, 5),
            dragging: false,
            dragged: true,
        });
        assert_eq!(s.selection_byte_range(0, 10), Some(2..5));
        assert_eq!(s.selection_byte_range(1, 10), None);
    }

    #[test]
    fn selection_byte_range_multi_line() {
        let mut s = make_session(vec![]);
        s.selection = Some(ContentSelection {
            anchor_before: cp(0, 2),
            anchor_after: cp(0, 2),
            head: cp(2, 3),
            dragging: false,
            dragged: true,
        });
        assert_eq!(s.selection_byte_range(0, 5), Some(2..5));
        assert_eq!(s.selection_byte_range(1, 5), Some(0..5));
        assert_eq!(s.selection_byte_range(2, 5), Some(0..3));
        assert_eq!(s.selection_byte_range(3, 5), None);
    }

    #[test]
    fn word_bounds_basic() {
        assert_eq!(EditSession::word_bounds_at("hello world", 0), 0..5);
        assert_eq!(EditSession::word_bounds_at("hello world", 3), 0..5);
        assert_eq!(EditSession::word_bounds_at("hello world", 7), 6..11);
    }

    #[test]
    fn word_bounds_underscore() {
        assert_eq!(EditSession::word_bounds_at("foo_bar", 0), 0..7);
    }

    #[test]
    fn word_bounds_hyphen() {
        assert_eq!(EditSession::word_bounds_at("foo-bar", 0), 0..3);
        assert_eq!(EditSession::word_bounds_at("foo-bar", 5), 4..7);
    }

    #[test]
    fn word_bounds_cjk() {
        assert_eq!(EditSession::word_bounds_at("你好世界", 0), 0..3); // 每字一词
        assert_eq!(EditSession::word_bounds_at("你好世界", 3), 3..6);
    }

    #[test]
    fn word_bounds_whitespace() {
        assert_eq!(EditSession::word_bounds_at("a b", 1), 1..1); // 空格上无词
    }

    #[test]
    fn word_bounds_punctuation() {
        assert_eq!(EditSession::word_bounds_at("a,b", 1), 1..1); // 逗号上无词
    }

    #[test]
    fn split_lines_lf() {
        let lines = split_lines("a\nb\nc", NewlineStyle::Lf);
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn split_lines_crlf() {
        let lines = split_lines("a\r\nb\r\nc", NewlineStyle::Crlf);
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn split_lines_trailing_newline() {
        let lines = split_lines("a\nb\n", NewlineStyle::Lf);
        assert_eq!(lines, vec!["a", "b", ""]);
    }

    #[test]
    fn split_lines_no_trailing_newline() {
        let lines = split_lines("a\nb", NewlineStyle::Lf);
        assert_eq!(lines, vec!["a", "b"]);
    }

    #[test]
    fn join_roundtrip_lf() {
        let lines = vec!["a".to_string(), "b".to_string()];
        assert_eq!(join_lines(&lines, NewlineStyle::Lf), "a\nb");
    }

    #[test]
    fn join_roundtrip_crlf() {
        let lines = vec!["a".to_string(), "b".to_string()];
        assert_eq!(join_lines(&lines, NewlineStyle::Crlf), "a\r\nb");
    }

    #[test]
    fn join_trailing_newline() {
        let lines = vec!["a".to_string(), "b".to_string(), "".to_string()];
        assert_eq!(join_lines(&lines, NewlineStyle::Lf), "a\nb\n");
    }

    #[test]
    fn atomic_write_and_read() {
        let path = unique_test_path("awr");
        let lines = vec!["hello".to_string(), "world".to_string()];

        atomic_write(&path, &lines, NewlineStyle::Lf, None).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello\nworld");

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn atomic_write_preserves_mode() {
        let path = unique_test_path("mode");
        let lines = vec!["test".to_string()];

        // 创建文件并设置权限
        fs::write(&path, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }

        atomic_write(&path, &lines, NewlineStyle::Lf, file_mode(&path)).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o644);
        }

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn save_conflict_detection() {
        let path = unique_test_path("conflict");
        fs::write(&path, "original").unwrap();

        let snapshot = PreviewSnapshot {
            lines: vec![],
            highlights: vec![],
            scroll: 0,
            horizontal_scroll: 0,
            collapsed_folds: HashSet::new(),
            cursor_line: 0,
            selection: None,
            navigation_caret: NavigationCaret {
                point: crate::navigation::SourcePosition { line: 0, byte: 0 },
                preferred_display_column: 0,
            },
        };
        let mut session = EditSession {
            preview_snapshot: snapshot,
            original_bytes: b"original".to_vec(),
            original_mode: None,
            newline_style: NewlineStyle::Lf,
            caret: cp(0, 0),
            preferred_column: 0,
            selection: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            dirty: true,
            saved: false,
            highlight_due: None,
            large_file: false,
        };

        // 未变更：保存成功
        session.save(&path, &["modified".to_string()]).unwrap();
        assert!(!session.dirty);
        assert_eq!(fs::read_to_string(&path).unwrap(), "modified");

        // 外部修改后保存：冲突
        fs::write(&path, "external change").unwrap();
        let err = session.save(&path, &["again".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("changed on disk"),
            "expected conflict error, got: {err}"
        );

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn undo_stack_cap() {
        let (mut s, mut lines) = session_with_lines(vec!["".to_string()]);
        // 插入超过上限的字符（每个不同行避免合并）
        for i in 0..MAX_UNDO_STACK + 10 {
            lines.push(String::new());
            s.caret = cp(i + 1, 0);
            s.insert_char(&mut lines, 'x');
        }
        assert_eq!(s.undo_stack.len(), MAX_UNDO_STACK);
    }

    #[test]
    fn redo_cleared_on_new_edit() {
        let (mut s, mut lines) = session_with_lines(vec!["".to_string()]);
        s.insert_char(&mut lines, 'a');
        s.undo(&mut lines);
        assert!(s.can_redo());
        s.insert_char(&mut lines, 'b');
        assert!(!s.can_redo());
    }

    #[test]
    fn grapheme_unicode() {
        let (mut s, mut lines) = session_with_lines(vec!["a̐é".to_string()]); // combining chars
        s.caret = cp(0, lines[0].len());
        s.backspace(&mut lines);
        // a̐ = a + combining grave (2 bytes for é... let's just check it deleted something
        assert!(lines[0].len() < "a̐é".len());
    }

    #[test]
    fn preview_snapshot_restore() {
        let s = make_session(vec!["full".to_string()]);
        let snapshot = s.into_preview_snapshot();
        assert_eq!(snapshot.lines, vec!["full".to_string()]);
    }
}
