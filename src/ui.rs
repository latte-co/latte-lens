use std::{ops::Range, path::Path};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[cfg(feature = "agent-observability")]
use crate::agent::{ActivityState, AgentViewSession, ObservationMode};

use crate::{
    app::{
        App, ContentMode, ContentVisualRow, DiffReviewState, FocusPane, FoldVisualMarker,
        GitRowKind, GitTreeRow, NavigationPickerRow, NavigationPickerState, SearchMode,
        SearchResult, TreeScope, UiRegions, display_workspace_path,
    },
    diff::{DiffLineAnnotation, DiffLineKind},
    git::FileStatus,
    preview::{HighlightKind, HighlightSpan},
    text_layout::expand_tabs,
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
#[cfg(feature = "agent-observability")]
const AGENTS_TAB_LABEL: &str = "  3 Agents ";
const REFRESH_LABEL: &str = " r  Refresh ";
const FILE_SEARCH_LABEL: &str = "/ Open ";
const TEXT_SEARCH_LABEL: &str = " ^T Text ";

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
    let search_status_height = 0;
    let [scope_tabs, tree_header, search_status, tree_rows] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(search_status_height),
        Constraint::Min(2),
    ])
    .areas(left);
    let [content_header, content_rows] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(right);
    let tree_body = Rect::new(
        left.x,
        tree_header.y,
        left.width,
        tree_header
            .height
            .saturating_add(search_status.height)
            .saturating_add(tree_rows.height),
    );
    let content_header = inset_left(content_header, 1);
    let content_rows = inset_left(content_rows, 1);
    app.prepare_content_width(content_rows.width);

    app.ui_regions = regions(DrawAreas {
        header,
        scope_tabs,
        tree_body,
        tree_header,
        tree_rows,
        divider,
        content_body: right,
        content_header,
        content_rows,
    });
    draw_header(frame, app, header);
    draw_scope_tabs(frame, app, scope_tabs);
    draw_divider(frame, divider, app.tree_resize_dragging());
    draw_tree(frame, app, tree_header, tree_rows);
    draw_content(frame, app, content_header, content_rows);
    draw_footer(frame, app, footer);
    if app.navigation_picker.is_some() {
        dim_underlay(frame);
        draw_navigation_picker(frame, app);
    } else if app.search_is_active() {
        dim_underlay(frame);
        draw_search_popup(frame, app);
    }
}

fn dim_underlay(frame: &mut Frame) {
    let buffer = frame.buffer_mut();
    let area = *buffer.area();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buffer[(x, y)].modifier.insert(Modifier::DIM);
        }
    }
}

struct DrawAreas {
    header: Rect,
    scope_tabs: Rect,
    tree_body: Rect,
    tree_header: Rect,
    tree_rows: Rect,
    divider: Rect,
    content_body: Rect,
    content_header: Rect,
    content_rows: Rect,
}

fn regions(areas: DrawAreas) -> UiRegions {
    let DrawAreas {
        header,
        scope_tabs,
        tree_body,
        tree_header,
        tree_rows,
        divider,
        content_body,
        content_header,
        content_rows,
    } = areas;
    let all_files_width = (ALL_FILES_TAB_LABEL.len() as u16).min(scope_tabs.width);
    let scope_end = scope_tabs.x.saturating_add(scope_tabs.width);
    let git_changes_x = scope_tabs
        .x
        .saturating_add(ALL_FILES_TAB_LABEL.len() as u16)
        .saturating_add(1)
        .min(scope_end);
    let git_changes_width =
        (GIT_CHANGES_TAB_LABEL.len() as u16).min(scope_end.saturating_sub(git_changes_x));
    #[cfg(feature = "agent-observability")]
    let agents_x = git_changes_x
        .saturating_add(GIT_CHANGES_TAB_LABEL.len() as u16)
        .saturating_add(1)
        .min(scope_end);
    #[cfg(feature = "agent-observability")]
    let agents_width = (AGENTS_TAB_LABEL.len() as u16).min(scope_end.saturating_sub(agents_x));
    let refresh_width = (REFRESH_LABEL.len() as u16).min(header.width);
    let refresh_x = header
        .x
        .saturating_add(header.width.saturating_sub(refresh_width));
    let (file_search_button, text_search_button) = inactive_search_buttons(tree_header);
    let (
        preview_find_input,
        preview_find_case,
        preview_find_position,
        preview_find_previous,
        preview_find_next,
        preview_find_close,
    ) = preview_find_controls(content_header);

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
        #[cfg(feature = "agent-observability")]
        agents_tab: Rect::new(agents_x, scope_tabs.y, agents_width, scope_tabs.height),
        refresh_button: Rect::new(refresh_x, header.y, refresh_width, header.height.min(1)),
        file_search_button,
        text_search_button,
        search_popup: Rect::default(),
        search_input: Rect::default(),
        search_clear: Rect::default(),
        search_close: Rect::default(),
        search_files_mode: Rect::default(),
        search_text_mode: Rect::default(),
        search_options: [Rect::default(); 4],
        search_results: Rect::default(),
        navigation_popup: Rect::default(),
        navigation_preview: Rect::default(),
        navigation_results: Rect::default(),
        preview_find_input,
        preview_find_case,
        preview_find_position,
        preview_find_previous,
        preview_find_next,
        preview_find_close,
        tree_body,
        tree_inner: tree_rows,
        divider,
        content_body,
        content_inner: content_rows,
    }
}

fn inactive_search_buttons(area: Rect) -> (Rect, Rect) {
    let (file_width, text_width) = if area.width >= 36 { (7, 10) } else { (3, 4) };
    let total = file_width + text_width;
    let start = area.x.saturating_add(area.width.saturating_sub(total));
    let file = Rect::new(start, area.y, file_width.min(area.width), area.height);
    let text = Rect::new(
        start.saturating_add(file.width),
        area.y,
        text_width.min(area.width.saturating_sub(file.width)),
        area.height,
    );
    (file, text)
}

fn preview_find_controls(area: Rect) -> (Rect, Rect, Rect, Rect, Rect, Rect) {
    let mut end = area.x.saturating_add(area.width);
    let mut take = |width: u16| {
        let width = width.min(end.saturating_sub(area.x));
        end = end.saturating_sub(width);
        Rect::new(end, area.y, width, area.height)
    };
    let close = take(4);
    let next = take(3);
    let previous = take(3);
    let position = take(8);
    let case = if area.width >= 36 {
        take(4)
    } else {
        Rect::default()
    };
    let input = Rect::new(area.x, area.y, end.saturating_sub(area.x), area.height);
    (input, case, position, previous, next, close)
}

fn active_search_controls(area: Rect) -> (Rect, Rect, [Rect; 4]) {
    if area.height == 0 {
        return (Rect::default(), Rect::default(), [Rect::default(); 4]);
    }
    let widths = [5, 5, 4, 5, 4, 5];
    let mut x = area.x;
    let end = area.x.saturating_add(area.width);
    let mut next = |width: u16| {
        let width = width.min(end.saturating_sub(x));
        let result = Rect::new(x, area.y, width, area.height);
        x = x.saturating_add(width);
        result
    };
    let files = next(widths[0]);
    let text = next(widths[1]);
    let options = [
        next(widths[2]),
        next(widths[3]),
        next(widths[4]),
        next(widths[5]),
    ];
    (files, text, options)
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let refresh_width = (REFRESH_LABEL.len() as u16).min(area.width);
    let [header_text, refresh] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(refresh_width)]).areas(area);
    let mut title = vec![Span::styled(
        " LATTE LENS ",
        Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
    )];
    if app.is_initial_loading() {
        title.push(Span::styled(
            "  loading workspace",
            Style::default().fg(MUTED),
        ));
    } else if app.total_repository_count > 0 {
        let change_count = format_change_count(app.changed_count);
        let review_progress = app.review_progress();
        let review_summary = if app.tree_scope == TreeScope::GitChanges && review_progress.total > 0
        {
            format!(
                " · {}/{} reviewed{}",
                review_progress.reviewed,
                review_progress.total,
                if review_progress.changed_after_review > 0 {
                    format!(" · {} changed", review_progress.changed_after_review)
                } else {
                    String::new()
                }
            )
        } else {
            String::new()
        };
        title.push(Span::raw("  "));
        if let Some(branch) = app.branch.as_deref() {
            title.push(Span::styled(branch, Style::default().fg(MINT)));
            title.push(Span::raw("  ·  "));
        }
        title.push(Span::styled(
            format!(
                "{}/{} repos · {change_count}{review_summary}{}{}{}",
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
    #[cfg(not(feature = "agent-observability"))]
    let labels = [
        (TreeScope::AllFiles, ALL_FILES_TAB_LABEL),
        (TreeScope::GitChanges, GIT_CHANGES_TAB_LABEL),
    ];
    #[cfg(feature = "agent-observability")]
    let labels = [
        (TreeScope::AllFiles, ALL_FILES_TAB_LABEL),
        (TreeScope::GitChanges, GIT_CHANGES_TAB_LABEL),
        (TreeScope::Agents, AGENTS_TAB_LABEL),
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

fn draw_search_popup(frame: &mut Frame, app: &mut App) {
    let popup = search_popup_area(frame.area());
    frame.render_widget(Clear, popup);
    let (mode, has_query) = app
        .search
        .as_ref()
        .map(|search| (search.mode, !search.query.is_empty()))
        .unwrap_or((SearchMode::Files, false));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(LAVENDER))
        .title(Span::styled(
            format!(" {} ", mode.label()),
            Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
        ));
    let block_inner = block.inner(popup);
    frame.render_widget(block, popup);
    let inner = inset_horizontal(block_inner, 1);
    let [input_row, status, results, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let clear_width = if has_query { 8.min(input_row.width) } else { 0 };
    let [input, clear] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(clear_width)]).areas(input_row);
    let close_width = popup.width.min(8);
    let close = Rect::new(
        popup
            .right()
            .saturating_sub(close_width.saturating_add(1))
            .max(popup.x),
        popup.y,
        close_width,
        u16::from(popup.height > 0),
    );
    let (files, text, options) = active_search_controls(status);

    app.ui_regions.search_popup = popup;
    app.ui_regions.search_input = input;
    app.ui_regions.search_clear = if has_query { clear } else { Rect::default() };
    app.ui_regions.search_close = close;
    app.ui_regions.search_files_mode = files;
    app.ui_regions.search_text_mode = text;
    app.ui_regions.search_options = if mode == SearchMode::Text {
        options
    } else {
        [Rect::default(); 4]
    };
    app.ui_regions.search_results = results;

    let Some(search) = app.search.as_ref() else {
        return;
    };
    let query_width = usize::from(input.width).saturating_sub(3).max(1);
    let (before, after) = search_query_window(&search.query, search.cursor, query_width);
    let mut input_spans = vec![
        Span::styled(
            "> ",
            Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(before, Style::default().fg(Color::Reset)),
    ];
    input_spans.push(Span::styled(
        "│",
        Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
    ));
    input_spans.push(Span::styled(after, Style::default().fg(Color::Reset)));
    frame.render_widget(Paragraph::new(Line::from(input_spans)), input);
    frame.render_widget(
        Paragraph::new(Span::styled(
            " Esc × ",
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        )),
        close,
    );
    if !search.query.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " Clear ×",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            )),
            clear,
        );
    }

    draw_search_toggle(frame, files, "File ", search.mode == SearchMode::Files);
    draw_search_toggle(frame, text, "Text ", search.mode == SearchMode::Text);
    let mut used_end = text.x.saturating_add(text.width);
    if search.mode == SearchMode::Text {
        let option_values = [
            (" Aa ", search.options.case_sensitive),
            ("Word ", search.options.whole_word),
            (" .* ", search.options.regex),
            (" Ign ", search.options.include_ignored),
        ];
        for (region, (label, active)) in options.into_iter().zip(option_values) {
            draw_search_toggle(frame, region, label, active);
            used_end = region.x.saturating_add(region.width);
        }
    }
    let detail_area = Rect::new(
        used_end,
        status.y,
        status
            .x
            .saturating_add(status.width)
            .saturating_sub(used_end),
        status.height,
    );
    let detail = if let Some(error) = &search.error {
        format!("  {error}")
    } else if search.indexing {
        format!("  {} matches · Indexing…", search.results.len())
    } else if search.searching {
        format!("  {} matches · Searching…", search.results.len())
    } else if search.truncated {
        format!("  {}+ matches · PARTIAL", search.results.len())
    } else if search.mode == SearchMode::Text && !search.query.is_empty() {
        format!(
            "  last Refresh · {} matches · {} files",
            search.results.len(),
            search.scanned_files
        )
    } else if search.query.is_empty() {
        if search.mode == SearchMode::Files {
            "  recent files".to_owned()
        } else {
            "  type to search".to_owned()
        }
    } else {
        format!("  {} results", search.results.len())
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            detail,
            Style::default().fg(if search.error.is_some() { ROSE } else { MUTED }),
        )),
        detail_area,
    );

    let selected = app.search_list_state.selected();
    let items: Vec<ListItem> = search
        .results
        .iter()
        .enumerate()
        .map(|(index, result)| search_result_item(result, selected == Some(index), results.width))
        .collect();
    if items.is_empty() {
        let empty = if search.query.is_empty() {
            if search.mode == SearchMode::Files {
                "Type a file name or path"
            } else {
                "Type text to search the workspace"
            }
        } else if search.searching || search.indexing {
            "Searching…"
        } else {
            "No matches"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(empty, Style::default().fg(MUTED))),
            results,
        );
    } else {
        frame.render_stateful_widget(List::new(items), results, &mut app.search_list_state);
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            "↑↓ select · Enter open · Ctrl+U clear · Esc close",
            Style::default().fg(MUTED),
        )),
        help,
    );
}

fn draw_navigation_picker(frame: &mut Frame, app: &mut App) {
    let popup = search_popup_area(frame.area());
    frame.render_widget(Clear, popup);
    let (title, count) = app
        .navigation_picker
        .as_ref()
        .map_or(("Navigation", 0), |picker| {
            (picker.title.as_str(), picker.results.len())
        });
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(LAVENDER))
        .title(Span::styled(
            format!(" {title} ({count}) "),
            Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
        ));
    let inner = inset_horizontal(block.inner(popup), 1);
    frame.render_widget(block, popup);
    let [body, help] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    app.ui_regions.navigation_popup = popup;
    let (preview, results) = if body.width >= 84 {
        let [preview, divider, results] = Layout::horizontal([
            Constraint::Percentage(58),
            Constraint::Length(1),
            Constraint::Percentage(42),
        ])
        .areas(body);
        frame.render_widget(
            Paragraph::new(Text::from(
                std::iter::repeat_n(Line::from("│"), usize::from(divider.height))
                    .collect::<Vec<_>>(),
            ))
            .style(Style::default().fg(SUBTLE)),
            divider,
        );
        (preview, results)
    } else {
        (Rect::default(), body)
    };
    app.ui_regions.navigation_preview = preview;
    app.ui_regions.navigation_results = results;
    let Some(picker) = app.navigation_picker.as_mut() else {
        return;
    };
    if preview.width > 0 {
        draw_navigation_preview(frame, preview, picker);
    }
    let selected = picker.list_state.selected();
    let items: Vec<ListItem> = picker
        .visible_rows
        .iter()
        .enumerate()
        .map(|(index, row)| navigation_picker_item(picker, *row, selected == Some(index)))
        .collect();
    frame.render_stateful_widget(List::new(items), results, &mut picker.list_state);
    frame.render_widget(
        Paragraph::new(Span::styled(
            "↑↓ select · Enter open/collapse · click location open · Esc close",
            Style::default().fg(MUTED),
        )),
        help,
    );
}

fn navigation_picker_item(
    picker: &NavigationPickerState,
    row: NavigationPickerRow,
    selected: bool,
) -> ListItem<'static> {
    let selected_style = Style::default()
        .fg(Color::Reset)
        .bg(Color::Rgb(54, 51, 58))
        .add_modifier(Modifier::BOLD);
    let style = if selected {
        selected_style
    } else {
        Style::default().fg(MUTED)
    };
    match row {
        NavigationPickerRow::Group(group_index) => {
            let Some(group) = picker.groups.get(group_index) else {
                return ListItem::new(Line::from(""));
            };
            let filename = group.path.file_name().map_or_else(
                || group.path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            let parent = group
                .path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(|parent| format!("  {}", display_workspace_path(parent)))
                .unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::styled(if group.expanded { "▾ " } else { "▸ " }, style),
                Span::styled(filename, style),
                Span::styled(
                    parent,
                    if selected {
                        selected_style
                    } else {
                        Style::default().fg(SUBTLE)
                    },
                ),
                Span::styled(
                    format!("  ({})", group.results.len()),
                    if selected {
                        selected_style
                    } else {
                        Style::default().fg(SUBTLE)
                    },
                ),
            ]))
        }
        NavigationPickerRow::Result(result_index) => {
            let Some(item) = picker.results.get(result_index) else {
                return ListItem::new(Line::from(""));
            };
            let path = picker
                .groups
                .iter()
                .find(|group| group.results.contains(&result_index))
                .map(|group| group.path.display().to_string())
                .unwrap_or_default();
            let location = item
                .label
                .find(&path)
                .map(|start| {
                    let suffix = item
                        .label
                        .get(start.saturating_add(path.len()).saturating_add(1)..)
                        .unwrap_or(item.label.as_str());
                    format!("{}{}", &item.label[..start], suffix)
                })
                .unwrap_or_else(|| item.label.clone());
            let mut spans = vec![Span::styled("  · ", style), Span::styled(location, style)];
            if let Some(detail) = item.detail.as_deref() {
                spans.push(Span::styled(
                    format!("  {detail}"),
                    if selected {
                        selected_style
                    } else {
                        Style::default().fg(SUBTLE)
                    },
                ));
            }
            ListItem::new(Line::from(spans))
        }
    }
}

fn draw_navigation_preview(frame: &mut Frame, area: Rect, picker: &NavigationPickerState) {
    let [header, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let header_text = picker.preview.as_ref().map_or_else(
        || "Preview".to_owned(),
        |preview| display_workspace_path(&preview.path),
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            header_text,
            Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
        )),
        header,
    );
    if picker.preview_loading {
        frame.render_widget(
            Paragraph::new(Span::styled("Loading preview…", Style::default().fg(MUTED))),
            body,
        );
        return;
    }
    if let Some(error) = picker.preview_error.as_deref() {
        frame.render_widget(
            Paragraph::new(Span::styled(error.to_owned(), Style::default().fg(ROSE))),
            body,
        );
        return;
    }
    let Some(preview) = picker.preview.as_ref() else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Select a result to preview it.",
                Style::default().fg(MUTED),
            )),
            body,
        );
        return;
    };
    let height = usize::from(body.height);
    let start_line = preview
        .target
        .start
        .line
        .saturating_sub(height.saturating_div(3));
    let number_width = preview.lines.len().max(1).to_string().len();
    let lines = preview
        .lines
        .iter()
        .enumerate()
        .skip(start_line)
        .take(height)
        .map(|(line_index, line)| {
            let mut highlights = preview
                .highlights
                .get(line_index)
                .cloned()
                .unwrap_or_default();
            if preview.target.start.line <= line_index && line_index <= preview.target.end.line {
                let start = if line_index == preview.target.start.line {
                    preview.target.start.byte
                } else {
                    0
                };
                let end = if line_index == preview.target.end.line {
                    preview.target.end.byte
                } else {
                    line.len()
                };
                if start < end && end <= line.len() {
                    highlights.push(HighlightSpan {
                        range: start..end,
                        kind: HighlightKind::NavigationTarget,
                    });
                }
            }
            let mut spans = vec![Span::styled(
                format!("{:>number_width$} │ ", line_index.saturating_add(1)),
                Style::default().fg(MUTED),
            )];
            spans.extend(preview_content_spans(
                line,
                0,
                &highlights,
                None,
                Style::default().fg(Color::Reset),
            ));
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Text::from(lines)), body);
}

fn search_popup_area(area: Rect) -> Rect {
    let available_width = area.width.saturating_sub(2);
    let available_height = area.height.saturating_sub(2);
    let width = if area.width < 72 {
        available_width
    } else {
        area.width
            .saturating_mul(4)
            .saturating_div(5)
            .max(64)
            .min(available_width)
    };
    let height = if area.height < 16 {
        available_height
    } else {
        area.height
            .saturating_mul(3)
            .saturating_div(4)
            .max(12)
            .min(available_height)
    };
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

fn search_query_window(query: &str, cursor: usize, max_width: usize) -> (String, String) {
    let before = query.get(..cursor).unwrap_or(query);
    let after = query.get(cursor..).unwrap_or_default();
    let before_width = UnicodeWidthStr::width(before);
    if before_width < max_width {
        let remaining = max_width.saturating_sub(before_width + 1);
        return (before.to_owned(), truncate_to_width(after, remaining));
    }

    let target_width = max_width.saturating_sub(2);
    let mut kept = Vec::new();
    let mut width = 0;
    for grapheme in before.graphemes(true).rev() {
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if width + grapheme_width > target_width {
            break;
        }
        kept.push(grapheme);
        width += grapheme_width;
    }
    kept.reverse();
    (format!("…{}", kept.concat()), String::new())
}

fn draw_search_toggle(frame: &mut Frame, area: Rect, label: &str, active: bool) {
    frame.render_widget(
        Paragraph::new(Span::styled(
            label.to_owned(),
            if active {
                Style::default()
                    .fg(LAVENDER)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(MUTED)
            },
        )),
        area,
    );
}

fn search_result_item(result: &SearchResult, selected: bool, width: u16) -> ListItem<'static> {
    let selection_style = if selected {
        Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Reset)
    };
    let icon = if result.is_dir { "▸ " } else { "· " };
    let location = result.line_number.map_or_else(
        || display_workspace_path(&result.path),
        |line| format!("{}:{line}", display_workspace_path(&result.path)),
    );
    let mut spans = vec![
        Span::styled(
            if selected { "▌ " } else { "  " },
            Style::default().fg(if selected { LAVENDER } else { SUBTLE }),
        ),
        Span::styled(
            icon,
            Style::default().fg(if result.is_dir { LAVENDER } else { MUTED }),
        ),
    ];
    let leading_width = spans_width(&spans);
    let available = usize::from(width).saturating_sub(leading_width);
    let location = truncate_to_width(&location, available);
    spans.push(Span::styled(location, selection_style));
    let location_line = Line::from(spans);
    if let Some(line) = &result.line {
        let mut snippet = vec![Span::raw("    ")];
        if let Some(range) = result
            .match_range
            .as_ref()
            .filter(|range| range.end <= line.len())
        {
            snippet.push(Span::styled(
                line[..range.start].to_owned(),
                Style::default().fg(MUTED),
            ));
            snippet.push(Span::styled(
                line[range.clone()].to_owned(),
                Style::default()
                    .fg(LAVENDER)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
            snippet.push(Span::styled(
                line[range.end..].to_owned(),
                Style::default().fg(MUTED),
            ));
        } else {
            snippet.push(Span::styled(
                truncate_to_width(line, usize::from(width).saturating_sub(4)),
                Style::default().fg(MUTED),
            ));
        }
        ListItem::new(Text::from(vec![location_line, Line::from(snippet)]))
    } else {
        ListItem::new(location_line)
    }
}

fn draw_tree(frame: &mut Frame, app: &mut App, header: Rect, rows: Rect) {
    let selected = app.tree_state.selected();
    let focused = app.focused_pane == FocusPane::Tree;
    let items: Vec<ListItem> = if app.is_initial_loading() && !is_agents_scope(app) {
        vec![ListItem::new(Line::from(Span::styled(
            "  Scanning files…",
            Style::default().fg(MINT),
        )))]
    } else {
        match app.tree_scope {
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
            #[cfg(feature = "agent-observability")]
            TreeScope::Agents => app
                .agent_view()
                .sessions
                .iter()
                .enumerate()
                .map(|(index, session)| {
                    ListItem::new(agent_session_line(
                        session,
                        selected == Some(index),
                        focused,
                        rows.width,
                    ))
                })
                .collect(),
        }
    };
    let entry_count = app.scope_entry_count();
    let detail = if is_agents_scope(app) {
        #[cfg(feature = "agent-observability")]
        {
            let view = app.agent_view();
            format!(
                "{}/{} live{}",
                view.live_count,
                view.known_count,
                if view.truncated { " · PARTIAL" } else { "" }
            )
        }
        #[cfg(not(feature = "agent-observability"))]
        {
            String::new()
        }
    } else if app.is_initial_loading() {
        "loading…".to_owned()
    } else if app.scope_is_truncated() {
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
    } else if app.scope_has_unloaded_directories() {
        format!("{entry_count} loaded")
    } else if app.tree_scope == TreeScope::GitChanges {
        let change_count = format_change_count(entry_count);
        let progress = app.review_progress();
        if progress.total == 0 {
            format!(
                "{change_count} · {}/{} repos",
                app.dirty_repository_count, app.total_repository_count
            )
        } else if progress.changed_after_review == 0 {
            format!(
                "{change_count} · {}/{} reviewed",
                progress.reviewed, progress.total
            )
        } else {
            format!(
                "{change_count} · {}/{} reviewed · {} changed",
                progress.reviewed, progress.total, progress.changed_after_review
            )
        }
    } else {
        format!("{entry_count} entries")
    };
    let (file_button, text_button) = inactive_search_buttons(header);
    let heading_width = file_button.x.saturating_sub(header.x);
    let heading = if is_agents_scope(app) {
        "Agents"
    } else {
        "Files"
    };
    draw_panel_heading(
        frame,
        Rect::new(header.x, header.y, heading_width, header.height),
        heading,
        &detail,
        focused,
    );
    let full_labels = file_button.width >= 7;
    frame.render_widget(
        Paragraph::new(Span::styled(
            if full_labels {
                FILE_SEARCH_LABEL
            } else {
                " / "
            },
            Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
        )),
        file_button,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            if full_labels {
                TEXT_SEARCH_LABEL
            } else {
                "^⇧F "
            },
            Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
        )),
        text_button,
    );
    frame.render_stateful_widget(List::new(items), rows, &mut app.tree_state);
}

#[cfg(feature = "agent-observability")]
fn is_agents_scope(app: &App) -> bool {
    app.tree_scope == TreeScope::Agents
}

#[cfg(not(feature = "agent-observability"))]
const fn is_agents_scope(_app: &App) -> bool {
    false
}

#[cfg(feature = "agent-observability")]
fn agent_session_line(
    session: &AgentViewSession,
    selected: bool,
    focused: bool,
    width: u16,
) -> Line<'static> {
    let state = match session.activity {
        ActivityState::Working => "Working",
        ActivityState::WaitingPermission => "Permission",
        ActivityState::Idle => "Idle",
        ActivityState::Unknown => "Unknown",
    };
    let mode = match session.mode {
        ObservationMode::MetadataOnly => "metadata",
        ObservationMode::LiveObserved => "live",
    };
    let marker = if session.reconciling {
        "~"
    } else if session.dropped_live_events > 0 {
        "!"
    } else {
        "•"
    };
    let label = format!(
        "{marker} {} {} {mode} {state}",
        session.subject.as_str(),
        session.short_key()
    );
    let label = truncate_to_width(&label, usize::from(width));
    let style = if selected && focused {
        Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default()
            .fg(Color::Reset)
            .add_modifier(Modifier::BOLD)
    } else if session.activity == ActivityState::Working {
        Style::default().fg(MINT)
    } else {
        Style::default().fg(MUTED)
    };
    Line::from(Span::styled(label, style))
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
    let repository_change_count = match &row.kind {
        GitRowKind::Repository { change_count, .. } => Some(*change_count),
        _ => None,
    };
    let repository_has_changes = repository_change_count.is_some_and(|count| count > 0);
    let indent = "  ".repeat(row.depth);
    let review_state = app.git_row_review_state(row);
    let icon = if row.is_container() {
        if app.git_row_is_expanded(row) {
            "▾ "
        } else {
            "▸ "
        }
    } else {
        match (review_state, &row.kind) {
            (Some(DiffReviewState::Unreviewed), _) => "○ ",
            (Some(DiffReviewState::Reviewed), _) => "✓ ",
            (Some(DiffReviewState::ChangedAfterReview), _) => "↻ ",
            (None, GitRowKind::Pointer(_)) => "~ ",
            (None, GitRowKind::Issue(_)) => "! ",
            (None, GitRowKind::Change(_)) => "- ",
            (None, GitRowKind::Repository { .. } | GitRowKind::Directory) => unreachable!(),
        }
    };
    let review_color = match review_state {
        Some(DiffReviewState::Reviewed) => Some(MINT),
        Some(DiffReviewState::ChangedAfterReview) => Some(PEACH),
        Some(DiffReviewState::Unreviewed) | None => None,
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
        review_color.unwrap_or(MUTED)
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
        let mut hint = Vec::new();
        if let Some(stat) = app.git_row_diff_stat(row) {
            if stat.binary {
                hint.push(Span::styled("binary", Style::default().fg(MUTED)));
            } else {
                hint.push(Span::styled(
                    format!("+{}", stat.added),
                    Style::default().fg(MINT),
                ));
                hint.push(Span::raw(" "));
                hint.push(Span::styled(
                    format!("-{}", stat.deleted),
                    Style::default().fg(ROSE),
                ));
            }
            hint.push(Span::raw(" "));
        }
        hint.push(Span::styled(
            compact_tree_status_label(status),
            Style::default().fg(status_color(status)),
        ));
        tree_row_with_styled_spans_hint(
            spans,
            vec![Span::styled(row.label.clone(), label_style)],
            hint,
            width,
        )
    } else if !row.detail.is_empty() {
        let mut content = vec![Span::styled(row.label.clone(), label_style)];
        if repository_has_changes {
            content.push(Span::raw(" "));
            content.push(Span::styled("•", Style::default().fg(TREE_CHANGE_HINT)));
        }
        content.push(Span::raw("  "));
        content.push(Span::styled(row.detail.clone(), Style::default().fg(MUTED)));
        if let Some(change_count) = repository_change_count.filter(|count| *count > 0) {
            tree_row_with_styled_hint(
                spans,
                content,
                change_count.to_string(),
                Style::default().fg(TREE_CHANGE_HINT),
                width,
            )
        } else {
            let mut spans = spans;
            spans.extend(content);
            Line::from(spans)
        }
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
    let disclosure = if entry.is_dir {
        if app.directory_is_loading(entry) {
            "… "
        } else if app.directory_is_expanded(entry) {
            "▾ "
        } else {
            "▸ "
        }
    } else {
        "  "
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
        Span::styled(disclosure, Style::default().fg(icon_color)),
    ];
    let label = entry.name();
    let label_style = if entry.exists || selected {
        selection_style
    } else {
        Style::default().fg(ROSE)
    };

    // A symbolic link shows a muted "⇢ target" suffix after its name so the
    // link's real destination is visible without opening it. The target uses
    // the raw link text (what `ln -s` recorded), truncated with the row width.
    let mut content = vec![Span::styled(label, label_style)];
    if let Some(target) = entry.symlink_target.as_ref() {
        content.push(Span::styled(
            format!("  ⇢ {}", target.display()),
            Style::default().fg(MUTED),
        ));
    }

    if let Some(status) = entry.status {
        tree_row_with_styled_hint(
            spans,
            content,
            compact_tree_status_label(status),
            Style::default().fg(status_color(status)),
            width,
        )
    } else if entry.is_dir && entry.contains_changes {
        tree_row_with_styled_hint(
            spans,
            content,
            "•".to_owned(),
            Style::default().fg(TREE_CHANGE_HINT),
            width,
        )
    } else if entry.symlink_target.is_some() {
        // Reuse the width-aware layout so a long target truncates cleanly even
        // without a trailing status hint.
        tree_row_with_styled_spans_hint(spans, content, Vec::new(), width)
    } else {
        let mut spans = spans;
        spans.extend(content);
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
#[cfg(test)]
fn tree_row_with_hint(
    leading: Vec<Span<'static>>,
    label: String,
    label_style: Style,
    hint: String,
    hint_style: Style,
    width: u16,
) -> Line<'static> {
    tree_row_with_styled_hint(
        leading,
        vec![Span::styled(label, label_style)],
        hint,
        hint_style,
        width,
    )
}

fn tree_row_with_styled_hint(
    leading: Vec<Span<'static>>,
    content: Vec<Span<'static>>,
    hint: String,
    hint_style: Style,
    width: u16,
) -> Line<'static> {
    tree_row_with_styled_spans_hint(
        leading,
        content,
        vec![Span::styled(hint, hint_style)],
        width,
    )
}

fn tree_row_with_styled_spans_hint(
    mut leading: Vec<Span<'static>>,
    content: Vec<Span<'static>>,
    hint: Vec<Span<'static>>,
    width: u16,
) -> Line<'static> {
    let width = usize::from(width);
    let leading_width = spans_width(&leading);
    let hint_width = spans_width(&hint);
    let content_width = width.saturating_sub(leading_width + hint_width + 2);

    leading.extend(truncate_spans_to_width(content, content_width));
    let used_width = spans_width(&leading);
    let padding = width.saturating_sub(used_width + hint_width + 1).max(1);
    leading.push(Span::raw(" ".repeat(padding)));
    leading.extend(hint);
    leading.push(Span::raw(" "));
    Line::from(leading)
}

fn truncate_spans_to_width(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= max_width {
        return spans;
    }

    let mut truncated = Vec::new();
    let mut remaining = max_width;
    for span in spans {
        let span_width = span.width();
        if span_width < remaining {
            remaining -= span_width;
            truncated.push(span);
            continue;
        }
        if remaining > 0 {
            truncated.push(Span::styled(
                truncate_to_width_forced(span.content.as_ref(), remaining),
                span.style,
            ));
        }
        break;
    }
    truncated
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn truncate_to_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_owned();
    }
    truncate_to_width_forced(value, max_width)
}

fn truncate_to_width_forced(value: &str, max_width: usize) -> String {
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
    if app.preview_find_is_active() {
        draw_preview_find(frame, app);
    } else {
        let mut detail = app.selected_content_label();
        if app.content_mode == ContentMode::Preview
            && let Some(provider) = app.content_provider.as_deref()
        {
            detail.push_str(&format!(" · {provider}"));
        }
        if app.content_mode == ContentMode::Preview
            && let Some(real_path) = app.selected_symlink_real_path()
        {
            detail.push_str(&format!(" · ↗ {}", display_path(&real_path)));
        }
        if app.is_content_loading() {
            detail.push_str(" · LOADING");
        }
        draw_panel_heading(
            frame,
            header,
            app.selected_content_title(),
            &detail,
            app.content_is_focused(),
        );
    }
    let line_number_width = app.content_line_number_width();
    let visual_rows = app.content_visual_rows(rows.width);
    let render_area = if app.content_mode == ContentMode::Info {
        inset_top(rows, 1)
    } else {
        rows
    };
    let start = app.effective_content_scroll(visual_rows.len());
    let end = start
        .saturating_add(usize::from(render_area.height))
        .min(visual_rows.len());
    let lines: Vec<Line> = visual_rows[start..end]
        .iter()
        .filter_map(|visual_row| {
            let line = app.content_lines.get(visual_row.line_index)?;
            let segment = line.get(visual_row.byte_range.clone())?;
            let mut highlights = if visual_row.synthetic {
                Vec::new()
            } else {
                app.content_highlights
                    .get(visual_row.line_index)
                    .cloned()
                    .unwrap_or_default()
            };
            if !visual_row.synthetic {
                highlights.extend(app.preview_find_highlights(visual_row.line_index));
                highlights.extend(app.navigation_highlights(visual_row.line_index));
            }
            let selection = (!visual_row.synthetic)
                .then(|| visual_row_selection(app, visual_row))
                .flatten();
            Some(match app.content_mode {
                ContentMode::Diff => diff_line(
                    segment,
                    app.content_diff_lines.get(visual_row.line_index).copied(),
                    line_number_width,
                    visual_row.continuation,
                    visual_row.tab_origin,
                    selection,
                ),
                ContentMode::Preview if app.content_show_line_numbers => preview_line(
                    (!visual_row.continuation).then_some(visual_row.line_index + 1),
                    line_number_width,
                    segment,
                    line.starts_with("… preview truncated"),
                    &highlights,
                    selection,
                    visual_row,
                ),
                ContentMode::Preview => {
                    preview_text_line(segment, visual_row.byte_range.start, &highlights, selection)
                }
                ContentMode::Info => Line::from(content_selection_spans(
                    segment,
                    selection,
                    Style::default().fg(MUTED),
                    visual_row.tab_origin,
                )),
            })
        })
        .collect();
    let paragraph = Paragraph::new(Text::from(lines))
        .scroll((0, app.effective_content_horizontal_scroll() as u16));
    frame.render_widget(paragraph, render_area);
}

fn draw_preview_find(frame: &mut Frame, app: &App) {
    let Some(find) = app.preview_find.as_ref() else {
        return;
    };
    let input = app.ui_regions.preview_find_input;
    let query_width = usize::from(input.width).saturating_sub(7).max(1);
    let (before, after) = search_query_window(&find.query, find.cursor, query_width);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " Find ",
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(before, Style::default().fg(Color::Reset)),
            Span::styled(
                "│",
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(after, Style::default().fg(Color::Reset)),
        ])),
        input,
    );
    let case = app.ui_regions.preview_find_case;
    if case.width > 0 {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " Aa ",
                Style::default()
                    .fg(if find.case_sensitive { LAVENDER } else { MUTED })
                    .add_modifier(if find.case_sensitive {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )),
            case,
        );
    }
    let (current, count) = app.preview_find_position().unwrap_or((0, 0));
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {current}/{count} "),
            Style::default().fg(MUTED),
        )),
        app.ui_regions.preview_find_position,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(" ↑ ", Style::default().fg(LAVENDER))),
        app.ui_regions.preview_find_previous,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(" ↓ ", Style::default().fg(LAVENDER))),
        app.ui_regions.preview_find_next,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            " [x]",
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        )),
        app.ui_regions.preview_find_close,
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if app.navigation_picker.is_some() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " Navigate ",
                    Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "  ↑↓ select  Enter open/collapse  click location open  Esc close",
                    Style::default().fg(MUTED),
                ),
            ])),
            area,
        );
        return;
    }
    if app.preview_find_is_active() {
        let help = if area.width < 96 {
            "  type query  Enter/↓ next  Shift+Enter/↑ previous  F2 case  Esc close"
        } else {
            "  type query  Enter/↓ next  Shift+Enter/↑ previous  F2 case  Esc close  Ctrl+T workspace"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " Find ",
                    Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
                ),
                Span::styled(help, Style::default().fg(MUTED)),
            ])),
            area,
        );
        return;
    }
    if let Some(search) = app.search.as_ref() {
        let help = if area.width < 96 {
            "  type query  ↑↓ select  Enter open  Ctrl+U clear  Esc close"
        } else if search.mode == SearchMode::Text {
            "  type query  ↑↓ select  Enter open  Ctrl+P files  F2 case  F3 word  F4 regex  F5 ignored  Ctrl+U clear  Esc close"
        } else {
            "  type path  ↑↓ select  Enter open  Ctrl+T text search  Ctrl+U clear  Esc close"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " Search ",
                    Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
                ),
                Span::styled(help, Style::default().fg(MUTED)),
            ])),
            area,
        );
        return;
    }
    let focus = match app.focused_pane {
        FocusPane::ScopeTabs => "Tabs",
        FocusPane::Tree => "Tree",
        FocusPane::Content => "Content",
    };
    let scope_keys = if cfg!(feature = "agent-observability") {
        "1/2/3 scope"
    } else {
        "1/2 scope"
    };
    let help = if area.width < 96 && app.content_mode == ContentMode::Preview {
        format!(
            "  ↑↓ scroll  Ctrl+C copy/quit  [/] folds  Ctrl+D/R/O nav  Ctrl+S symbols  Ctrl+F find  {scope_keys}  q×2 quit"
        )
    } else if area.width < 96 {
        format!("  ↑↓ move  ←→ focus  drag copies  ^C quit/copy  {scope_keys}  r refresh  q×2 quit")
    } else if app.content_mode == ContentMode::Preview {
        format!(
            "  ↑↓ scroll  Ctrl+C copy/quit  [/] folds  Enter toggle  Ctrl+D/R/O nav  Ctrl+S symbols  Alt+click definition  Alt+←/→ history  Ctrl+F find  {scope_keys}  q×2 quit"
        )
    } else if app.content_mode == ContentMode::Diff {
        format!(
            "  ↑↓ scroll  ←→ focus  Space review  n/N file  Ctrl+F find  {scope_keys}  p preview  d diff  r refresh  q×2 quit"
        )
    } else {
        format!(
            "  ↑↓ move  ←→ focus  drag copies  Ctrl+C quit/copy selection  Shift+←→ scroll  {scope_keys}  p preview  d diff  r refresh  q×2 quit"
        )
    };
    let content = if let Some(message) = app.quit_confirmation_message() {
        Line::from(vec![
            Span::styled(
                format!(" {focus} "),
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(
                message,
                Style::default().fg(PEACH).add_modifier(Modifier::BOLD),
            ),
        ])
    } else if let Some(error) = &app.last_error {
        Line::from(vec![
            Span::styled(
                format!(" {focus} "),
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(error.to_owned(), Style::default().fg(ROSE)),
        ])
    } else if app.is_refreshing() || app.is_directory_loading() || app.is_content_loading() {
        let status = match (
            app.is_initial_loading(),
            app.is_refreshing(),
            app.is_directory_loading(),
            app.is_content_loading(),
        ) {
            (true, _, _, true) => "Scanning initial directories · Loading content",
            (true, _, _, false) => "Scanning initial directories",
            (false, true, true, true) => "Refreshing workspace · Loading directory and content",
            (false, true, true, false) => "Refreshing workspace · Loading directory",
            (false, true, false, true) => "Refreshing workspace · Loading content",
            (false, true, false, false) => "Refreshing workspace",
            (false, false, true, true) => "Loading directory and content",
            (false, false, true, false) => "Loading directory",
            (false, false, false, true) => "Loading content",
            (false, false, false, false) => unreachable!(),
        };
        Line::from(vec![
            Span::styled(
                format!(" {focus} "),
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(status, Style::default().fg(MINT)),
        ])
    } else if let Some(status) = &app.navigation_status {
        Line::from(vec![
            Span::styled(
                format!(" {focus} "),
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(
                status.message.clone(),
                Style::default().fg(match status.level {
                    super::app::NavigationStatusLevel::Info => MINT,
                    super::app::NavigationStatusLevel::Error => ROSE,
                }),
            ),
        ])
    } else if let Some(warning) = &app.navigation_config_warning {
        Line::from(vec![
            Span::styled(
                format!(" {focus} "),
                Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(
                format!("Configuration: {warning}"),
                Style::default().fg(ROSE),
            ),
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

fn status_color(status: FileStatus) -> Color {
    match (status.index, status.worktree) {
        ('?', '?') => MINT,
        ('A', _) | (_, 'A') => MINT,
        ('D', _) | (_, 'D') => ROSE,
        _ => TREE_CHANGE_HINT,
    }
}

fn diff_line(
    line: &str,
    annotation: Option<DiffLineAnnotation>,
    number_width: usize,
    continuation: bool,
    tab_origin: usize,
    selection: Option<Range<usize>>,
) -> Line<'static> {
    let kind = annotation.map_or(DiffLineKind::Metadata, |annotation| annotation.kind);
    let style = match kind {
        DiffLineKind::Addition => Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD),
        DiffLineKind::Deletion => Style::default()
            .fg(Color::LightRed)
            .add_modifier(Modifier::BOLD),
        DiffLineKind::Hunk => Style::default().fg(LAVENDER).add_modifier(Modifier::BOLD),
        DiffLineKind::NoNewline => Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        DiffLineKind::Context => Style::default().fg(Color::Reset),
        DiffLineKind::Metadata if line.starts_with("+++") || line.starts_with("---") => {
            Style::default().fg(MUTED)
        }
        DiffLineKind::Metadata
            if line.starts_with("diff ") || line.starts_with("index ") || line.starts_with('─') =>
        {
            Style::default().fg(PEACH).add_modifier(Modifier::BOLD)
        }
        DiffLineKind::Metadata => Style::default().fg(Color::Reset),
    };
    let annotation = annotation.filter(|_| !continuation);
    let old_line = annotation
        .and_then(|annotation| annotation.old_line)
        .map(|line| line.to_string())
        .unwrap_or_default();
    let new_line = annotation
        .and_then(|annotation| annotation.new_line)
        .map(|line| line.to_string())
        .unwrap_or_default();
    let gutter_style = match kind {
        DiffLineKind::Addition => Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD),
        DiffLineKind::Deletion => Style::default()
            .fg(Color::LightRed)
            .add_modifier(Modifier::BOLD),
        DiffLineKind::Hunk => Style::default().fg(LAVENDER),
        _ => Style::default().fg(MUTED),
    };
    let mut spans = vec![Span::styled(
        format!("{old_line:>number_width$} {new_line:>number_width$} │ "),
        gutter_style,
    )];
    spans.extend(content_selection_spans(line, selection, style, tab_origin));
    Line::from(spans)
}

fn preview_line(
    number: Option<usize>,
    width: usize,
    line: &str,
    truncated: bool,
    highlights: &[HighlightSpan],
    selection: Option<Range<usize>>,
    visual_row: &ContentVisualRow,
) -> Line<'static> {
    let text_style = if truncated {
        Style::default().fg(PEACH)
    } else {
        Style::default().fg(Color::Reset)
    };
    let number = number.map(|number| number.to_string()).unwrap_or_default();
    let marker = match visual_row.fold_marker {
        FoldVisualMarker::None => '│',
        FoldVisualMarker::Expanded => '▾',
        FoldVisualMarker::Collapsed => '▸',
    };
    let mut spans = vec![Span::styled(
        format!("{number:>width$} {marker} "),
        Style::default().fg(MUTED),
    )];
    spans.extend(preview_content_spans(
        line,
        visual_row.byte_range.start,
        if truncated { &[] } else { highlights },
        selection,
        text_style,
    ));
    if let Some(summary) = visual_row.summary.as_deref() {
        spans.push(Span::styled(summary.to_owned(), Style::default().fg(MUTED)));
    }
    Line::from(spans)
}

fn visual_row_selection(app: &App, visual_row: &ContentVisualRow) -> Option<Range<usize>> {
    if visual_row.synthetic {
        return None;
    }
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
    let mut display_column = 0;
    for range in boundaries.windows(2) {
        let start = range[0];
        let end = range[1];
        if start == end {
            continue;
        }
        let absolute_start = segment_start.saturating_add(start);
        let mut style = highlights
            .iter()
            .rev()
            .find(|highlight| {
                highlight.kind != HighlightKind::NavigationHover
                    && highlight.range.contains(&absolute_start)
            })
            .map_or(default_style, |highlight| highlight_style(highlight.kind));
        if highlights.iter().any(|highlight| {
            highlight.kind == HighlightKind::NavigationHover
                && highlight.range.contains(&absolute_start)
        }) {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if selection
            .as_ref()
            .is_some_and(|selection| selection.contains(&start))
        {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let Some(text) = line.get(start..end) else {
            return vec![expanded_span(line, 0, 0, default_style).0];
        };
        let (span, next_column) = expanded_span(text, display_column, 0, style);
        spans.push(span);
        display_column = next_column;
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
        HighlightKind::SearchMatch => Style::default()
            .fg(LAVENDER)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        HighlightKind::Search => Style::default()
            .fg(LAVENDER)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        HighlightKind::NavigationTarget => Style::default()
            .fg(Color::Black)
            .bg(MINT)
            .add_modifier(Modifier::BOLD),
        HighlightKind::NavigationHover => Style::default().add_modifier(Modifier::UNDERLINED),
    }
}

fn content_selection_spans(
    line: &str,
    selection: Option<Range<usize>>,
    text_style: Style,
    tab_origin: usize,
) -> Vec<Span<'static>> {
    let Some(selection) = selection.filter(|selection| selection.start < selection.end) else {
        return vec![expanded_span(line, 0, tab_origin, text_style).0];
    };
    let Some(before) = line.get(..selection.start) else {
        return vec![expanded_span(line, 0, tab_origin, text_style).0];
    };
    let Some(selected) = line.get(selection.clone()) else {
        return vec![expanded_span(line, 0, tab_origin, text_style).0];
    };
    let Some(after) = line.get(selection.end..) else {
        return vec![expanded_span(line, 0, tab_origin, text_style).0];
    };
    let (before, before_end) = expanded_span(before, 0, tab_origin, text_style);
    let (selected, selected_end) = expanded_span(
        selected,
        before_end,
        tab_origin,
        text_style.add_modifier(Modifier::REVERSED),
    );
    let (after, _) = expanded_span(after, selected_end, tab_origin, text_style);
    vec![before, selected, after]
}

fn expanded_span(
    value: &str,
    display_column: usize,
    tab_origin: usize,
    style: Style,
) -> (Span<'static>, usize) {
    let (expanded, next_column) = expand_tabs(value, display_column, tab_origin);
    (Span::styled(expanded, style), next_column)
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

fn inset_horizontal(rect: Rect, amount: u16) -> Rect {
    let amount = amount.min(rect.width / 2);
    Rect::new(
        rect.x.saturating_add(amount),
        rect.y,
        rect.width.saturating_sub(amount.saturating_mul(2)),
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
    fn long_search_queries_keep_the_cursor_end_visible() {
        assert_eq!(
            search_query_window("src/a/very/long/path.rs", 23, 10),
            ("…/path.rs".to_owned(), String::new())
        );
        assert_eq!(
            search_query_window("拿铁.rs", "拿".len(), 8),
            ("拿".to_owned(), "铁.rs".to_owned())
        );
    }

    #[test]
    fn search_popup_stays_inside_small_terminals() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(4, 2, 1, 1),
            Rect::new(3, 5, 20, 8),
            Rect::new(0, 0, 100, 24),
        ] {
            let popup = search_popup_area(area);
            assert!(popup.x >= area.x);
            assert!(popup.y >= area.y);
            assert!(popup.right() <= area.right());
            assert!(popup.bottom() <= area.bottom());
        }
    }

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

    #[test]
    fn navigation_hover_adds_underlining_without_replacing_syntax_color() {
        let spans = preview_content_spans(
            "caller",
            0,
            &[
                HighlightSpan {
                    range: 0..6,
                    kind: HighlightKind::Function,
                },
                HighlightSpan {
                    range: 0..6,
                    kind: HighlightKind::NavigationHover,
                },
            ],
            None,
            Style::default().fg(Color::Reset),
        );

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.fg, Some(MINT));
        assert!(spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn preview_and_diff_expand_tabs_without_losing_span_styles() {
        let preview = preview_content_spans(
            "\tField\tstring",
            0,
            &[HighlightSpan {
                range: 1..6,
                kind: HighlightKind::Type,
            }],
            None,
            Style::default().fg(Color::Reset),
        );
        assert_eq!(
            preview
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "    Field   string"
        );
        assert_eq!(preview[1].style.fg, Some(ROSE));

        let diff = content_selection_spans(
            "+\tfield",
            Some(1..2),
            Style::default().fg(Color::LightGreen),
            1,
        );
        assert_eq!(
            diff.iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "+    field"
        );
        assert_eq!(diff[1].content.as_ref(), "    ");
        assert!(diff[1].style.add_modifier.contains(Modifier::REVERSED));
    }
}
