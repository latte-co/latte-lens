use std::{ops::Range, path::Path};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{List, ListItem, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{
        App, ContentMode, ContentVisualRow, FocusPane, GitRowKind, GitTreeRow, TreeScope, UiRegions,
    },
    git::FileStatus,
    preview::{HighlightKind, HighlightSpan},
    tree::FileEntry,
};

const MUTED: Color = Color::Rgb(168, 162, 158);
const SUBTLE: Color = Color::Rgb(104, 100, 98);
const LAVENDER: Color = Color::Rgb(200, 184, 224);
const MINT: Color = Color::Rgb(167, 229, 211);
const PEACH: Color = Color::Rgb(244, 197, 168);
const ROSE: Color = Color::Rgb(232, 184, 196);
const TREE_CHANGE_HINT: Color = Color::Rgb(196, 151, 126);
pub(crate) const MIN_TREE_WIDTH: u16 = 28;
const MIN_CONTENT_WIDTH: u16 = 24;
const DEFAULT_MAX_TREE_WIDTH: u16 = 44;

const ALL_FILES_TAB_LABEL: &str = "  1 Files ";
const GIT_CHANGES_TAB_LABEL: &str = "  2 Git changes ";
const REFRESH_LABEL: &str = " r  Refresh ";

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [left, divider, right] = Layout::horizontal([
        Constraint::Length(app.tree_panel_width(body.width)),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(body);
    let [scope_tabs, tree_header, tree_rows] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
    ])
    .areas(left);
    let [content_header, content_rows] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(right);
    let tree_body = Rect::new(
        left.x,
        tree_header.y,
        left.width,
        tree_header.height.saturating_add(tree_rows.height),
    );
    let content_header = inset_left(content_header, 1);
    let content_rows = inset_left(content_rows, 1);

    app.ui_regions = regions(
        header,
        scope_tabs,
        tree_body,
        tree_rows,
        divider,
        right,
        content_rows,
    );
    draw_header(frame, app, header);
    draw_scope_tabs(frame, app, scope_tabs);
    draw_divider(frame, divider, app.tree_resize_dragging());
    draw_tree(frame, app, tree_header, tree_rows);
    draw_content(frame, app, content_header, content_rows);
    draw_footer(frame, app, footer);
}

fn regions(
    header: Rect,
    scope_tabs: Rect,
    tree_body: Rect,
    tree_rows: Rect,
    divider: Rect,
    content_body: Rect,
    content_rows: Rect,
) -> UiRegions {
    let all_files_width = (ALL_FILES_TAB_LABEL.len() as u16).min(scope_tabs.width);
    let scope_end = scope_tabs.x.saturating_add(scope_tabs.width);
    let git_changes_x = scope_tabs
        .x
        .saturating_add(ALL_FILES_TAB_LABEL.len() as u16)
        .saturating_add(1)
        .min(scope_end);
    let git_changes_width =
        (GIT_CHANGES_TAB_LABEL.len() as u16).min(scope_end.saturating_sub(git_changes_x));
    let refresh_width = (REFRESH_LABEL.len() as u16).min(header.width);
    let refresh_x = header
        .x
        .saturating_add(header.width.saturating_sub(refresh_width));

    UiRegions {
        all_files_tab: Rect::new(
            scope_tabs.x,
            scope_tabs.y,
            all_files_width,
            scope_tabs.height,
        ),
        git_changes_tab: Rect::new(
            git_changes_x,
            scope_tabs.y,
            git_changes_width,
            scope_tabs.height,
        ),
        refresh_button: Rect::new(refresh_x, header.y, refresh_width, header.height.min(1)),
        tree_body,
        tree_inner: tree_rows,
        divider,
        content_body,
        content_inner: content_rows,
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let refresh_width = (REFRESH_LABEL.len() as u16).min(area.width);
    let [header_text, refresh] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(refresh_width)]).areas(area);
    let mut title = vec![Span::styled(
        " LATTE LENS ",
        Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
    )];
    if app.total_repository_count > 0 {
        let change_count = format_change_count(app.changed_count);
        title.push(Span::raw("  "));
        if let Some(branch) = app.branch.as_deref() {
            title.push(Span::styled(branch, Style::default().fg(MINT)));
            title.push(Span::raw("  ·  "));
        }
        title.push(Span::styled(
            format!(
                "{}/{} repos · {change_count}{}{}{}",
                app.dirty_repository_count,
                app.total_repository_count,
                if app.repo.is_none() {
                    " · workspace not repo"
                } else {
                    ""
                },
                if app.repository_graph_truncated {
                    " · PARTIAL"
                } else {
                    ""
                },
                if app.repository_error_count > 0 {
                    " · ERRORS"
                } else {
                    ""
                }
            ),
            Style::default().fg(MUTED),
        ));
    } else {
        title.push(Span::styled("  directory", Style::default().fg(MUTED)));
    }
    let subtitle = Line::from(Span::styled(
        display_path(&app.root),
        Style::default().fg(MUTED),
    ));
    frame.render_widget(
        Paragraph::new(vec![Line::from(title), subtitle]),
        header_text,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if app.is_refreshing() {
                    " ⟳  "
                } else {
                    " r  "
                },
                Style::default().fg(MUTED),
            ),
            Span::styled(
                if app.is_refreshing() {
                    "Working "
                } else {
                    "Refresh "
                },
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
        ])),
        refresh,
    );
}

fn draw_scope_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let labels = [
        (TreeScope::AllFiles, ALL_FILES_TAB_LABEL),
        (TreeScope::GitChanges, GIT_CHANGES_TAB_LABEL),
    ];
    let mut spans = Vec::with_capacity(labels.len() * 2 - 1);
    for (index, (scope, label)) in labels.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let active = app.tree_scope == scope;
        let focused_active = app.focused_pane == FocusPane::ScopeTabs && active;
        let style = if active {
            Style::default()
                .fg(LAVENDER)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(MUTED)
        };
        // Replace one fixed leading space rather than adding a new cell:
        // tab labels and their mouse hit boxes stay the same width.
        let display_label = if focused_active {
            format!("●{}", &label[1..])
        } else {
            label.to_owned()
        };
        spans.push(Span::styled(display_label, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_divider(frame: &mut Frame, area: Rect, resizing: bool) {
    let (glyph, color) = if resizing {
        ("┃", LAVENDER)
    } else {
        ("│", SUBTLE)
    };
    let lines: Vec<Line> = (0..area.height)
        .map(|_| Line::from(Span::styled(glyph, Style::default().fg(color))))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_tree(frame: &mut Frame, app: &mut App, header: Rect, rows: Rect) {
    let selected = app.tree_state.selected();
    let focused = app.focused_pane == FocusPane::Tree;
    let items: Vec<ListItem> = match app.tree_scope {
        TreeScope::AllFiles => app
            .visible_entries()
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                ListItem::new(tree_line(
                    app,
                    entry,
                    selected == Some(index),
                    focused,
                    rows.width,
                ))
            })
            .collect(),
        TreeScope::GitChanges => app
            .visible_git_rows()
            .iter()
            .enumerate()
            .map(|(index, row)| {
                ListItem::new(git_tree_line(
                    app,
                    row,
                    selected == Some(index),
                    focused,
                    rows.width,
                ))
            })
            .collect(),
    };
    let entry_count = app.scope_entry_count();
    let detail = if app.scope_is_truncated() {
        let noun = if app.tree_scope == TreeScope::GitChanges {
            "changes"
        } else {
            "entries"
        };
        let full = format!("{entry_count}+ {noun} · PARTIAL");
        let heading_width = "● Files  ".chars().count() + full.chars().count();
        if heading_width <= usize::from(header.width) {
            full
        } else {
            format!("{entry_count}+ · PARTIAL")
        }
    } else if app.tree_scope == TreeScope::GitChanges {
        let change_count = format_change_count(entry_count);
        format!(
            "{change_count} · {}/{} repos",
            app.dirty_repository_count, app.total_repository_count
        )
    } else {
        format!("{entry_count} entries")
    };
    draw_panel_heading(frame, header, "Files", &detail, focused);
    frame.render_stateful_widget(List::new(items), rows, &mut app.tree_state);
}

fn format_change_count(count: usize) -> String {
    format!("{count} change{}", if count == 1 { "" } else { "s" })
}

fn git_tree_line(
    app: &App,
    row: &GitTreeRow,
    selected: bool,
    focused: bool,
    width: u16,
) -> Line<'static> {
    let repository_has_changes = matches!(
        &row.kind,
        GitRowKind::Repository { change_count, .. } if *change_count > 0
    );
    let indent = "  ".repeat(row.depth);
    let icon = if row.is_container() {
        if app.git_row_is_expanded(row) {
            "▾ "
        } else {
            "▸ "
        }
    } else {
        match row.kind {
            GitRowKind::Pointer(_) => "~ ",
            GitRowKind::Issue(_) => "! ",
            GitRowKind::Change(_) => "- ",
            GitRowKind::Repository { .. } | GitRowKind::Directory => unreachable!(),
        }
    };
    let selection_style = if selected && focused {
        Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default()
            .fg(Color::Reset)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Reset)
    };
    let icon_color = if selected && focused || row.is_container() {
        LAVENDER
    } else if matches!(row.kind, GitRowKind::Issue(_)) {
        ROSE
    } else {
        MUTED
    };
    let spans = vec![
        Span::styled(
            if selected { "▌ " } else { "  " },
            Style::default().fg(if selected && focused {
                LAVENDER
            } else {
                SUBTLE
            }),
        ),
        Span::raw(indent),
        Span::styled(icon, Style::default().fg(icon_color)),
    ];
    let label_style = if !row.exists && !selected {
        Style::default().fg(ROSE)
    } else if repository_has_changes && !selected {
        Style::default().fg(TREE_CHANGE_HINT)
    } else {
        selection_style
    };
    if let Some(status) = row.status {
        tree_row_with_hint(
            spans,
            row.label.clone(),
            label_style,
            compact_tree_status_label(status),
            Style::default().fg(status_color(status)),
            width,
        )
    } else if !row.detail.is_empty() {
        let mut spans = spans;
        spans.push(Span::styled(row.label.clone(), label_style));
        if repository_has_changes {
            spans.push(Span::raw(" "));
            spans.push(Span::styled("•", Style::default().fg(TREE_CHANGE_HINT)));
        }
        spans.push(Span::raw("  "));
        spans.push(Span::styled(row.detail.clone(), Style::default().fg(MUTED)));
        Line::from(spans)
    } else {
        let mut spans = spans;
        spans.push(Span::styled(row.label.clone(), label_style));
        Line::from(spans)
    }
}

fn tree_line(
    app: &App,
    entry: &FileEntry,
    selected: bool,
    focused: bool,
    width: u16,
) -> Line<'static> {
    let indent = "  ".repeat(entry.depth);
    let icon = if entry.is_dir {
        if app.directory_is_expanded(entry) {
            "▾ "
        } else {
            "▸ "
        }
    } else {
        "· "
    };
    let selection_style = if selected && focused {
        Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default()
            .fg(Color::Reset)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Reset)
    };
    let icon_color = if selected && focused || entry.is_dir {
        LAVENDER
    } else {
        MUTED
    };
    let spans = vec![
        Span::styled(
            if selected { "▌ " } else { "  " },
            Style::default().fg(if selected && focused {
                LAVENDER
            } else {
                SUBTLE
            }),
        ),
        Span::raw(indent),
        Span::styled(icon, Style::default().fg(icon_color)),
    ];
    let label = entry.name();
    let label_style = if entry.exists || selected {
        selection_style
    } else {
        Style::default().fg(ROSE)
    };

    if let Some(status) = entry.status {
        tree_row_with_hint(
            spans,
            label,
            label_style,
            compact_tree_status_label(status),
            Style::default().fg(status_color(status)),
            width,
        )
    } else if entry.is_dir && entry.contains_changes {
        tree_row_with_hint(
            spans,
            label,
            label_style,
            "•".to_owned(),
            Style::default().fg(TREE_CHANGE_HINT),
            width,
        )
    } else {
        let mut spans = spans;
        spans.push(Span::styled(label, label_style));
        Line::from(spans)
    }
}

/// Modified state uses a quieter small-cap glyph in both tree scopes while
/// preserving every other porcelain status letter.
fn compact_tree_status_label(status: FileStatus) -> String {
    status.label().replace('M', "ᴍ")
}

/// Keeps Git state in a quiet, fixed column instead of attaching it to names.
/// The trailing cell leaves visual space before the Tree/content divider.
fn tree_row_with_hint(
    mut leading: Vec<Span<'static>>,
    label: String,
    label_style: Style,
    hint: String,
    hint_style: Style,
    width: u16,
) -> Line<'static> {
    let width = usize::from(width);
    let leading_width = spans_width(&leading);
    let hint_width = UnicodeWidthStr::width(hint.as_str());
    let label_width = width.saturating_sub(leading_width + hint_width + 2);
    let label = truncate_to_width(&label, label_width);

    leading.push(Span::styled(label, label_style));
    let used_width = spans_width(&leading);
    let padding = width.saturating_sub(used_width + hint_width + 1).max(1);
    leading.push(Span::raw(" ".repeat(padding)));
    leading.push(Span::styled(hint, hint_style));
    leading.push(Span::raw(" "));
    Line::from(leading)
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn truncate_to_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }

    let mut result = String::new();
    let content_width = max_width.saturating_sub(1);
    let mut used_width = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used_width + character_width > content_width {
            break;
        }
        result.push(character);
        used_width += character_width;
    }
    result.push('…');
    result
}

fn draw_content(frame: &mut Frame, app: &App, header: Rect, rows: Rect) {
    let mut detail = if app.tree_scope == TreeScope::GitChanges {
        app.selected_content_label()
    } else {
        app.selected_entry().map_or_else(String::new, |entry| {
            let provider = app
                .content_provider
                .as_deref()
                .filter(|_| app.content_mode == ContentMode::Preview)
                .map(|provider| format!(" · {provider}"))
                .unwrap_or_default();
            format!("{}{provider}", entry.relative.display())
        })
    };
    if app.is_content_loading() {
        detail.push_str(" · LOADING");
    }
    draw_panel_heading(
        frame,
        header,
        if app.tree_scope == TreeScope::GitChanges {
            app.selected_content_title()
        } else if app.selected_entry().is_some_and(|entry| entry.is_dir) {
            "Directory"
        } else {
            content_mode_title(app.content_mode)
        },
        &detail,
        app.focused_pane == FocusPane::Content,
    );
    let line_number_width = app.content_lines.len().max(1).to_string().len();
    let visual_rows = app.content_visual_rows(rows.width);
    let lines: Vec<Line> = visual_rows
        .iter()
        .filter_map(|visual_row| {
            let line = app.content_lines.get(visual_row.line_index)?;
            let segment = line.get(visual_row.byte_range.clone())?;
            let highlights = app
                .content_highlights
                .get(visual_row.line_index)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let selection = visual_row_selection(app, visual_row);
            Some(match app.content_mode {
                ContentMode::Diff => diff_line(segment, selection),
                ContentMode::Preview if app.content_show_line_numbers => preview_line(
                    (!visual_row.continuation).then_some(visual_row.line_index + 1),
                    line_number_width,
                    segment,
                    line.starts_with("… preview truncated"),
                    visual_row.byte_range.start,
                    highlights,
                    selection,
                ),
                ContentMode::Preview => {
                    preview_text_line(segment, visual_row.byte_range.start, highlights, selection)
                }
                ContentMode::Info => Line::from(content_selection_spans(
                    segment,
                    selection,
                    Style::default().fg(MUTED),
                )),
            })
        })
        .collect();
    let paragraph = Paragraph::new(Text::from(lines)).scroll((
        app.content_scroll
            .min(visual_rows.len().saturating_sub(1))
            .min(u16::MAX as usize) as u16,
        if app.content_mode == ContentMode::Preview {
            0
        } else {
            app.content_horizontal_scroll.min(u16::MAX as usize) as u16
        },
    ));
    frame.render_widget(
        paragraph,
        if app.content_mode == ContentMode::Info {
            inset_top(rows, 1)
        } else {
            rows
        },
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let focus = match app.focused_pane {
        FocusPane::ScopeTabs => "Tabs",
        FocusPane::Tree => "Tree",
        FocusPane::Content => "Content",
    };
    let help = if area.width < 96 {
        "  ↑↓ move  ←→ focus  drag copies  ^C recopy  1/2 scope  r refresh  q quit"
    } else if app.content_mode == ContentMode::Preview {
        "  ↑↓ scroll  ←→ focus  auto-wrap  drag copies  Ctrl+C/Cmd+C recopy  1/2 scope  p preview  d diff  r refresh  q quit"
    } else {
        "  ↑↓ move  ←→ focus  drag copies  Ctrl+C/Cmd+C recopy  Shift+←→ scroll  1/2 scope  p preview  d diff  r refresh  q quit"
    };
    let content = if let Some(error) = &app.last_error {
        Line::from(vec![
            Span::styled(
                format!(" {focus} "),
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(error.to_owned(), Style::default().fg(ROSE)),
        ])
    } else if app.is_refreshing() || app.is_content_loading() {
        let status = match (app.is_refreshing(), app.is_content_loading()) {
            (true, true) => "Refreshing repository graph · Loading content",
            (true, false) => "Refreshing repository graph",
            (false, true) => "Loading content",
            (false, false) => unreachable!(),
        };
        Line::from(vec![
            Span::styled(
                format!(" {focus} "),
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(status, Style::default().fg(MINT)),
        ])
    } else if let Some(status) = &app.clipboard_status {
        Line::from(vec![
            Span::styled(
                format!(" {focus} "),
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(status.to_owned(), Style::default().fg(MINT)),
        ])
    } else if app.repository_graph_truncated || app.repository_error_count > 0 {
        let status = match (app.repository_graph_truncated, app.repository_error_count) {
            (true, 0) => "Repository discovery is PARTIAL".to_owned(),
            (true, errors) => format!("Repository discovery is PARTIAL · {errors} errors"),
            (false, errors) => format!("{errors} repository errors"),
        };
        Line::from(vec![
            Span::styled(
                format!(" {focus} "),
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(status, Style::default().fg(PEACH)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                format!(" {focus} "),
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(help, Style::default().fg(MUTED)),
        ])
    };
    frame.render_widget(Paragraph::new(content), area);
}

fn draw_panel_heading(frame: &mut Frame, area: Rect, title: &str, detail: &str, focused: bool) {
    let title_style = if focused {
        Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Reset)
            .add_modifier(Modifier::BOLD)
    };
    let mut spans = vec![
        Span::styled(
            if focused { "● " } else { "  " },
            Style::default().fg(if focused { LAVENDER } else { SUBTLE }),
        ),
        Span::styled(title.to_owned(), title_style),
    ];
    if !detail.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(detail.to_owned(), Style::default().fg(MUTED)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

const fn content_mode_title(mode: ContentMode) -> &'static str {
    match mode {
        ContentMode::Info => "Info",
        ContentMode::Diff => "Diff",
        ContentMode::Preview => "Preview",
    }
}

fn status_color(status: FileStatus) -> Color {
    match (status.index, status.worktree) {
        ('?', '?') => MINT,
        ('A', _) | (_, 'A') => MINT,
        ('D', _) | (_, 'D') => ROSE,
        _ => TREE_CHANGE_HINT,
    }
}

fn diff_line(line: &str, selection: Option<Range<usize>>) -> Line<'static> {
    let style = if line.starts_with("+++") || line.starts_with("---") {
        Style::default().fg(MUTED)
    } else if line.starts_with('+') {
        Style::default().fg(MINT)
    } else if line.starts_with('-') {
        Style::default().fg(ROSE)
    } else if line.starts_with("@@") {
        Style::default().fg(LAVENDER)
    } else if line.starts_with("diff ") || line.starts_with("index ") || line.starts_with('─') {
        Style::default().fg(PEACH).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Reset)
    };
    Line::from(content_selection_spans(line, selection, style))
}

fn preview_line(
    number: Option<usize>,
    width: usize,
    line: &str,
    truncated: bool,
    segment_start: usize,
    highlights: &[HighlightSpan],
    selection: Option<Range<usize>>,
) -> Line<'static> {
    let text_style = if truncated {
        Style::default().fg(PEACH)
    } else {
        Style::default().fg(Color::Reset)
    };
    let number = number.map(|number| number.to_string()).unwrap_or_default();
    let mut spans = vec![Span::styled(
        format!("{number:>width$} │ "),
        Style::default().fg(MUTED),
    )];
    spans.extend(preview_content_spans(
        line,
        segment_start,
        if truncated { &[] } else { highlights },
        selection,
        text_style,
    ));
    Line::from(spans)
}

fn visual_row_selection(app: &App, visual_row: &ContentVisualRow) -> Option<Range<usize>> {
    let selection = app.content_selection_range(visual_row.line_index)?;
    let start = selection.start.max(visual_row.byte_range.start);
    let end = selection.end.min(visual_row.byte_range.end);
    (start < end).then_some(
        start.saturating_sub(visual_row.byte_range.start)
            ..end.saturating_sub(visual_row.byte_range.start),
    )
}

fn preview_text_line(
    line: &str,
    segment_start: usize,
    highlights: &[HighlightSpan],
    selection: Option<Range<usize>>,
) -> Line<'static> {
    Line::from(preview_content_spans(
        line,
        segment_start,
        highlights,
        selection,
        Style::default().fg(Color::Reset),
    ))
}

fn preview_content_spans(
    line: &str,
    segment_start: usize,
    highlights: &[HighlightSpan],
    selection: Option<Range<usize>>,
    default_style: Style,
) -> Vec<Span<'static>> {
    let segment_end = segment_start.saturating_add(line.len());
    let mut boundaries = vec![0, line.len()];
    for highlight in highlights {
        let start = highlight.range.start.max(segment_start);
        let end = highlight.range.end.min(segment_end);
        if start < end {
            boundaries.push(start - segment_start);
            boundaries.push(end - segment_start);
        }
    }
    if let Some(selection) = selection
        .as_ref()
        .filter(|selection| selection.start < selection.end)
    {
        boundaries.push(selection.start.min(line.len()));
        boundaries.push(selection.end.min(line.len()));
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut spans = Vec::with_capacity(boundaries.len().saturating_sub(1));
    for range in boundaries.windows(2) {
        let start = range[0];
        let end = range[1];
        if start == end {
            continue;
        }
        let absolute_start = segment_start.saturating_add(start);
        let mut style = highlights
            .iter()
            .find(|highlight| highlight.range.contains(&absolute_start))
            .map_or(default_style, |highlight| highlight_style(highlight.kind));
        if selection
            .as_ref()
            .is_some_and(|selection| selection.contains(&start))
        {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let Some(text) = line.get(start..end) else {
            return vec![Span::styled(line.to_owned(), default_style)];
        };
        spans.push(Span::styled(text.to_owned(), style));
    }
    spans
}

fn highlight_style(kind: HighlightKind) -> Style {
    match kind {
        HighlightKind::Comment => Style::default().fg(SUBTLE).add_modifier(Modifier::ITALIC),
        HighlightKind::String => Style::default().fg(PEACH),
        HighlightKind::Keyword => Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
        HighlightKind::Function => Style::default().fg(MINT),
        HighlightKind::Type => Style::default().fg(ROSE),
        HighlightKind::Number => Style::default().fg(PEACH),
        HighlightKind::Constant => Style::default().fg(MINT),
        HighlightKind::Attribute => Style::default().fg(LAVENDER),
    }
}

fn content_selection_spans(
    line: &str,
    selection: Option<Range<usize>>,
    text_style: Style,
) -> Vec<Span<'static>> {
    let Some(selection) = selection.filter(|selection| selection.start < selection.end) else {
        return vec![Span::styled(line.to_owned(), text_style)];
    };
    let Some(before) = line.get(..selection.start) else {
        return vec![Span::styled(line.to_owned(), text_style)];
    };
    let Some(selected) = line.get(selection.clone()) else {
        return vec![Span::styled(line.to_owned(), text_style)];
    };
    let Some(after) = line.get(selection.end..) else {
        return vec![Span::styled(line.to_owned(), text_style)];
    };
    vec![
        Span::styled(before.to_owned(), text_style),
        Span::styled(
            selected.to_owned(),
            text_style.add_modifier(Modifier::REVERSED),
        ),
        Span::styled(after.to_owned(), text_style),
    ]
}

fn inset_left(rect: Rect, amount: u16) -> Rect {
    let amount = amount.min(rect.width);
    Rect::new(
        rect.x.saturating_add(amount),
        rect.y,
        rect.width.saturating_sub(amount),
        rect.height,
    )
}

fn inset_top(rect: Rect, amount: u16) -> Rect {
    let amount = amount.min(rect.height);
    Rect::new(
        rect.x,
        rect.y.saturating_add(amount),
        rect.width,
        rect.height.saturating_sub(amount),
    )
}

fn default_sidebar_width(total_width: u16) -> u16 {
    let preferred = total_width.saturating_mul(36) / 100;
    preferred.clamp(MIN_TREE_WIDTH, DEFAULT_MAX_TREE_WIDTH)
}

pub(crate) fn tree_panel_width(total_width: u16, requested: Option<u16>) -> u16 {
    if total_width <= 1 {
        return 0;
    }
    let maximum = total_width.saturating_sub(MIN_CONTENT_WIDTH + 1);
    let minimum = MIN_TREE_WIDTH.min(maximum);
    requested
        .unwrap_or_else(|| default_sidebar_width(total_width))
        .clamp(minimum, maximum)
}

fn display_path(path: &Path) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path.display().to_string();
    };
    let Ok(relative) = path.strip_prefix(home) else {
        return path.display().to_string();
    };
    if relative.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~/{}", relative.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_hint_uses_a_quiet_right_aligned_status_column() {
        let leading = vec![
            Span::raw("  "),
            Span::raw("    "),
            Span::styled("· ", Style::default().fg(MUTED)),
        ];
        let directory_line = tree_row_with_hint(
            leading.clone(),
            "a-very-long-directory-name".to_owned(),
            Style::default().fg(Color::Reset),
            "•".to_owned(),
            Style::default().fg(TREE_CHANGE_HINT),
            24,
        );
        let file_line = tree_row_with_hint(
            leading,
            "a-very-long-file-name.rs".to_owned(),
            Style::default().fg(Color::Reset),
            compact_tree_status_label(FileStatus {
                index: ' ',
                worktree: 'M',
            }),
            Style::default().fg(TREE_CHANGE_HINT),
            24,
        );

        for (line, expected_hint) in [(&directory_line, "•"), (&file_line, "ᴍ")] {
            let hint = &line.spans[line.spans.len() - 2];
            assert_eq!(line.width(), 24);
            assert_eq!(hint.content.as_ref(), expected_hint);
            assert_eq!(UnicodeWidthStr::width(hint.content.as_ref()), 1);
            assert_eq!(hint.style.fg, Some(TREE_CHANGE_HINT));
            assert_eq!(hint.style.bg.unwrap_or(Color::Reset), Color::Reset);
            assert_eq!(line.spans.last().unwrap().content.as_ref(), " ");
            assert!(
                line.spans
                    .iter()
                    .any(|span| span.content.as_ref().contains('…'))
            );
        }

        let directory_hint = &directory_line.spans[directory_line.spans.len() - 2];
        let file_hint = &file_line.spans[file_line.spans.len() - 2];
        assert!(!directory_hint.style.add_modifier.contains(Modifier::BOLD));
        assert!(!file_hint.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(directory_hint.style, file_hint.style);

        let directory_hint_x = directory_line.width() - 2;
        let file_hint_x = file_line.width() - 2;
        assert_eq!(directory_hint_x, file_hint_x);
    }

    #[test]
    fn all_files_compacts_only_modified_status_letters() {
        assert_eq!(
            compact_tree_status_label(FileStatus {
                index: 'M',
                worktree: 'D',
            }),
            "ᴍD"
        );
        assert_eq!(
            compact_tree_status_label(FileStatus {
                index: '?',
                worktree: '?',
            }),
            "??"
        );
    }

    #[test]
    fn resizable_tree_width_preserves_both_panel_minimums() {
        assert_eq!(tree_panel_width(80, None), 28);
        assert_eq!(tree_panel_width(200, None), 44);
        assert_eq!(tree_panel_width(100, Some(50)), 50);
        assert_eq!(tree_panel_width(100, Some(0)), 28);
        assert_eq!(tree_panel_width(100, Some(99)), 75);
        assert_eq!(tree_panel_width(32, Some(28)), 7);
    }

    #[test]
    fn truncation_respects_double_width_names() {
        let truncated = truncate_to_width("目录文件.rs", 7);

        assert_eq!(UnicodeWidthStr::width(truncated.as_str()), 7);
        assert_eq!(truncated, "目录文…");
    }

    #[test]
    fn wrapped_preview_highlights_and_selection_share_byte_ranges() {
        let spans = preview_content_spans(
            "n mai",
            1,
            &[
                HighlightSpan {
                    range: 0..2,
                    kind: HighlightKind::Keyword,
                },
                HighlightSpan {
                    range: 3..7,
                    kind: HighlightKind::Function,
                },
            ],
            Some(2..4),
            Style::default().fg(Color::Reset),
        );

        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "n mai"
        );
        assert_eq!(spans[0].style.fg, Some(LAVENDER));
        assert_eq!(spans[2].style.fg, Some(MINT));
        assert!(spans[2].style.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(spans[3].style.fg, Some(MINT));
        assert!(!spans[3].style.add_modifier.contains(Modifier::REVERSED));
        assert!(
            spans
                .iter()
                .all(|span| span.style.bg.unwrap_or(Color::Reset) == Color::Reset)
        );
    }
}
