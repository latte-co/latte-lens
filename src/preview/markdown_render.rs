//! Built-in typeset Markdown preview provider.
//!
//! When a [`PreviewRequest`] opts into
//! [`MarkdownPresentation::Rendered`] for a `.md`/`.markdown` file, this
//! provider transforms the pulldown-cmark event stream into reader-oriented
//! terminal lines: markup (`#`, `**`, `` ` ``, `[]()`, …) is hidden and block
//! structure is conveyed with ordinary Unicode glyphs (`┃`, `•`, `☐`, `─`)
//! and semantic [`HighlightSpan`]s. The output stays a plain
//! [`PreviewKind::Text`] payload without line numbers, so the existing content
//! renderer draws it with no special-casing and folding/navigation (which key
//! off source lines) stay disabled via [`FoldSource::None`].
//!
//! # Bounds and safety
//!
//! Parsing is fail-closed: exceeding the parser event budget or the cooperative
//! deadline makes the provider decline ([`Ok(None)`]) so the registry falls
//! back to the raw source view. Output is independently bounded by the
//! request's `max_bytes`/`max_lines`, reported via `truncated`. All user text is
//! sanitized for terminal control/bidi characters before a span range is
//! recorded, which keeps `highlights[i]` aligned with `lines[i]`. Rendering
//! never opens URLs, fetches images, or interprets HTML: link destinations are
//! dropped, images render as an `[image]` marker plus their alt text, and raw
//! HTML is shown as sanitized text.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use super::common::{ParseBudget, sanitize_terminal_text};
use super::{
    HighlightKind, HighlightSpan, MarkdownPresentation, PreviewContent, PreviewKind,
    PreviewProvider, PreviewRequest,
};

/// Parser events consumed before the document is considered too expensive. This
/// mirrors the Markdown fold/structure passes.
const MAX_RENDER_EVENTS: usize = 50_000;

/// Width, in terminal cells, of the horizontal rule glyph run.
const RULE_WIDTH: usize = 32;

/// Built-in provider that typesets Markdown for reading.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MarkdownRenderProvider;

impl PreviewProvider for MarkdownRenderProvider {
    fn id(&self) -> &'static str {
        "markdown"
    }

    fn preview(&self, request: &PreviewRequest<'_>) -> Result<Option<PreviewContent>> {
        if request.markdown_presentation() != MarkdownPresentation::Rendered {
            return Ok(None);
        }
        if !is_markdown_path(request.display_path) {
            return Ok(None);
        }

        // Read path deliberately mirrors TextPreviewProvider: the vetted
        // regular-file handle, the look-ahead byte for truncation, NUL as a
        // binary signal, and a UTF-8 prefix tolerant of a split codepoint.
        let Some(file) = request.open_regular()? else {
            return Ok(None);
        };
        let read_limit = u64::try_from(request.max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::new();
        file.take(read_limit)
            .read_to_end(&mut bytes)
            .with_context(|| {
                format!(
                    "cannot read Markdown preview {}",
                    request.display_path.display()
                )
            })?;
        if bytes.contains(&0) {
            return Ok(None);
        }
        let truncated_by_bytes = bytes.len() > request.max_bytes;
        bytes.truncate(request.max_bytes);
        let Some(text) = super::utf8_prefix(&bytes, truncated_by_bytes) else {
            return Ok(None);
        };
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);

        render_markdown(text, request, truncated_by_bytes)
    }
}

/// Whether the provider should claim a path. Extension based on purpose: the
/// underlying bytes are still vetted (NUL/UTF-8) by the read path.
fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown"))
}

fn render_markdown(
    text: &str,
    request: &PreviewRequest<'_>,
    input_truncated_by_bytes: bool,
) -> Result<Option<PreviewContent>> {
    render_with_budget(text, request, MAX_RENDER_EVENTS, input_truncated_by_bytes)
}

/// Parse and typeset with an explicit event ceiling. Exposed (module-local) so
/// tests can prove the fail-closed budget path.
///
/// Returns [`None`] only when parsing exceeds its budget/deadline; output
/// truncation still yields [`Some`] with `truncated` set.
fn render_with_budget(
    text: &str,
    request: &PreviewRequest<'_>,
    max_events: usize,
    input_truncated_by_bytes: bool,
) -> Result<Option<PreviewContent>> {
    let mut builder = LineBuilder::new(request);
    builder.input_truncated = input_truncated_by_bytes;
    let mut budget = ParseBudget::new(max_events);
    for (event, _range) in Parser::new_ext(text, Options::all()).into_offset_iter() {
        // One budget tick per event; fail closed to the source view.
        if budget.check().is_err() {
            return Ok(None);
        }
        if builder.sealed {
            break;
        }
        builder.handle(event);
    }
    Ok(Some(builder.finish()))
}

/// Block context currently open, outermost first.
enum Block {
    Paragraph,
    Heading,
    Code,
    Quote,
    List {
        ordered: bool,
        next: u64,
    },
    Item {
        /// Nesting depth (0 = a top-level list), used for indentation.
        depth: usize,
        /// Whether the item's marker has already been emitted.
        marker_emitted: bool,
        /// False until the item's first paragraph starts (tight list spacing).
        first_paragraph: bool,
    },
    Table {
        header: bool,
    },
    TableRow {
        cells: u32,
    },
    TableCell,
    FootnoteDef,
    Metadata,
    DefList,
    DefTitle,
    DefDefinition,
}

/// Active inline style, outermost first.
#[derive(Clone, Copy)]
enum Inline {
    Strong,
    Emphasis,
    Strikethrough,
    Link,
    Image,
}

impl Inline {
    fn kind(self) -> HighlightKind {
        match self {
            Inline::Strong => HighlightKind::MdStrong,
            Inline::Emphasis => HighlightKind::MdEmphasis,
            Inline::Strikethrough => HighlightKind::MdStrikethrough,
            Inline::Link | Inline::Image => HighlightKind::MdLink,
        }
    }
}

struct LineBuilder<'a> {
    request: &'a PreviewRequest<'a>,
    lines: Vec<String>,
    highlights: Vec<Vec<HighlightSpan>>,
    line: String,
    line_spans: Vec<HighlightSpan>,
    blocks: Vec<Block>,
    styles: Vec<Inline>,
    used_bytes: usize,
    /// Real (non-prefix) content has been written to the current line.
    line_contentful: bool,
    /// The current line has not received any content yet, so its prefix is
    /// still pending.
    at_line_start: bool,
    /// At least one physical line was emitted.
    started: bool,
    /// The most recently emitted line was blank (or no line exists yet).
    last_blank: bool,
    truncated: bool,
    /// The input bytes were cut by max_bytes before parsing.
    input_truncated: bool,
    /// Output budget exhausted; stop accepting content.
    sealed: bool,
}

impl<'a> LineBuilder<'a> {
    fn new(request: &'a PreviewRequest<'a>) -> Self {
        Self {
            request,
            lines: Vec::new(),
            highlights: Vec::new(),
            line: String::new(),
            line_spans: Vec::new(),
            blocks: Vec::new(),
            styles: Vec::new(),
            used_bytes: 0,
            line_contentful: false,
            at_line_start: true,
            started: false,
            last_blank: true,
            truncated: false,
            input_truncated: false,
            sealed: false,
        }
    }

    fn finish(mut self) -> PreviewContent {
        // Flush a trailing content line, still honoring the line cap.
        if (self.line_contentful || !self.line.is_empty())
            && self.lines.len() < self.request.max_lines
        {
            self.emit_line();
        } else if self.line_contentful {
            self.truncated = true;
        }
        debug_assert_eq!(self.lines.len(), self.highlights.len());
        let truncated = self.truncated || self.input_truncated;
        PreviewContent {
            lines: self.lines,
            highlights: self.highlights,
            truncated,
            show_line_numbers: false,
            kind: PreviewKind::Text,
        }
    }

    // -- block / event dispatch ---------------------------------------------

    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag_end) => self.end_tag(tag_end),
            Event::Text(text) => self.append_text(&text, self.current_kind(None)),
            Event::Code(text) => {
                self.append_text(&text, self.current_kind(Some(HighlightKind::MdCode)))
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.append_text(&text, self.current_kind(Some(HighlightKind::MdCode)));
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                self.append_text(&text, self.current_kind(Some(HighlightKind::MdRaw)));
            }
            Event::FootnoteReference(label) => {
                self.append_text(&format!("[^{label}]"), None);
            }
            // Soft breaks collapse to a space; hard breaks start a fresh line.
            Event::SoftBreak => self.emit_segment(" ", self.current_kind(None)),
            Event::HardBreak => self.hard_newline(),
            Event::Rule => {
                self.prepare_top_block();
                let rule = "─".repeat(RULE_WIDTH);
                self.emit_segment(&rule, Some(HighlightKind::MdRule));
                self.terminate_line();
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "☑ " } else { "☐ " };
                self.emit_segment(marker, Some(HighlightKind::MdListMarker));
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.begin_paragraph(),
            Tag::Heading { .. } => {
                self.prepare_top_block();
                self.blocks.push(Block::Heading);
            }
            Tag::BlockQuote(_) => {
                self.prepare_top_block();
                self.blocks.push(Block::Quote);
            }
            Tag::CodeBlock(_) => {
                self.prepare_top_block();
                self.blocks.push(Block::Code);
            }
            Tag::HtmlBlock => {
                self.prepare_top_block();
                self.blocks.push(Block::Metadata);
            }
            Tag::List(start) => {
                if self.in_item() {
                    // Nested list: close the preceding paragraph line tightly.
                    if !self.at_line_start {
                        self.hard_newline();
                    }
                } else {
                    self.prepare_top_block();
                }
                self.blocks.push(Block::List {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                });
            }
            Tag::Item => self.begin_item(),
            Tag::FootnoteDefinition(label) => {
                self.prepare_top_block();
                self.blocks.push(Block::FootnoteDef);
                if self.at_line_start {
                    self.write_quote_bars();
                }
                self.emit_segment(&format!("[^{label}] "), None);
            }
            Tag::DefinitionList => {
                self.prepare_top_block();
                self.blocks.push(Block::DefList);
            }
            Tag::DefinitionListTitle => {
                if !self.at_line_start {
                    self.hard_newline();
                }
                self.blocks.push(Block::DefTitle);
            }
            Tag::DefinitionListDefinition => {
                if !self.at_line_start {
                    self.hard_newline();
                }
                self.blocks.push(Block::DefDefinition);
                if self.at_line_start {
                    self.write_quote_bars();
                }
                self.emit_segment(": ", None);
            }
            Tag::Table(_) => {
                self.prepare_top_block();
                self.blocks.push(Block::Table { header: false });
            }
            Tag::TableHead => {
                // The header row is not wrapped in a TableRow event, so push a
                // row block ourselves to share cell counting/separators.
                if let Some(Block::Table { header }) = self
                    .blocks
                    .iter_mut()
                    .rev()
                    .find(|b| matches!(b, Block::Table { .. }))
                {
                    *header = true;
                }
                self.prepare_top_block();
                self.blocks.push(Block::TableRow { cells: 0 });
            }
            Tag::TableRow => {
                if !self.at_line_start {
                    self.hard_newline();
                }
                self.blocks.push(Block::TableRow { cells: 0 });
            }
            Tag::TableCell => {
                // Emit the inter-cell separator for every cell after the first,
                // finishing the row counter borrow before calling back into
                // `self` for emission.
                let preceding_cells = self.blocks.iter_mut().rev().find_map(|b| match b {
                    Block::TableRow { cells } => Some(cells),
                    _ => None,
                });
                let need_separator = if let Some(cells) = preceding_cells {
                    let was_non_first = *cells > 0;
                    *cells += 1;
                    was_non_first
                } else {
                    false
                };
                if need_separator {
                    self.emit_segment(" │ ", Some(HighlightKind::MdRule));
                }
                self.blocks.push(Block::TableCell);
            }
            Tag::Strong => self.styles.push(Inline::Strong),
            Tag::Emphasis => self.styles.push(Inline::Emphasis),
            Tag::Strikethrough => self.styles.push(Inline::Strikethrough),
            Tag::Link { .. } => self.styles.push(Inline::Link),
            Tag::Image { .. } => {
                self.styles.push(Inline::Image);
                self.emit_segment("[image] ", Some(HighlightKind::MdLink));
            }
            // No dedicated styling for super/subscript yet; text stays plain.
            Tag::Superscript | Tag::Subscript => {}
            Tag::MetadataBlock(_) => {
                self.prepare_top_block();
                self.blocks.push(Block::Metadata);
            }
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                // Cell paragraphs do not terminate the physical table row.
                if !self.in_table_cell() {
                    self.terminate_line();
                }
                self.pop_block();
            }
            TagEnd::Heading(_) => {
                self.terminate_line();
                self.pop_block();
            }
            TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::MetadataBlock(_) => {
                if !self.at_line_start {
                    self.terminate_line();
                }
                self.pop_block();
            }
            TagEnd::Item
            | TagEnd::List(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::Table
            | TagEnd::DefinitionList => self.pop_block(),
            TagEnd::TableRow => {
                self.pop_block();
                self.hard_newline();
            }
            TagEnd::TableCell => self.pop_block(),
            TagEnd::TableHead => {
                self.hard_newline();
                // Pop the row block pushed by Start(TableHead).
                self.blocks.pop();
                if let Some(Block::Table { header }) = self
                    .blocks
                    .iter_mut()
                    .rev()
                    .find(|b| matches!(b, Block::Table { .. }))
                {
                    *header = false;
                }
            }
            TagEnd::DefinitionListTitle | TagEnd::DefinitionListDefinition => {
                self.terminate_line();
                self.pop_block();
            }
            TagEnd::Strong
            | TagEnd::Emphasis
            | TagEnd::Strikethrough
            | TagEnd::Link
            | TagEnd::Image => {
                self.styles.pop();
            }
            TagEnd::Superscript | TagEnd::Subscript => {}
        }
    }

    // -- paragraph / list spacing -------------------------------------------

    fn begin_paragraph(&mut self) {
        if self.in_table_cell() {
            self.blocks.push(Block::Paragraph);
            return;
        }
        if let Some(Block::Item {
            first_paragraph, ..
        }) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(b, Block::Item { .. }))
        {
            if *first_paragraph {
                // Tight first paragraph: text follows the marker directly.
                *first_paragraph = false;
            } else if !self.at_line_start {
                // Loose list: subsequent paragraphs wrap with indentation.
                self.hard_newline();
            }
        } else if matches!(self.blocks.last(), Some(Block::FootnoteDef)) && !self.at_line_start {
            // The definition's first paragraph follows the `[^label] ` marker
            // on the same line; later paragraphs fall through to normal block
            // spacing once the marker line has been terminated.
        } else {
            self.prepare_top_block();
        }
        self.blocks.push(Block::Paragraph);
    }

    fn begin_item(&mut self) {
        if !self.at_line_start {
            self.hard_newline();
        }
        let depth = self
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::List { .. }))
            .count()
            .saturating_sub(1);
        self.blocks.push(Block::Item {
            depth,
            marker_emitted: false,
            first_paragraph: true,
        });

        // The marker line: quote bars, nesting indent, then the glyph.
        if self.at_line_start {
            self.write_quote_bars();
            self.at_line_start = false;
        }
        let indent = "  ".repeat(depth);
        self.emit_raw(&indent, None);
        let marker = self.next_item_marker();
        self.emit_raw(&marker, Some(HighlightKind::MdListMarker));
        if let Some(Block::Item { marker_emitted, .. }) = self.blocks.last_mut() {
            *marker_emitted = true;
        }
    }

    /// Compute and advance the ordered/unordered marker for the current item.
    fn next_item_marker(&mut self) -> String {
        if let Some(Block::List { ordered, next }) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(b, Block::List { .. }))
        {
            if *ordered {
                let marker = format!("{next}. ");
                *next = next.saturating_add(1);
                marker
            } else {
                "• ".to_owned()
            }
        } else {
            "• ".to_owned()
        }
    }

    // -- line lifecycle ------------------------------------------------------

    /// Blank-line separation for a top-level block; tighter inside list items.
    fn prepare_top_block(&mut self) {
        if self.in_item() {
            if !self.at_line_start {
                self.hard_newline();
            }
        } else {
            self.blank_line();
        }
    }

    fn blank_line(&mut self) {
        if self.sealed {
            return;
        }
        if self.line_contentful {
            self.terminate_line();
        }
        if self.started && !self.last_blank {
            // Inside a quote, the bar column stays continuous across blanks.
            if self.at_line_start {
                self.write_quote_bars();
            }
            self.emit_line();
        }
    }

    fn hard_newline(&mut self) {
        if self.sealed {
            return;
        }
        self.terminate_line();
    }

    /// Flush the current line as-is and reset to a fresh, prefixed line.
    fn terminate_line(&mut self) {
        // Once the output budget has sealed the builder, block closers must
        // not push trailing empty lines. A pending line whose content already
        // fit the budget is still flushed exactly once.
        if self.sealed && self.line.is_empty() && !self.line_contentful {
            return;
        }
        if self.lines.len() >= self.request.max_lines {
            self.truncated = true;
            self.sealed = true;
            self.line.clear();
            self.line_spans.clear();
            self.line_contentful = false;
            return;
        }
        let line = std::mem::take(&mut self.line);
        let spans = std::mem::take(&mut self.line_spans);
        self.last_blank = !self.line_contentful;
        self.highlights.push(spans);
        self.lines.push(line);
        self.line_contentful = false;
        self.at_line_start = true;
        self.started = true;
    }

    /// Internal alias used where a blank (prefix-only) line is emitted directly.
    fn emit_line(&mut self) {
        self.terminate_line();
    }

    /// Write the quote bar prefix for one physical line (`┃ ` per depth).
    fn write_quote_bars(&mut self) {
        let depth = self
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Quote))
            .count();
        for _ in 0..depth {
            self.emit_raw("┃ ", Some(HighlightKind::MdQuote));
        }
    }

    /// Continuation indentation after a hard break, aligned to the item text.
    fn write_item_continuation(&mut self) {
        if self.blocks.iter().any(|b| matches!(b, Block::Code)) {
            return;
        }
        // Copy the depth out so the immutable borrow ends before emission.
        let Some(depth) = self.blocks.iter().rev().find_map(|b| match b {
            Block::Item {
                depth,
                marker_emitted: true,
                ..
            } => Some(*depth),
            _ => None,
        }) else {
            return;
        };
        // Marker glyphs are two cells wide; align wrapped text to them.
        self.emit_raw(&"  ".repeat(depth + 1), None);
    }

    // -- text emission -------------------------------------------------------

    /// Append user/leaf text that may itself contain newlines (code blocks,
    /// hard-wrapped source). Each newline-delimited piece is sanitized
    /// separately because the sanitizer escapes raw `\n`.
    fn append_text(&mut self, raw: &str, kind: Option<HighlightKind>) {
        if self.sealed {
            return;
        }
        for (index, piece) in raw.split('\n').enumerate() {
            if index > 0 {
                self.hard_newline();
            }
            let cleaned = sanitize_terminal_text(piece);
            self.emit_segment(&cleaned, kind);
        }
    }

    /// Emit an already-safe fragment (glyph prefix, marker, rule, space),
    /// applying the lazy line prefix first.
    fn emit_segment(&mut self, text: &str, kind: Option<HighlightKind>) {
        if self.sealed || text.is_empty() {
            return;
        }
        if self.at_line_start {
            self.write_quote_bars();
            self.write_item_continuation();
            self.at_line_start = false;
        }
        self.emit_raw(text, kind);
    }

    /// Low-level append: byte-bounded, char-boundary-safe, span-aligned.
    fn emit_raw(&mut self, text: &str, kind: Option<HighlightKind>) {
        if self.sealed || text.is_empty() {
            return;
        }
        let remaining = self.request.max_bytes.saturating_sub(self.used_bytes);
        let (text, cut) = truncate_char_boundary(text, remaining);
        if text.is_empty() {
            self.truncated = true;
            self.sealed = true;
            return;
        }
        let start = self.line.len();
        self.line.push_str(text);
        if let Some(kind) = kind {
            self.line_spans.push(HighlightSpan {
                range: start..self.line.len(),
                kind,
            });
        }
        self.used_bytes = self.used_bytes.saturating_add(text.len());
        self.line_contentful = true;
        if cut {
            self.truncated = true;
            self.sealed = true;
        }
    }

    // -- style resolution ----------------------------------------------------

    /// Resolve the effective single kind for a leaf. Span resolution is
    /// last-wins and non-composing, so nested markup collapses to one kind by
    /// priority: code block > explicit leaf (inline code/raw HTML) > link/image
    /// > table header/heading > definition title > strong/em/strike.
    fn current_kind(&self, leaf: Option<HighlightKind>) -> Option<HighlightKind> {
        if self.blocks.iter().any(|b| matches!(b, Block::Code)) {
            return Some(HighlightKind::MdCodeBlock);
        }
        if leaf.is_some() {
            return leaf;
        }
        if self
            .styles
            .iter()
            .rev()
            .copied()
            .map(Inline::kind)
            .any(|kind| kind == HighlightKind::MdLink)
        {
            return Some(HighlightKind::MdLink);
        }
        if self.in_table_header() || self.blocks.iter().any(|b| matches!(b, Block::Heading)) {
            return Some(HighlightKind::MdHeading);
        }
        if self.blocks.iter().any(|b| matches!(b, Block::DefTitle)) {
            return Some(HighlightKind::MdStrong);
        }
        // Link/image are already handled above; skip them here so a nested
        // strong/em still resolves when no link is present.
        self.styles
            .iter()
            .rev()
            .copied()
            .map(Inline::kind)
            .find(|kind| *kind != HighlightKind::MdLink)
    }

    // -- block queries -------------------------------------------------------

    fn pop_block(&mut self) {
        self.blocks.pop();
    }

    fn in_item(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::Item { .. }))
    }

    fn in_table_cell(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::TableCell))
    }

    fn in_table_header(&self) -> bool {
        self.blocks
            .iter()
            .any(|b| matches!(b, Block::Table { header: true }))
    }
}

fn truncate_char_boundary(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::{PreviewRegistry, PreviewResolution};
    use std::path::Path;
    use tempfile::tempdir;

    const MAX_BYTES: usize = 512 * 1024;
    const MAX_LINES: usize = 2_000;

    /// Resolve through the real registry so provider ordering and fall-back
    /// are exercised.
    fn resolve(
        source: &str,
        file_name: &str,
        presentation: MarkdownPresentation,
        max_bytes: usize,
        max_lines: usize,
    ) -> (String, PreviewContent) {
        let dir = tempdir().unwrap();
        let path = dir.path().join(file_name);
        std::fs::write(&path, source).unwrap();
        let registry = PreviewRegistry::with_builtins();
        let request = PreviewRequest::new(&path, Path::new(file_name))
            .with_limits(max_bytes, max_lines)
            .with_markdown_presentation(presentation);
        let PreviewResolution::Preview { preview, .. } =
            registry.resolve(&request).expect("registry resolution")
        else {
            panic!("expected a preview");
        };
        (
            preview.provider_id,
            PreviewContent {
                lines: preview.lines,
                highlights: preview.highlights,
                truncated: preview.truncated,
                show_line_numbers: preview.show_line_numbers,
                kind: preview.kind,
            },
        )
    }

    fn rendered(source: &str) -> PreviewContent {
        let (provider, content) = resolve(
            source,
            "doc.md",
            MarkdownPresentation::Rendered,
            MAX_BYTES,
            MAX_LINES,
        );
        assert_eq!(provider, "markdown");
        content
    }

    /// Render with an artificially small event budget.
    fn rendered_with_event_budget(source: &str, max_events: usize) -> Option<PreviewContent> {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        std::fs::write(&path, source).unwrap();
        let request =
            PreviewRequest::new(&path, Path::new("doc.md")).with_limits(MAX_BYTES, MAX_LINES);
        render_with_budget(source, &request, max_events, false).unwrap()
    }

    fn kinds_of(content: &PreviewContent, line_index: usize) -> Vec<HighlightKind> {
        content.highlights[line_index]
            .iter()
            .map(|span| span.kind)
            .collect()
    }

    #[test]
    fn declines_when_presentation_is_source_and_falls_back_to_text() {
        let (provider, content) = resolve(
            "# Hi",
            "doc.md",
            MarkdownPresentation::Source,
            MAX_BYTES,
            MAX_LINES,
        );
        assert_eq!(provider, "text");
        assert!(content.show_line_numbers);
    }

    #[test]
    fn declines_non_markdown_extensions() {
        for name in ["doc.txt", "doc.mdx", "doc", "doc.MD.bak"] {
            let (provider, _) = resolve(
                "# Hi",
                name,
                MarkdownPresentation::Rendered,
                MAX_BYTES,
                MAX_LINES,
            );
            assert_eq!(provider, "text", "{name} should not be claimed");
        }
    }

    #[test]
    fn accepts_md_and_markdown_case_insensitively() {
        for name in ["doc.md", "doc.MD", "doc.MarkDown", "notes.markdown"] {
            let (provider, content) = resolve(
                "# Hi",
                name,
                MarkdownPresentation::Rendered,
                MAX_BYTES,
                MAX_LINES,
            );
            assert_eq!(provider, "markdown", "{name}");
            assert!(!content.show_line_numbers);
            assert_eq!(content.kind, PreviewKind::Text);
        }
    }

    #[test]
    fn heading_hides_hash_and_emits_heading_span() {
        let c = rendered("# Title here\n");
        assert_eq!(c.lines[0], "Title here");
        assert!(c.lines.iter().all(|l| !l.contains('#')));
        let spans = &c.highlights[0];
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, HighlightKind::MdHeading);
        assert_eq!(&c.lines[0][spans[0].range.clone()], "Title here");
    }

    #[test]
    fn soft_break_becomes_space_and_hard_break_newline() {
        let c = rendered("one\ntwo\\\nthree\n");
        // Hard break splits; soft break stays within a paragraph.
        assert!(
            c.lines
                .iter()
                .any(|l| l.contains("one two") || l == "one two")
        );
        assert!(c.lines.iter().any(|l| l.trim_end() == "three"));
    }

    #[test]
    fn inline_strong_emphasis_strike_and_code() {
        let c = rendered("a **bold** b *em* c ~~no~~ d `code`\n");
        let joined = c.lines.join("\n");
        assert!(joined.contains("bold") && !joined.contains("**"));
        assert!(joined.contains("em") && !joined.contains('*'));
        assert!(joined.contains("no") && !joined.contains("~~"));
        assert!(joined.contains("code") && !joined.contains('`'));
        let all: Vec<HighlightKind> = c.highlights.iter().flatten().map(|s| s.kind).collect();
        assert!(all.contains(&HighlightKind::MdStrong));
        assert!(all.contains(&HighlightKind::MdEmphasis));
        assert!(all.contains(&HighlightKind::MdStrikethrough));
        assert!(all.contains(&HighlightKind::MdCode));
    }

    #[test]
    fn fenced_code_block_hides_fence_and_language() {
        let c = rendered("```rust\nlet x = 1;\n```\n");
        assert!(c.lines.iter().all(|l| !l.contains("```")));
        assert!(c.lines.iter().all(|l| l.trim() != "rust"));
        assert!(c.lines.contains(&"let x = 1;".to_owned()));
        assert!(kinds_of(&c, 0).contains(&HighlightKind::MdCodeBlock));
    }

    #[test]
    fn link_keeps_text_drops_destination() {
        let c = rendered("see [the docs](https://example.com/page) now\n");
        let joined = c.lines.join("\n");
        assert!(joined.contains("the docs"));
        assert!(!joined.contains("example.com"));
        let link_span = c
            .highlights
            .iter()
            .flatten()
            .find(|s| s.kind == HighlightKind::MdLink)
            .expect("a link span");
        let linked_line = &c.lines[0];
        assert_eq!(&linked_line[link_span.range.clone()], "the docs");
    }

    #[test]
    fn blockquote_bar_single_and_nested() {
        let c = rendered("> one\n>\n> > deep\n");
        assert_eq!(c.lines[0], "┃ one");
        assert!(c.lines.iter().any(|l| l == "┃ "));
        assert!(c.lines.iter().any(|l| l == "┃ ┃ deep"));
        assert!(
            c.highlights
                .iter()
                .flatten()
                .any(|s| s.kind == HighlightKind::MdQuote)
        );
    }

    #[test]
    fn unordered_nested_and_ordered_task_lists() {
        let c = rendered("- a\n  - b\n\n3. c\n4. d\n\n- [ ] open\n- [x] shut\n");
        let joined = c.lines.join("\n");
        assert!(joined.contains("• a"));
        assert!(joined.contains("  • b"));
        assert!(joined.contains("3. c"));
        assert!(joined.contains("4. d"));
        assert!(joined.contains("• ☐ open"));
        assert!(joined.contains("• ☑ shut"));
        assert!(
            c.highlights
                .iter()
                .flatten()
                .any(|s| s.kind == HighlightKind::MdListMarker)
        );
    }

    #[test]
    fn horizontal_rule_is_a_run_of_dashes() {
        let c = rendered("---\n");
        assert_eq!(c.lines[0], "─".repeat(32));
        assert_eq!(c.highlights[0][0].kind, HighlightKind::MdRule);
    }

    #[test]
    fn table_keeps_separator_and_bolds_header() {
        let c = rendered("| A | B |\n|---|---|\n| 1 | 2 |\n");
        assert_eq!(c.lines[0], "A │ B");
        assert_eq!(c.lines[1], "1 │ 2");
        assert!(kinds_of(&c, 0).contains(&HighlightKind::MdHeading));
        assert!(kinds_of(&c, 0).contains(&HighlightKind::MdRule));
    }

    #[test]
    fn image_shows_marker_and_alt_only() {
        let c = rendered("![diagram](https://x/y.png)\n");
        let joined = c.lines.join("\n");
        assert!(joined.contains("[image] diagram"));
        assert!(!joined.contains("y.png"));
    }

    #[test]
    fn html_renders_as_raw_text() {
        let c = rendered("<div class=\"x\">hi</div>\n");
        let joined = c.lines.join("\n");
        assert!(joined.contains("div"));
        assert!(
            c.highlights
                .iter()
                .flatten()
                .any(|s| s.kind == HighlightKind::MdRaw)
        );
    }

    #[test]
    fn control_characters_are_sanitized_but_spans_stay_aligned() {
        // Include a semantic span so the per-span alignment assertion actually
        // runs against sanitized, span-bearing lines.
        let c = rendered("a\t**b**\x1bc\n");
        for (line, spans) in c.lines.iter().zip(&c.highlights) {
            for span in spans {
                assert!(line.get(span.range.clone()).is_some());
            }
        }
        assert!(c.lines.join("").contains("    "));
        assert!(c.lines.join("").contains("<U+001B>"));
    }

    #[test]
    fn nul_byte_declines_to_text_provider() {
        // NUL bypasses the markdown renderer and is a binary signal for text
        // too, so resolution becomes Unsupported (no panic).
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        std::fs::write(&path, b"# a\0b").unwrap();
        let registry = PreviewRegistry::with_builtins();
        let request = PreviewRequest::new(&path, Path::new("doc.md"))
            .with_limits(MAX_BYTES, MAX_LINES)
            .with_markdown_presentation(MarkdownPresentation::Rendered);
        assert!(matches!(
            registry.resolve(&request).unwrap(),
            PreviewResolution::Unsupported
        ));
    }

    #[test]
    fn byte_limit_truncates_at_char_boundary() {
        let (_, c) = resolve(
            "# 拿x",
            "doc.md",
            MarkdownPresentation::Rendered,
            3,
            MAX_LINES,
        );
        assert!(c.truncated);
        // Every line remains valid UTF-8.
        for line in &c.lines {
            assert!(std::str::from_utf8(line.as_bytes()).is_ok());
        }
    }

    #[test]
    fn line_limit_sets_truncated() {
        let source: String = (0..50).map(|i| format!("- item {i}\n")).collect();
        let (_, c) = resolve(
            &source,
            "doc.md",
            MarkdownPresentation::Rendered,
            MAX_BYTES,
            10,
        );
        assert!(c.truncated);
        assert!(c.lines.len() <= 10);
        assert_eq!(c.lines.len(), c.highlights.len());
    }

    #[test]
    fn event_budget_exceeded_declines() {
        let source: String = (0..10_000).map(|i| format!("- {i}\n")).collect();
        assert!(rendered_with_event_budget(&source, 5).is_none());
        // A healthy budget renders the same document.
        assert!(rendered_with_event_budget(&source, MAX_RENDER_EVENTS).is_some());
    }

    #[test]
    fn spans_always_align_with_lines_and_are_utf8_boundaries() {
        let source = concat!(
            "# H\n\n",
            "p **b** *i* `c` [l](u) ![a](i.png) ~~s~~\n\n",
            "> q1\n> q2\n\n",
            "- x\n  - y\n\n```\ncode\n```\n\n",
            "| H1 | H2 |\n|----|----|\n| a  | b  |\n\n",
            "1. n\n2. m\n\n- [ ] t\n\n---\n\n[^1]\n",
        );
        let c = rendered(source);
        assert_eq!(c.lines.len(), c.highlights.len());
        for (line, spans) in c.lines.iter().zip(&c.highlights) {
            for span in spans {
                let text = line
                    .get(span.range.clone())
                    .expect("span range inside line");
                assert!(!text.is_empty());
                assert!(std::str::from_utf8(text.as_bytes()).is_ok());
            }
        }
    }

    #[test]
    fn empty_document_is_empty() {
        let c = rendered("");
        assert!(c.lines.is_empty());
        assert!(c.highlights.is_empty());
        assert!(!c.truncated);
    }

    #[test]
    fn bom_is_stripped_before_heading() {
        let c = rendered("\u{feff}# Title\n");
        assert_eq!(c.lines[0], "Title");
        assert!(c.lines[0].chars().all(|ch| ch != '\u{feff}'));
    }

    #[test]
    fn footnote_reference_and_definition_render() {
        let c = rendered("see [^one] now\n\n[^one]: the note body\n");
        let joined = c.lines.join("\n");
        assert!(joined.contains("see [^one] now"));
        assert!(joined.contains("[^one] the note body"));
    }

    #[test]
    fn definition_list_renders_title_and_definition() {
        let c = rendered("term\n: definition body\n");
        assert!(c.lines.iter().any(|l| l == "term"));
        assert!(c.lines.iter().any(|l| l.contains(": definition body")));
        // The title picks up the strong/heading-like emphasis.
        let title_line = c
            .lines
            .iter()
            .position(|l| l == "term")
            .expect("title line");
        assert!(kinds_of(&c, title_line).contains(&HighlightKind::MdStrong));
    }

    #[test]
    fn metadata_block_renders_as_plain_text() {
        let c = rendered("---\ntitle: T\n---\n\nbody\n");
        let joined = c.lines.join("\n");
        assert!(joined.contains("title: T"));
        assert!(joined.contains("body"));
    }

    #[test]
    fn inline_and_display_math_use_code_kind() {
        let c = rendered("$a^2$ then\n$$\nx\n$$\n");
        let all: Vec<HighlightKind> = c.highlights.iter().flatten().map(|s| s.kind).collect();
        assert!(all.contains(&HighlightKind::MdCode));
        // Display math with its internal newline becomes its own code line.
        assert!(c.lines.iter().any(|l| l.trim() == "x"));
    }

    #[test]
    fn multiline_block_html_is_raw() {
        let c = rendered("<div>\n  hi\n</div>\n");
        let joined = c.lines.join("\n");
        assert!(joined.contains("hi"));
        assert!(
            c.highlights
                .iter()
                .flatten()
                .any(|s| s.kind == HighlightKind::MdRaw)
        );
    }

    #[test]
    fn loose_list_follow_up_paragraph_wraps_with_indent() {
        // Tight nested list, a paragraph after it, and block siblings inside
        // an item (quote, code block) exercise item-relative block spacing.
        let c = rendered("- p1\n\n  p2 continued\n\n- a\n  - b\n\n  c after nested\n");
        let joined = c.lines.join("\n");
        assert!(joined.contains("• p1"));
        assert!(joined.contains("p2 continued"));
        assert!(joined.contains("c after nested"));
    }

    #[test]
    fn block_siblings_inside_list_item_stay_tight() {
        let c = rendered("- a\n  > quoted in item\n\n- q\n  ```\n  xline\n  ```\n");
        let joined = c.lines.join("\n");
        assert!(joined.contains("• a"));
        // The nested quote keeps its bar and aligns to the item text column.
        assert!(
            c.lines
                .iter()
                .any(|l| l.starts_with("┃") && l.contains("quoted in item"))
        );
        assert!(joined.contains("• q"));
        // Code-block content inside an item gets no continuation indent.
        assert!(c.lines.iter().any(|l| l == "xline"));
    }

    #[test]
    fn hard_break_inside_list_item_indents_continuation() {
        let c = rendered("- one\\\ntwo\n");
        assert!(c.lines.iter().any(|l| l == "• one"));
        assert!(c.lines.iter().any(|l| l == "  two"));
    }

    #[test]
    fn byte_budget_filled_exactly_then_extra_leaf_truncates() {
        // "ab" consumes the whole output budget; the following code leaf gets
        // nothing and the builder seals without dropping span alignment.
        let (_, c) = resolve(
            "ab`c`\n",
            "doc.md",
            MarkdownPresentation::Rendered,
            2,
            MAX_LINES,
        );
        assert!(c.truncated);
        assert_eq!(c.lines.join(""), "ab");
    }

    #[test]
    fn code_block_output_cut_inside_leaf_seals_aligned() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        let request = PreviewRequest::new(&path, Path::new("doc.md")).with_limits(12, MAX_LINES);
        let mut builder = LineBuilder::new(&request);
        builder.handle(Event::Start(Tag::CodeBlock(
            pulldown_cmark::CodeBlockKind::Fenced("".into()),
        )));
        // 10 bytes for line one, two bytes of "BBBB" fit line two before seal.
        builder.handle(Event::Text("AAAAAAAAAA\nBBBB".into()));
        builder.handle(Event::End(TagEnd::CodeBlock));
        let c = builder.finish();
        assert!(c.truncated);
        assert_eq!(c.lines, vec!["AAAAAAAAAA".to_string(), "BB".to_string()]);
        assert_eq!(c.lines.len(), c.highlights.len());
        assert!(
            c.highlights
                .iter()
                .flatten()
                .all(|s| s.kind == HighlightKind::MdCodeBlock)
        );
    }

    #[test]
    fn code_block_exact_fill_then_sealed_events_emit_nothing_more() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        let request = PreviewRequest::new(&path, Path::new("doc.md")).with_limits(10, MAX_LINES);
        let mut builder = LineBuilder::new(&request);
        builder.handle(Event::Start(Tag::CodeBlock(
            pulldown_cmark::CodeBlockKind::Fenced("".into()),
        )));
        builder.handle(Event::Text("AAAAAAAAAA\nB".into()));
        // After the zero-width leaf seals, later events must add no lines.
        builder.handle(Event::End(TagEnd::CodeBlock));
        builder.handle(Event::Start(Tag::Paragraph));
        builder.handle(Event::Text("late".into()));
        builder.handle(Event::End(TagEnd::Paragraph));
        let c = builder.finish();
        assert!(c.truncated);
        assert_eq!(c.lines, vec!["AAAAAAAAAA".to_string()]);
    }

    #[test]
    fn html_block_without_trailing_newline_terminates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        let request =
            PreviewRequest::new(&path, Path::new("doc.md")).with_limits(MAX_BYTES, MAX_LINES);
        let mut builder = LineBuilder::new(&request);
        builder.handle(Event::Start(Tag::HtmlBlock));
        builder.handle(Event::Html("<div>x</div>".into()));
        builder.handle(Event::End(TagEnd::HtmlBlock));
        let c = builder.finish();
        assert_eq!(c.lines, vec!["<div>x</div>".to_string()]);
        assert_eq!(c.highlights[0][0].kind, HighlightKind::MdRaw);
    }

    #[test]
    fn line_cap_with_pending_content_marks_truncated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        let request = PreviewRequest::new(&path, Path::new("doc.md")).with_limits(MAX_BYTES, 1);
        let mut builder = LineBuilder::new(&request);
        builder.handle(Event::Start(Tag::Paragraph));
        builder.handle(Event::Text("a".into()));
        builder.handle(Event::End(TagEnd::Paragraph));
        // A trailing leaf after the line cap was reached stays pending.
        builder.handle(Event::Text("b".into()));
        let c = builder.finish();
        assert!(c.truncated);
        assert_eq!(c.lines, vec!["a".to_string()]);
    }

    #[test]
    fn sup_sub_events_render_plain_and_kind_resolution() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        let request =
            PreviewRequest::new(&path, Path::new("doc.md")).with_limits(MAX_BYTES, MAX_LINES);
        let mut builder = LineBuilder::new(&request);
        builder.handle(Event::Start(Tag::Superscript));
        builder.handle(Event::Text("2".into()));
        builder.handle(Event::End(TagEnd::Superscript));
        builder.handle(Event::Start(Tag::Subscript));
        builder.handle(Event::Text("j".into()));
        builder.handle(Event::End(TagEnd::Subscript));
        let content = builder.finish();
        assert_eq!(content.lines, vec!["2j".to_string()]);
        assert!(content.highlights[0].is_empty());

        let request =
            PreviewRequest::new(&path, Path::new("doc.md")).with_limits(MAX_BYTES, MAX_LINES);
        let mut builder = LineBuilder::new(&request);
        builder.blocks.push(Block::DefTitle);
        assert_eq!(builder.current_kind(None), Some(HighlightKind::MdStrong));
        builder.blocks.pop();
        builder.styles.push(Inline::Strikethrough);
        assert_eq!(
            builder.current_kind(None),
            Some(HighlightKind::MdStrikethrough)
        );
        builder.styles.pop();
        builder.styles.push(Inline::Emphasis);
        assert_eq!(builder.current_kind(None), Some(HighlightKind::MdEmphasis));
    }

    #[test]
    fn truncate_char_boundary_helper() {
        assert_eq!(truncate_char_boundary("abc", 10), ("abc", false));
        assert_eq!(truncate_char_boundary("abc", 2), ("ab", true));
        // 😀 spans bytes 2..6; cuts at 3/4/5 move back to the prior boundary.
        assert_eq!(truncate_char_boundary("ab😀", 3), ("ab", true));
        assert_eq!(truncate_char_boundary("ab😀", 4), ("ab", true));
        assert_eq!(truncate_char_boundary("ab😀", 5), ("ab", true));
        assert_eq!(truncate_char_boundary("ab😀", 6), ("ab😀", false));
    }

    #[test]
    fn markdown_extension_on_a_directory_is_unsafe() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        std::fs::create_dir(&path).unwrap();
        let registry = PreviewRegistry::with_builtins();
        let request = PreviewRequest::new(&path, Path::new("doc.md"))
            .with_limits(MAX_BYTES, MAX_LINES)
            .with_markdown_presentation(MarkdownPresentation::Rendered);
        assert!(matches!(
            registry.resolve(&request).unwrap(),
            PreviewResolution::Unsafe { .. }
        ));
    }

    #[test]
    fn invalid_utf8_within_limit_declines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        std::fs::write(&path, b"# \xff\n").unwrap();
        let registry = PreviewRegistry::with_builtins();
        let request = PreviewRequest::new(&path, Path::new("doc.md"))
            .with_limits(MAX_BYTES, MAX_LINES)
            .with_markdown_presentation(MarkdownPresentation::Rendered);
        assert!(matches!(
            registry.resolve(&request).unwrap(),
            PreviewResolution::Unsupported
        ));
    }

    #[test]
    fn rule_inside_list_item_uses_item_relative_spacing() {
        let c = rendered("- a\n\n  ---\n");
        let joined = c.lines.join("\n");
        assert!(joined.contains("• a"));
        assert!(joined.contains("─".repeat(8).as_str()));
    }

    #[test]
    fn definition_blocks_joining_an_open_line_wrap_first() {
        // pulldown normally terminates a title before the next block starts,
        // but the builder accepts blocks arriving on an open line: drive the
        // events manually to cover both wrap branches.
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        let request =
            PreviewRequest::new(&path, Path::new("doc.md")).with_limits(MAX_BYTES, MAX_LINES);
        let mut builder = LineBuilder::new(&request);
        builder.handle(Event::Start(Tag::DefinitionList));
        builder.handle(Event::Start(Tag::DefinitionListTitle));
        builder.handle(Event::Text("term".into()));
        // A second title joins while the line is open and wraps it.
        builder.handle(Event::Start(Tag::DefinitionListTitle));
        builder.handle(Event::Text("two".into()));
        // The definition starts while the line is still open.
        builder.handle(Event::Start(Tag::DefinitionListDefinition));
        builder.handle(Event::Text("body".into()));
        builder.handle(Event::End(TagEnd::DefinitionListDefinition));
        let c = builder.finish();
        assert_eq!(
            c.lines,
            vec!["term".to_string(), "two".to_string(), ": body".to_string()]
        );
    }

    #[test]
    fn table_row_and_cell_outside_row_are_defensive() {
        // Synthesize event shapes pulldown does not normally emit: a row
        // starting on an open line, and a cell whose nearest enclosing block
        // is the table itself rather than a row.
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        let request =
            PreviewRequest::new(&path, Path::new("doc.md")).with_limits(MAX_BYTES, MAX_LINES);

        let mut builder = LineBuilder::new(&request);
        builder.blocks.push(Block::Table { header: false });
        builder.handle(Event::Text("lead".into()));
        builder.handle(Event::Start(Tag::TableRow));
        builder.handle(Event::Start(Tag::TableCell));
        builder.handle(Event::Text("x".into()));
        let c = builder.finish();
        assert_eq!(c.lines, vec!["lead".to_string(), "x".to_string()]);
        assert_eq!(c.lines.len(), c.highlights.len());

        let mut builder = LineBuilder::new(&request);
        builder.blocks.push(Block::Table { header: false });
        // No TableRow on the stack: the cell counter lookup declines and no
        // separator is emitted.
        builder.handle(Event::Start(Tag::TableCell));
        builder.handle(Event::Text("y".into()));
        let c = builder.finish();
        assert_eq!(c.lines, vec!["y".to_string()]);
        assert_eq!(c.lines.len(), c.highlights.len());
    }

    #[test]
    fn paragraph_starting_inside_table_cell_stays_inline() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        let request =
            PreviewRequest::new(&path, Path::new("doc.md")).with_limits(MAX_BYTES, MAX_LINES);
        let mut builder = LineBuilder::new(&request);
        builder.blocks.push(Block::TableCell);
        builder.handle(Event::Start(Tag::Paragraph));
        builder.handle(Event::Text("cell".into()));
        builder.handle(Event::End(TagEnd::Paragraph));
        let c = builder.finish();
        assert_eq!(c.lines, vec!["cell".to_string()]);
    }

    #[test]
    fn newline_after_byte_seal_is_ignored_without_extra_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        let request = PreviewRequest::new(&path, Path::new("doc.md")).with_limits(1, MAX_LINES);
        let mut builder = LineBuilder::new(&request);
        // The first piece fills the budget and seals; the split's newline must
        // not add an empty line.
        builder.handle(Event::Text("ab\nc".into()));
        let c = builder.finish();
        assert!(c.truncated);
        assert_eq!(c.lines, vec!["a".to_string()]);
        assert_eq!(c.lines.len(), c.highlights.len());
    }

    #[test]
    fn item_marker_fallback_and_block_on_open_item_line() {
        // Defensive branches pulldown's own event stream does not reach: an
        // item with no enclosing list, and a top-level block starting while an
        // item's marker line is still open.
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc.md");
        let request =
            PreviewRequest::new(&path, Path::new("doc.md")).with_limits(MAX_BYTES, MAX_LINES);

        let mut builder = LineBuilder::new(&request);
        builder.begin_item();
        builder.handle(Event::Text("lone".into()));
        let c = builder.finish();
        assert_eq!(c.lines, vec!["• lone".to_string()]);

        let mut builder = LineBuilder::new(&request);
        builder.blocks.push(Block::Item {
            depth: 0,
            marker_emitted: true,
            first_paragraph: false,
        });
        builder.handle(Event::Text("open".into()));
        builder.handle(Event::Start(Tag::CodeBlock(
            pulldown_cmark::CodeBlockKind::Fenced("".into()),
        )));
        builder.handle(Event::Text("snippet".into()));
        builder.handle(Event::End(TagEnd::CodeBlock));
        let c = builder.finish();
        assert_eq!(c.lines[0], "  open");
        assert!(c.lines.contains(&"snippet".to_string()));
    }
}
