//! All ratatui rendering.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Gauge, HighlightSpacing, List, ListItem, ListState,
    Paragraph, Row, Table, Wrap,
};
use unicode_width::UnicodeWidthChar;

use crate::app::{App, Mode, SuggestKind};
use crate::{columns, theme};

const VERSION: &str = env!("CARGO_PKG_VERSION");

enum TableCellText<'a> {
    Borrowed(&'a str),
    Owned(String),
}

impl<'a> TableCellText<'a> {
    fn as_str(&self) -> &str {
        match self {
            TableCellText::Borrowed(value) => value,
            TableCellText::Owned(value) => value,
        }
    }

    fn into_cell(self) -> Cell<'a> {
        match self {
            TableCellText::Borrowed(value) => Cell::from(value),
            TableCellText::Owned(value) => Cell::from(value),
        }
    }

    /// Like [`Self::into_cell`], honoring a custom column's alignment.
    fn into_cell_aligned(self, align: Option<Alignment>) -> Cell<'a> {
        let Some(align) = align else {
            return self.into_cell();
        };
        match self {
            TableCellText::Borrowed(value) => Cell::from(Text::from(value).alignment(align)),
            TableCellText::Owned(value) => Cell::from(Text::from(value).alignment(align)),
        }
    }
}

/// Map a view column's configured alignment onto ratatui's.
fn cell_alignment(align: crate::views::Align) -> Alignment {
    match align {
        crate::views::Align::Left => Alignment::Left,
        crate::views::Align::Center => Alignment::Center,
        crate::views::Align::Right => Alignment::Right,
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    // Fill the whole frame with the skin's background first (when enabled), so
    // every view that only sets foreground colors sits on it. Widgets that set
    // their own background (the selection bar, gauges, search highlights) still
    // win where they draw.
    if let Some(bg) = theme::background() {
        let area = frame.area();
        frame.buffer_mut().set_style(area, Style::default().bg(bg));
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // header
            Constraint::Min(3),    // body
            Constraint::Length(1), // prompt
            Constraint::Length(1), // status
        ])
        .split(frame.area());

    draw_header(frame, app, chunks[0]);

    match app.mode {
        Mode::Detail => draw_scrollable(frame, &app.detail, chunks[1], theme::sky()),
        Mode::Diff => draw_diff(frame, &app.detail, chunks[1]),
        Mode::Events => draw_scrollable(frame, &app.detail, chunks[1], theme::peach()),
        Mode::Logs | Mode::LogFilter => draw_logs(frame, app, chunks[1]),
        // The lookback prompt opens from the logs view — keep it underneath.
        Mode::Prompt if app.prompt_over_logs() => draw_logs(frame, app, chunks[1]),
        // While typing a doc search, keep drawing the view it was opened from
        // so the matches narrow live under the prompt.
        Mode::DocFilter => match app.doc_filter_return {
            Mode::Diff => draw_diff(frame, &app.detail, chunks[1]),
            Mode::Events => draw_scrollable(frame, &app.detail, chunks[1], theme::peach()),
            Mode::Help => draw_help(frame, app, chunks[1]),
            _ => draw_scrollable(frame, &app.detail, chunks[1], theme::sky()),
        },
        Mode::Help => draw_help(frame, app, chunks[1]),
        Mode::Pulse => draw_pulse(frame, app, chunks[1]),
        Mode::Xray => draw_xray(frame, app, chunks[1]),
        Mode::Explain => draw_explain(frame, app, chunks[1]),
        Mode::Timeline => draw_timeline(frame, app, chunks[1]),
        Mode::PortForwards => draw_port_forwards(frame, app, chunks[1]),
        _ => draw_table(frame, app, chunks[1]),
    }

    match app.mode {
        Mode::Namespaces => draw_namespaces(frame, app, chunks[1]),
        Mode::Contexts => draw_contexts(frame, app, chunks[1]),
        Mode::Containers => draw_containers(frame, app, chunks[1]),
        Mode::SetImage => draw_set_image(frame, app, chunks[1]),
        Mode::Confirm => draw_confirm(frame, app, chunks[1]),
        Mode::Prompt => draw_prompt_popup(frame, app, chunks[1]),
        Mode::Command => draw_palette(frame, app, chunks[1]),
        Mode::FluxMenu => draw_flux_menu(frame, app, chunks[1]),
        Mode::Skins => draw_skins(frame, app, chunks[1]),
        _ => {}
    }

    draw_prompt(frame, app, chunks[2]);
    draw_status(frame, app, chunks[3]);
}

/// Width reserved for the per-kind key-hint column inside the header box:
/// three 13-wide cells (2-char key + space + 10-char label) with 2-space gaps.
const HEADER_HINTS_WIDTH: u16 = 44;
/// Minimum width the info cluster keeps before the hint column may appear.
const HEADER_INFO_MIN: u16 = 44;

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(30), Constraint::Length(26)])
        .split(area);

    let ns = if app.all_namespaces() {
        "<all>".to_string()
    } else {
        app.namespace.clone()
    };
    let mut kind = app.resource_title();
    if let Some(scope) = &app.scope_label {
        kind = format!("{kind}  ‹ {scope}");
    }

    let field = |label: &str, val: String, color| {
        Line::from(vec![
            Span::styled(format!("{label:<12}"), theme::dim()),
            Span::styled(val, Style::default().fg(color)),
        ])
    };

    let mut context_line = field("Context:", app.cluster.context.clone(), theme::mauve());
    if app.readonly {
        context_line.push_span(Span::styled(
            "  [read-only]",
            Style::default().fg(theme::red()),
        ));
    }
    let info = vec![
        context_line,
        field(
            "Cluster:",
            app.cluster.cluster_url.clone(),
            theme::sapphire(),
        ),
        field("Namespace:", ns, theme::green()),
        field("Resource:", kind, theme::peach()),
        field("Count:", app.store.len().to_string(), theme::text()),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(Span::styled(" sofka ", theme::title()));
    let inner = block.inner(cols[0]);
    frame.render_widget(block, cols[0]);

    // Per-kind key hints share the box with the info cluster (k9s-style);
    // narrow terminals collapse back to info-only and keep the full hint
    // line at the bottom instead.
    let hints = header_hints(app);
    if !hints.is_empty() && header_hints_fit(area.width) {
        let sub = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(HEADER_INFO_MIN),
                Constraint::Length(HEADER_HINTS_WIDTH),
            ])
            .split(inner);
        frame.render_widget(Paragraph::new(info), sub[0]);
        frame.render_widget(Paragraph::new(hints), sub[1]);
    } else {
        frame.render_widget(Paragraph::new(info), inner);
    }

    // Sophie the Russian Blue: tall pointed ears, a narrow watchful stare
    // (not round cutesy eyes), cool grey-blue coat. Lines are equal width so
    // the right-aligned block stays coherent.
    let logo = vec![
        Line::from(Span::styled(
            "  /\\        /\\ ",
            Style::default().fg(theme::overlay1()),
        )),
        Line::from(Span::styled(
            " /  \\______/  \\",
            Style::default().fg(theme::overlay1()),
        )),
        Line::from(Span::styled(
            "( -        -  )",
            Style::default().fg(theme::green()),
        )),
        Line::from(Span::styled(
            " \\     ᴥ      /",
            Style::default().fg(theme::maroon()),
        )),
        Line::from(Span::styled(
            "  \\    \\__/   /",
            Style::default().fg(theme::overlay1()),
        )),
        Line::from(Span::styled(
            "   '--------'  ",
            Style::default().fg(theme::overlay1()),
        )),
        Line::from(Span::styled(format!("   sofka v{VERSION}"), theme::dim())),
    ];
    frame.render_widget(Paragraph::new(logo).alignment(Alignment::Right), cols[1]);
}

/// Whether the frame is wide enough for the header's key-hint column:
/// logo (26) + box borders (2) + info cluster + hints.
fn header_hints_fit(frame_width: u16) -> bool {
    frame_width.saturating_sub(26 + 2) >= HEADER_INFO_MIN + HEADER_HINTS_WIDTH
}

/// One hint row of fixed-width cells (right-aligned key, padded label) so
/// consecutive rows line up into a table. Labels must stay ≤ 10 chars.
fn hint_line(pairs: &[(&str, &str)]) -> Line<'static> {
    let key_style = Style::default()
        .fg(theme::sky())
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::with_capacity(pairs.len() * 3);
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(format!("{key:>2}"), key_style));
        spans.push(Span::styled(format!(" {label:<10}"), theme::dim()));
    }
    Line::from(spans)
}

/// Per-kind action hints for the header (k9s-style): only the verbs that
/// actually do something for the current kind — the full reference stays in
/// `?` help, and mode-specific keys stay on the bottom line. Empty when a
/// full-screen view (logs, detail, help, …) replaces the table.
fn header_hints(app: &App) -> Vec<Line<'static>> {
    if matches!(
        app.mode,
        Mode::Detail
            | Mode::Diff
            | Mode::Events
            | Mode::Logs
            | Mode::LogFilter
            | Mode::DocFilter
            | Mode::Help
            | Mode::Pulse
            | Mode::Xray
            | Mode::Explain
            | Mode::Timeline
            | Mode::PortForwards
    ) {
        return Vec::new();
    }
    let mut lines = match app.kind_plural.as_str() {
        "pods" => vec![
            hint_line(&[("⏎", "containers"), ("l", "logs"), ("p", "prev logs")]),
            hint_line(&[("s", "shell"), ("a", "attach"), ("f", "port-fwd")]),
            hint_line(&[("y", "yaml"), ("d", "describe"), ("E", "events")]),
            hint_line(&[("e", "edit"), ("o", "node"), ("J", "owner")]),
            hint_line(&[("X", "explain"), ("T", "timeline"), ("^d", "delete")]),
        ],
        "deployments" | "statefulsets" => vec![
            hint_line(&[("⏎", "pods"), ("l", "logs"), ("E", "events")]),
            hint_line(&[("s", "scale"), ("r", "restart"), ("i", "image")]),
            hint_line(&[("y", "yaml"), ("d", "describe"), ("e", "edit")]),
            hint_line(&[("X", "explain"), ("T", "timeline"), ("f", "port-fwd")]),
        ],
        "daemonsets" => vec![
            hint_line(&[("⏎", "pods"), ("l", "logs"), ("E", "events")]),
            hint_line(&[("r", "restart"), ("i", "image")]),
            hint_line(&[("y", "yaml"), ("d", "describe"), ("e", "edit")]),
            hint_line(&[("X", "explain"), ("^d", "delete")]),
        ],
        "replicasets" | "jobs" => vec![
            hint_line(&[("⏎", "pods"), ("l", "logs"), ("E", "events")]),
            hint_line(&[("y", "yaml"), ("d", "describe"), ("e", "edit")]),
            hint_line(&[("X", "explain"), ("J", "owner"), ("^d", "delete")]),
        ],
        "services" => vec![
            hint_line(&[("⏎", "pods"), ("f", "port-fwd")]),
            hint_line(&[("y", "yaml"), ("d", "describe"), ("e", "edit")]),
            hint_line(&[("^d", "delete")]),
        ],
        "nodes" => vec![
            hint_line(&[("⏎", "pods"), ("y", "yaml"), ("d", "describe")]),
            hint_line(&[("C", "cordon"), ("U", "uncordon"), ("D", "drain")]),
        ],
        "namespaces" => vec![
            hint_line(&[("⏎", "switch to"), ("y", "yaml"), ("d", "describe")]),
            hint_line(&[("e", "edit"), ("^d", "delete")]),
        ],
        "helm" => vec![
            hint_line(&[("⏎", "history")]),
            hint_line(&[("y", "yaml"), ("d", "describe")]),
            hint_line(&[("^d", "uninstall")]),
        ],
        "helmhistory" => vec![
            hint_line(&[("⏎", "values"), ("r", "rollback")]),
            hint_line(&[("^d", "uninstall")]),
        ],
        "customresourcedefinitions" => vec![
            hint_line(&[("⏎", "resources"), ("y", "yaml"), ("d", "describe")]),
            hint_line(&[("e", "edit"), ("^d", "delete")]),
        ],
        "secrets" => vec![
            hint_line(&[("x", "decode"), ("y", "yaml"), ("d", "describe")]),
            hint_line(&[("e", "edit"), ("E", "events"), ("c", "copy name")]),
            hint_line(&[("^d", "delete")]),
        ],
        _ => vec![
            hint_line(&[("⏎", "yaml"), ("d", "describe"), ("E", "events")]),
            hint_line(&[("e", "edit"), ("c", "copy name")]),
            hint_line(&[("^d", "delete")]),
        ],
    };
    if app.flux_suspendable() {
        lines.push(hint_line(&[("t", "flux menu")]));
    }
    if app.external_secret_kind() {
        lines.push(hint_line(&[("r", "force-sync")]));
    }
    // The header box has 5 inner rows.
    lines.truncate(5);
    lines
}

fn draw_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_ns = app.show_namespace_column();
    let metrics_cols = app.metrics_columns();
    let headers: Vec<String> = app.display_headers();
    let pods_view = app.kind_plural == "pods";
    let sort_col = app.sort_column;
    let sort_arrow = if app.sort_desc { " ↓" } else { " ↑" };
    // Offset from a displayed column index back to the view spec's (the spec
    // doesn't know about the prepended NAMESPACE or appended CPU/MEM).
    let ns_off = usize::from(show_ns);
    // Per-column custom alignment, precomputed so cells don't re-borrow app.
    let aligns: Vec<Option<Alignment>> = (0..headers.len())
        .map(|i| {
            i.checked_sub(ns_off)
                .and_then(|si| app.view_spec().align_at(si))
                .map(cell_alignment)
        })
        .collect();
    let align_of = |i: usize| aligns.get(i).copied().flatten();

    let header_row = Row::new(
        headers
            .iter()
            .enumerate()
            .map(|(i, h)| {
                // Active sort column gets a direction arrow in the sorter color
                // (sky, bold), matching k9s; the label inherits the header color.
                if Some(i) == sort_col {
                    let mut line = Line::from(vec![
                        Span::raw(h.clone()),
                        Span::styled(
                            sort_arrow,
                            Style::default()
                                .fg(theme::sorter())
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]);
                    if let Some(a) = align_of(i) {
                        line = line.alignment(a);
                    }
                    Cell::from(line)
                } else {
                    match align_of(i) {
                        Some(a) => Cell::from(Text::from(h.clone()).alignment(a)),
                        None => Cell::from(h.clone()),
                    }
                }
            })
            .collect::<Vec<_>>(),
    )
    .style(theme::header_row());

    // Column indices (fixed for the whole table) for the columns that get
    // their own visibility treatment below, computed once rather than
    // string-compared per cell.
    let name_col = if show_ns { 1 } else { 0 };
    let age_idx = headers.iter().position(|h| h == "AGE");
    let ready_idx = headers.iter().position(|h| h == "READY");
    let restarts_idx = headers.iter().position(|h| h == "RESTARTS");
    let cpu_idx = headers.iter().position(|h| h == "CPU");
    let mem_idx = headers.iter().position(|h| h == "MEM");

    let count = app.row_count();
    let visible_rows = area.height.saturating_sub(3).max(1) as usize;
    if count == 0 {
        *app.table_state.offset_mut() = 0;
    } else {
        if app.table_state.selected().is_some_and(|i| i >= count) {
            app.table_state.select(Some(count - 1));
        }
        let selected = app.table_state.selected();
        let mut offset = app.table_state.offset().min(count.saturating_sub(1));
        if let Some(sel) = selected {
            if sel < offset {
                offset = sel;
            } else if sel >= offset + visible_rows {
                offset = sel + 1 - visible_rows;
            }
        }
        *app.table_state.offset_mut() = offset;
    }
    let offset = app.table_state.offset();
    let selected = app.table_state.selected();

    let visible_objects: Vec<_> = app
        .rows()
        .into_iter()
        .skip(offset)
        .take(visible_rows)
        .collect();
    app.ensure_table_cell_cache(&visible_objects);
    let cell_cache = app.table_cell_cache();
    let spec = app.view_spec();
    let thresholds = app.resolved_thresholds();

    let rows: Vec<Row> = visible_objects
        .iter()
        .map(|obj| {
            let row_key = crate::store::row_key(obj);
            let marked_row = !app.marked.is_empty() && app.marked.contains(&row_key);
            let (base_cells, status_idx) = cell_cache
                .get(&row_key)
                .expect("visible rows are warmed in the table cell cache");
            let mut style_idx = status_idx;
            let mut cells = Vec::with_capacity(headers.len());
            if show_ns {
                cells.push(TableCellText::Borrowed(
                    obj.metadata.namespace.as_deref().unwrap_or_default(),
                ));
                style_idx = status_idx.map(|i| i + 1);
            }
            for (i, cell) in base_cells.iter().enumerate() {
                if let Some(value) = spec.volatile(obj, &app.kind_plural, i) {
                    cells.push(TableCellText::Owned(value));
                } else {
                    cells.push(TableCellText::Borrowed(cell));
                }
            }
            let mut metrics_raw = None;
            if metrics_cols {
                let name = obj.metadata.name.as_deref().unwrap_or_default();
                let key = if pods_view {
                    format!(
                        "{}/{}",
                        obj.metadata.namespace.as_deref().unwrap_or_default(),
                        name
                    )
                } else {
                    name.to_string()
                };
                let (cpu, mem) = app.metrics.get(&key).copied().unwrap_or((0, 0));
                metrics_raw = Some((cpu, mem));
                cells.push(TableCellText::Owned(columns::fmt_cpu(cpu)));
                cells.push(TableCellText::Owned(columns::fmt_mem(mem)));
            }
            // Combined colorer: the whole row takes a k9s-style status tint
            // (errors red, pending peach, completed/terminating dimmed, healthy
            // blue), but a handful of columns keep their own visibility
            // treatment on top: STATUS gets a semantic badge, RESTARTS/CPU/MEM
            // flag outliers, AGE is dimmed (rarely the interesting signal),
            // and NAME highlights the active fuzzy filter's matched chars.
            let status_val = style_idx
                .and_then(|i| cells.get(i))
                .map(TableCellText::as_str)
                .unwrap_or("");
            // A pod is phase=Running the moment its sandbox starts, long before
            // every container passes its readiness probe — until READY is n/n,
            // paint it as transitional, not healthy.
            let running_not_ready = status_val == "Running"
                && ready_idx
                    .and_then(|i| cells.get(i))
                    .is_some_and(|r| !all_ready(r.as_str()));
            let status_key = if running_not_ready {
                "PodInitializing"
            } else {
                status_val
            };
            let row_color = theme::row_color(status_key);
            let status_badge = theme::status_color(status_key);
            let render_cells: Vec<Cell> = cells
                .into_iter()
                .enumerate()
                .map(|(i, c)| {
                    let align = align_of(i);
                    if marked_row {
                        // Marked rows override everything so a bulk selection
                        // stands out.
                        c.into_cell_aligned(align).style(
                            Style::default()
                                .fg(theme::mark())
                                .add_modifier(Modifier::BOLD),
                        )
                    } else if Some(i) == style_idx {
                        c.into_cell_aligned(align)
                            .style(Style::default().fg(status_badge))
                    } else if i == name_col {
                        render_name_cell(app, c.as_str(), row_color)
                    } else if Some(i) == age_idx {
                        c.into_cell_aligned(align).style(theme::dim())
                    } else if Some(i) == restarts_idx {
                        let n: i64 = c.as_str().trim().parse().unwrap_or(0);
                        let color = thresholds
                            .restarts
                            .severity(n)
                            .map(theme::severity_fg)
                            .unwrap_or(row_color);
                        c.into_cell_aligned(align).style(Style::default().fg(color))
                    } else if Some(i) == cpu_idx {
                        let color = metrics_raw
                            .and_then(|(cpu, _)| thresholds.cpu.severity(cpu))
                            .map(theme::severity_fg)
                            .unwrap_or(row_color);
                        c.into_cell_aligned(align).style(Style::default().fg(color))
                    } else if Some(i) == mem_idx {
                        let color = metrics_raw
                            .and_then(|(_, mem)| thresholds.memory.severity(mem))
                            .map(theme::severity_fg)
                            .unwrap_or(row_color);
                        c.into_cell_aligned(align).style(Style::default().fg(color))
                    } else {
                        c.into_cell_aligned(align)
                            .style(Style::default().fg(row_color))
                    }
                })
                .collect();
            Row::new(render_cells)
        })
        .collect();

    let widths: Vec<Constraint> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            // A custom column's configured width wins over the curated rules.
            if let Some(w) = i
                .checked_sub(ns_off)
                .and_then(|si| app.view_spec().width_at(si))
            {
                return Constraint::Length(w);
            }
            match h.as_str() {
                // NAME is the column you actually read — give it most of the
                // remaining space so long pod/deployment names don't truncate
                // while NODE (a full GKE node name) crowds it out.
                "NAME" => Constraint::Fill(6),
                "NAMESPACE" => Constraint::Fill(2),
                "NODE" | "CLAIM" | "VOLUME" | "HOSTS" => Constraint::Fill(1),
                "AGE" => Constraint::Length(7),
                "CPU" | "MEM" => Constraint::Length(8),
                // Wide enough for the long pod reasons (ContainerCreating,
                // CrashLoopBackOff, ImagePullBackOff…) so status is never clipped.
                "STATUS" => Constraint::Length(19),
                "READY" | "RESTARTS" => Constraint::Length(10),
                // CRD view: group domains run long (e.g.
                // "kustomize.toolkit.fluxcd.io"), so give GROUP/KIND/VERSIONS a
                // fixed floor wide enough that real-world values don't clip —
                // Fill(1) alongside NAME's Fill(6) would crush them.
                "GROUP" => Constraint::Length(30),
                "KIND" => Constraint::Length(20),
                "VERSIONS" => Constraint::Length(20),
                "SCOPE" => Constraint::Length(12),
                // Flux views: the Ready condition message and git/chart revision
                // are the columns you read — split the leftover space with NAME.
                "MESSAGE" => Constraint::Fill(4),
                "REVISION" => Constraint::Fill(2),
                "SUSPENDED" => Constraint::Length(9),
                _ => Constraint::Fill(1),
            }
        })
        .collect();

    let kind_label = app.list_title();
    // k9s title: resource name (teal, bold) then a yellow [count].
    let mut title = vec![
        Span::styled(format!(" {kind_label} "), theme::title()),
        Span::styled(format!("[{count}]"), Style::default().fg(theme::counter())),
    ];
    if !app.marked.is_empty() {
        title.push(Span::styled(
            format!(" ✓{}", app.marked.len()),
            Style::default().fg(theme::mark()),
        ));
    }
    // Keep the active filter visible after leaving the `/` prompt (esc
    // clears it, `/` re-opens it for editing), and say whether the API or
    // this process is doing the filtering. Malformed input turns red.
    if !app.filter.is_empty() {
        let style = if app.filter_error().is_some() {
            Style::default().fg(theme::red())
        } else {
            Style::default().fg(theme::teal())
        };
        title.push(Span::styled(format!(" /{}", app.filter), style));
        title.push(Span::styled(
            if app.filter_server_side() {
                " ·server"
            } else {
                " ·local"
            },
            theme::dim(),
        ));
    }
    title.push(Span::raw(" "));

    let mut render_state = ratatui::widgets::TableState::default();
    let render_selected = if count > 0 {
        selected.map(|i| i.saturating_sub(offset))
    } else {
        None
    };
    render_state.select(render_selected);
    let table = Table::new(rows, widths)
        .header(header_row)
        .row_highlight_style(theme::selected_row())
        .highlight_symbol("▌ ")
        // Always reserve the highlight-symbol column so rows never shift right
        // when a selection appears.
        .highlight_spacing(HighlightSpacing::Always)
        // A little breathing room between columns (default is a single space,
        // easy to lose track of where one column ends and the next starts).
        .column_spacing(2)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::border_focused())
                .title(Line::from(title)),
        );

    frame.render_stateful_widget(table, area, &mut render_state);
}

/// `true` when a `n/m` READY cell has every container ready. Cells that
/// aren't in that shape (statuses without a ready fraction) count as ready so
/// they never trigger the not-ready tint.
fn all_ready(ready: &str) -> bool {
    match ready.split_once('/') {
        Some((r, t)) => r == t,
        None => true,
    }
}

/// Render the NAME cell, highlighting characters that matched the active
/// fuzzy row filter (bold yellow) so a scan across many filtered results is
/// faster — every visible row already matched, this just shows *where*.
/// Falls back to a flat `base`-colored cell when there's no active filter.
fn render_name_cell(app: &App, name: &str, base: Color) -> Cell<'static> {
    let Some(matched) = app.filter_match_indices(name).filter(|idx| !idx.is_empty()) else {
        return Cell::from(name.to_string()).style(Style::default().fg(base));
    };
    let matched: std::collections::HashSet<usize> = matched.into_iter().collect();
    let plain = Style::default().fg(base);
    let hl = Style::default()
        .fg(theme::yellow())
        .add_modifier(Modifier::BOLD);

    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_matched = false;
    for (i, ch) in name.chars().enumerate() {
        let is_match = matched.contains(&i);
        if !run.is_empty() && is_match != run_matched {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                if run_matched { hl } else { plain },
            ));
        }
        run_matched = is_match;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_matched { hl } else { plain }));
    }
    Cell::from(Line::from(spans))
}

fn draw_scrollable(
    frame: &mut Frame,
    view: &crate::app::Scrollable,
    area: Rect,
    accent: ratatui::style::Color,
) {
    let inner_h = area.height.saturating_sub(2) as usize;
    let shown = doc_filtered_lines(view);
    // The scroll offset is clamped against the unfiltered length by the key
    // handlers; re-clamp against the (shorter) filtered list.
    let scroll = view.scroll.min(shown.len().saturating_sub(1));
    let (start, end) = visible_line_window(shown.len(), scroll, inner_h);
    let text: Vec<Line> = shown[start..end]
        .iter()
        .map(|l| highlight_matches(Line::from(highlight_yaml(l)), &view.filter))
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Span::styled(doc_title(view, shown.len()), theme::title()));
    let p = Paragraph::new(text).block(block);
    // Wrap folds long lines; otherwise honor the horizontal offset so content
    // past the right edge can be scrolled into view.
    let p = if view.wrap {
        p.wrap(Wrap { trim: false })
    } else {
        p.scroll((0, view.hscroll.min(u16::MAX as usize) as u16))
    };
    frame.render_widget(p, area);
}

/// Logs view with optional substring filter + match highlighting.
///
/// The layout is computed here, not by ratatui: per-line wrapped heights come
/// from [`wrapped_height`] and the visible rows are cut by [`wrap_line`] —
/// the *same* greedy fill — so the scroll math and the pixels can never
/// disagree (ratatui's `Wrap` word-wraps and counts ANSI escape bytes, which
/// made the follow anchor drift). Only the viewport slice is styled and
/// rendered, so a 100k-line paused buffer costs a row-count walk per frame,
/// not a full restyle; and the display-row offset is a `usize`, immune to the
/// `u16` ceiling of `Paragraph::scroll`.
fn draw_logs(frame: &mut Frame, app: &mut App, area: Rect) {
    let filter = app.logs.filter.to_lowercase();
    let active = !filter.is_empty();
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;

    let shown: Vec<&String> = app
        .logs
        .view
        .lines
        .iter()
        .filter(|l| !active || l.to_lowercase().contains(&filter))
        .collect();

    // Exact display height of every shown line, so follow can anchor the
    // newest line to the *bottom* of the viewport (not the top).
    let heights: Vec<usize> = if app.logs.wrap {
        shown.iter().map(|l| wrapped_height(l, inner_w)).collect()
    } else {
        Vec::new() // 1 row per line; skip the allocation walk
    };
    let total_rows: usize = if app.logs.wrap {
        heights.iter().sum()
    } else {
        shown.len()
    };

    // Record viewport geometry (display rows) so key handlers clamp the scroll
    // in the same units, and the message handler can convert trimmed lines
    // into rows when shifting a paused anchor.
    app.logs.viewport_rows = total_rows;
    app.logs.viewport_h = inner_h;
    app.logs.last_wrap_width = if app.logs.wrap { inner_w } else { 0 };

    // Deepest offset pins the last full page to the viewport bottom; that same
    // value is where `follow` anchors, so pausing freezes exactly in place.
    let max_scroll = total_rows.saturating_sub(inner_h);
    let scroll = if app.logs.follow {
        max_scroll
    } else {
        app.logs.view.scroll.min(max_scroll)
    };
    // While following, remember the bottom-anchored position so that turning
    // autoscroll off freezes exactly here instead of jumping to a stale offset.
    if app.logs.follow {
        app.logs.view.scroll = scroll;
    }

    // Style + wrap only the lines that intersect [scroll, scroll + inner_h).
    let mut rows: Vec<Line> = Vec::with_capacity(inner_h);
    let mut row = 0usize; // display row where the current line starts
    for (i, l) in shown.iter().enumerate() {
        let h = if app.logs.wrap { heights[i] } else { 1 };
        if row + h <= scroll {
            row += h;
            continue;
        }
        if row >= scroll + inner_h {
            break;
        }
        let line = render_log_line(l, &app.logs.filter);
        if app.logs.wrap {
            for (j, sub) in wrap_line(line, inner_w).into_iter().enumerate() {
                let r = row + j;
                if r < scroll {
                    continue;
                }
                if r >= scroll + inner_h {
                    break;
                }
                rows.push(sub);
            }
        } else {
            rows.push(line);
        }
        row += h;
    }

    let flags = format!(
        "{}{}{}",
        if app.logs.stopped {
            " ⏹stopped"
        } else if app.logs.follow {
            " ▶follow"
        } else {
            " ⏸paused"
        },
        if app.logs.wrap { " wrap" } else { "" },
        if app.logs.timestamps { " ts" } else { "" },
    );
    let title = if active {
        format!(
            " {} · /{} [{}]{} ",
            app.logs.view.title,
            app.logs.filter,
            shown.len(),
            flags
        )
    } else {
        format!(" {}{} ", app.logs.view.title, flags)
    };

    // The rows are already the exact viewport slice — no Paragraph scroll or
    // wrap, so ratatui can't re-lay-out (and disagree with) the math above.
    let p = Paragraph::new(rows).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::green()))
            .title(Span::styled(title, theme::title())),
    );
    frame.render_widget(p, area);
}

/// Display rows `raw` occupies when char-wrapped to `width` columns: ANSI
/// escapes are zero-width (they're stripped at render time) and East-Asian
/// wide glyphs take two columns. Must stay the exact greedy fill
/// [`wrap_line`] performs — the scroll math depends on them agreeing.
pub(crate) fn wrapped_height(raw: &str, width: usize) -> usize {
    let width = width.max(1);
    // Fast path: plain ASCII with no escapes wraps at exactly `width` chars.
    if raw.is_ascii() && !raw.as_bytes().contains(&0x1b) {
        return raw.len().div_ceil(width).max(1);
    }
    let mut rows = 1usize;
    let mut col = 0usize;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Mirror ansi_runs: swallow a whole CSI sequence, or a lone ESC.
            if chars.peek() == Some(&'[') {
                chars.next();
                for pc in chars.by_ref() {
                    if !(pc.is_ascii_digit() || pc == ';') {
                        break;
                    }
                }
            }
            continue;
        }
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if col + w > width && col > 0 {
            rows += 1;
            col = 0;
        }
        col += w;
    }
    rows
}

/// Greedily split a styled line into rows of at most `width` display columns,
/// breaking spans mid-way as needed. A wide glyph that doesn't fit in the
/// remaining columns moves whole to the next row. Counterpart of
/// [`wrapped_height`] — keep the fill rules identical.
fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line> = Vec::new();
    let mut cur: Vec<Span> = Vec::new();
    let mut col = 0usize;
    for span in line.spans {
        let style = span.style;
        let mut buf = String::new();
        for c in span.content.chars() {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if col + w > width && col > 0 {
                if !buf.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut buf), style));
                }
                out.push(Line::from(std::mem::take(&mut cur)));
                col = 0;
            }
            buf.push(c);
            col += w;
        }
        if !buf.is_empty() {
            cur.push(Span::styled(buf, style));
        }
    }
    out.push(Line::from(cur)); // final row; an empty line still takes one row
    out
}

/// Render a log line: an optional `[source]` prefix (pod/container/component)
/// in its own stable color, an optional leading RFC3339 timestamp dimmed (k9s
/// style), then the message body in its severity color with search matches
/// highlighted on top.
fn render_log_line(line: &str, needle: &str) -> Line<'static> {
    // Severity is detected on the ANSI-stripped text so a color-wrapped level
    // token (e.g. "\x1b[33mwarn\x1b[0m") is still recognized.
    let base = if line.as_bytes().contains(&0x1b) {
        log_level_color(&strip_ansi(line))
    } else {
        log_level_color(line)
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = line;

    // 1. Source prefix in its per-source color (bold).
    if let Some((end, color)) = source_prefix(rest) {
        let (prefix, r) = rest.split_at(end);
        spans.push(Span::styled(
            prefix.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        rest = r;
    }

    // 2. Leading timestamp (from `--timestamps`) dimmed, like k9s.
    if let Some(len) = leading_timestamp(rest) {
        let (ts, r) = rest.split_at(len);
        spans.push(Span::styled(ts.to_string(), theme::dim()));
        rest = r;
    }

    // 3. Message body: honor embedded ANSI colors (from the source app),
    //    falling back to the severity color, with search matches on top.
    spans.extend(render_body(rest, needle, base));
    Line::from(spans)
}

/// Length of a leading RFC3339 timestamp (`2026-06-30T12:52:20.876Z`,
/// `…+02:00`) **only** when it's terminated by whitespace or end-of-line — so a
/// timestamp glued to the message (`…216Zinfo`) is left alone. Hand-rolled to
/// avoid pulling in a regex dependency.
fn leading_timestamp(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let at = |i: usize, c: u8| b.get(i) == Some(&c);
    // YYYY-MM-DD(T| )HH:MM:SS
    let shape = digit(0)
        && digit(1)
        && digit(2)
        && digit(3)
        && at(4, b'-')
        && digit(5)
        && digit(6)
        && at(7, b'-')
        && digit(8)
        && digit(9)
        && (at(10, b'T') || at(10, b' '))
        && digit(11)
        && digit(12)
        && at(13, b':')
        && digit(14)
        && digit(15)
        && at(16, b':')
        && digit(17)
        && digit(18);
    if !shape {
        return None;
    }
    let mut i = 19;
    if at(i, b'.') {
        i += 1;
        while digit(i) {
            i += 1;
        }
    }
    if at(i, b'Z') || at(i, b'z') {
        i += 1;
    } else if (at(i, b'+') || at(i, b'-'))
        && digit(i + 1)
        && digit(i + 2)
        && at(i + 3, b':')
        && digit(i + 4)
        && digit(i + 5)
    {
        i += 6;
    }
    // Require a whitespace/EOL boundary so glued "…Zinfo" isn't treated as a ts.
    match b.get(i) {
        None => Some(i),
        Some(&c) if c == b' ' || c == b'\t' => Some(i),
        _ => None,
    }
}

/// Detect a leading `[label]` source prefix; returns its byte length (including
/// a trailing space, if any) and a stable color for that label.
fn source_prefix(line: &str) -> Option<(usize, Color)> {
    let rest = line.strip_prefix('[')?;
    let close = rest.find(']')?;
    let label = &rest[..close];
    if label.is_empty() {
        return None;
    }
    // `[` + label + `]` = close + 2 bytes; consume a following space too.
    let mut end = close + 2;
    if line[end..].starts_with(' ') {
        end += 1;
    }
    Some((end, source_color(label)))
}

/// Stable color for a source label (FNV-1a hash into a palette). Excludes the
/// severity colors (red/peach) and the search-highlight yellow so a prefix is
/// never mistaken for a level.
fn source_color(label: &str) -> Color {
    let palette: [Color; 10] = [
        theme::mauve(),
        theme::blue(),
        theme::green(),
        theme::teal(),
        theme::pink(),
        theme::sapphire(),
        theme::lavender(),
        theme::flamingo(),
        theme::sky(),
        theme::rosewater(),
    ];
    let mut h: u32 = 0x811c_9dc5;
    for b in label.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    palette[(h as usize) % palette.len()]
}

/// Render a log-line body: split it into runs by any embedded ANSI SGR codes
/// (escape bytes stripped), style each run by its ANSI color — or `base` when
/// it carries none — and overlay search-match highlights.
fn render_body(body: &str, needle: &str, base: Color) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for run in ansi_runs(body) {
        let mut style = Style::default().fg(run.color.unwrap_or(base));
        if run.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        push_highlighted(&mut spans, &run.text, needle, style);
    }
    spans
}

/// Append `text` to `spans` styled with `base`, highlighting case-insensitive
/// occurrences of `needle` on top.
fn push_highlighted(spans: &mut Vec<Span<'static>>, text: &str, needle: &str, base: Style) {
    if needle.is_empty() {
        if !text.is_empty() {
            spans.push(Span::styled(text.to_string(), base));
        }
        return;
    }
    // Lowercasing is not always length-preserving (e.g. Turkish İ, German ß),
    // so match on the same string we slice to keep byte offsets valid and avoid
    // panicking on a non-char-boundary index for multi-byte log lines.
    let hay = text.to_lowercase();
    let pat = needle.to_lowercase();
    if text.len() != hay.len() {
        // Offsets from `hay` wouldn't be valid in `text`; skip highlighting
        // rather than risk slicing mid-character.
        spans.push(Span::styled(text.to_string(), base));
        return;
    }
    let hl = Style::default()
        .bg(theme::yellow())
        .fg(theme::crust())
        .add_modifier(Modifier::BOLD);
    let mut idx = 0;
    while let Some(pos) = hay[idx..].find(&pat) {
        let start = idx + pos;
        let end = start + pat.len();
        if start > idx {
            spans.push(Span::styled(text[idx..start].to_string(), base));
        }
        spans.push(Span::styled(text[start..end].to_string(), hl));
        idx = end;
    }
    if idx < text.len() {
        spans.push(Span::styled(text[idx..].to_string(), base));
    }
}

/// A run of text sharing one style, extracted from an ANSI-coded string.
struct AnsiRun {
    text: String,
    color: Option<Color>,
    bold: bool,
}

/// Concatenated visible text of `s` with all ANSI escapes removed.
fn strip_ansi(s: &str) -> String {
    ansi_runs(s).into_iter().map(|r| r.text).collect()
}

/// Split a string into styled runs by parsing ANSI SGR (`\x1b[…m`) sequences,
/// dropping the escape bytes. Non-SGR CSI sequences (cursor moves, etc.) are
/// swallowed too. Standard 8/16 foreground colors map onto the active skin so
/// embedded colors stay theme-consistent; 256-color (`38;5;n`) and truecolor
/// (`38;2;r;g;b`) pass through verbatim. A string with no escapes yields a
/// single run.
fn ansi_runs(s: &str) -> Vec<AnsiRun> {
    let mut runs = Vec::new();
    let mut cur = String::new();
    let mut color: Option<Color> = None;
    let mut bold = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            let mut params = String::new();
            let mut final_byte = None;
            for pc in chars.by_ref() {
                if pc.is_ascii_digit() || pc == ';' {
                    params.push(pc);
                } else {
                    final_byte = Some(pc);
                    break;
                }
            }
            if final_byte == Some('m') {
                if !cur.is_empty() {
                    runs.push(AnsiRun {
                        text: std::mem::take(&mut cur),
                        color,
                        bold,
                    });
                }
                apply_sgr(&params, &mut color, &mut bold);
            }
            continue; // non-'m' CSI (or a truncated one) is dropped
        }
        if c == '\x1b' {
            continue; // lone / non-CSI escape — drop the ESC byte
        }
        cur.push(c);
    }
    if !cur.is_empty() || runs.is_empty() {
        runs.push(AnsiRun {
            text: cur,
            color,
            bold,
        });
    }
    runs
}

/// Apply one SGR parameter list (the digits/semicolons between `\x1b[` and `m`)
/// to the running foreground color and bold flag.
fn apply_sgr(params: &str, color: &mut Option<Color>, bold: &mut bool) {
    if params.is_empty() {
        *color = None; // bare `\x1b[m` == reset
        *bold = false;
        return;
    }
    let mut it = params.split(';');
    while let Some(tok) = it.next() {
        match tok {
            "" | "0" => {
                *color = None;
                *bold = false;
            }
            "1" => *bold = true,
            "22" => *bold = false,
            "39" => *color = None,
            "38" => match it.next() {
                Some("5") => {
                    if let Some(n) = it.next().and_then(|v| v.parse::<u8>().ok()) {
                        *color = Some(Color::Indexed(n));
                    }
                }
                Some("2") => {
                    let r = it.next().and_then(|v| v.parse::<u8>().ok());
                    let g = it.next().and_then(|v| v.parse::<u8>().ok());
                    let b = it.next().and_then(|v| v.parse::<u8>().ok());
                    if let (Some(r), Some(g), Some(b)) = (r, g, b) {
                        *color = Some(Color::Rgb(r, g, b));
                    }
                }
                _ => {}
            },
            other => {
                if let Some(c) = other.parse::<u8>().ok().and_then(ansi_16_color) {
                    *color = Some(c);
                }
                // background (40-49, 100-107) and other attrs are ignored
            }
        }
    }
}

/// Map a standard 8/16-color SGR foreground code onto the active skin, so
/// embedded ANSI colors read consistently with the chosen theme.
fn ansi_16_color(code: u8) -> Option<Color> {
    Some(match code {
        30 => theme::overlay0(),
        31 => theme::red(),
        32 => theme::green(),
        33 => theme::yellow(),
        34 => theme::blue(),
        35 => theme::mauve(),
        36 => theme::teal(),
        37 => theme::subtext1(),
        90 => theme::overlay1(),
        91 => theme::maroon(),
        92 => theme::green(),
        93 => theme::peach(),
        94 => theme::sapphire(),
        95 => theme::pink(),
        96 => theme::sky(),
        97 => theme::text(),
        _ => return None,
    })
}

/// Guess a log line's severity color across common formats: structured JSON
/// (`"level":"warn"`), space/tab-delimited (` warn `), glued-after-timestamp
/// (`…Zwarn`), `level=error`, and the klog prefix (`E0627 …`). Errors red,
/// warnings peach, debug/trace dimmed; info and anything unrecognized stay in
/// the default text color so they read calmly and real problems pop.
fn log_level_color(line: &str) -> Color {
    let l = line.to_ascii_lowercase();
    // Structured logs: read the level field directly (authoritative — a later
    // "…error…" in the message can't override it).
    if let Some(level) = json_field(&l, "level").or_else(|| json_field(&l, "severity")) {
        return level_color(level);
    }
    // klog prefixes (`E0627 …`) put the level at the very start.
    if klog_level(&l, 'e') || klog_level(&l, 'f') {
        return theme::red();
    }
    if klog_level(&l, 'w') {
        return theme::peach();
    }
    // Otherwise the leftmost level marker wins, since the level precedes the
    // message — so a later "…the last error:" can't override a `warn` level.
    let first = |needles: &[&str]| needles.iter().filter_map(|n| l.find(n)).min();
    let candidates = [
        (
            first(&[
                " error",
                "\terror",
                "zerror",
                "level=error",
                " fatal",
                "zfatal",
                " panic",
            ]),
            theme::red(),
        ),
        (
            first(&[" warn", "\twarn", "zwarn", "level=warn"]),
            theme::peach(),
        ),
        (
            first(&[
                " debug",
                "\tdebug",
                "zdebug",
                " trace",
                "ztrace",
                "level=debug",
            ]),
            theme::overlay1(),
        ),
    ];
    candidates
        .into_iter()
        .filter_map(|(pos, color)| pos.map(|p| (p, color)))
        .min_by_key(|(p, _)| *p)
        .map(|(_, color)| color)
        .unwrap_or(theme::text())
}

/// Color for a parsed level token (already lowercased).
fn level_color(level: &str) -> Color {
    if level.starts_with("err")
        || level.starts_with("fatal")
        || level.starts_with("crit")
        || level.starts_with("panic")
    {
        theme::red()
    } else if level.starts_with("warn") {
        theme::peach()
    } else if level.starts_with("debug") || level.starts_with("trace") {
        theme::overlay1()
    } else {
        theme::text() // info, notice, unknown — keep readable
    }
}

/// Read a JSON string field's value, e.g. `json_field(r#"…"level":"warn"…"#,
/// "level") == Some("warn")`. Tolerant of whitespace around the colon. Input is
/// expected already lowercased.
fn json_field<'a>(l: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let i = l.find(&pat)?;
    let rest = l[i + pat.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// True if `l` starts with a klog level marker, e.g. `e0627 …` (lowercased).
fn klog_level(l: &str, level: char) -> bool {
    let mut it = l.chars();
    it.next() == Some(level) && it.next().is_some_and(|c| c.is_ascii_digit())
}

/// Unified-diff view with +/- line coloring.
fn draw_diff(frame: &mut Frame, view: &crate::app::Scrollable, area: Rect) {
    let inner_h = area.height.saturating_sub(2) as usize;
    let shown = doc_filtered_lines(view);
    let scroll = view.scroll.min(shown.len().saturating_sub(1));
    let (start, end) = visible_line_window(shown.len(), scroll, inner_h);
    let lines: Vec<Line> = shown[start..end]
        .iter()
        .map(|l| {
            let color = match l.chars().next() {
                Some('+') => theme::green(),
                Some('-') => theme::red(),
                _ => theme::overlay1(),
            };
            let line = Line::from(Span::styled((*l).clone(), Style::default().fg(color)));
            highlight_matches(line, &view.filter)
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::peach()))
        .title(Span::styled(doc_title(view, shown.len()), theme::title()));
    let p = Paragraph::new(lines).block(block);
    let p = if view.wrap {
        p.wrap(Wrap { trim: false })
    } else {
        p.scroll((0, view.hscroll.min(u16::MAX as usize) as u16))
    };
    frame.render_widget(p, area);
}

fn visible_line_window(len: usize, scroll: usize, height: usize) -> (usize, usize) {
    let start = scroll.min(len);
    let end = start.saturating_add(height).min(len);
    (start, end)
}

/// The lines a doc view shows: all of them, or only those matching the active
/// `/` search (case-insensitive substring, like the logs filter).
fn doc_filtered_lines(view: &crate::app::Scrollable) -> Vec<&String> {
    let filter = view.filter.to_lowercase();
    view.lines
        .iter()
        .filter(|l| filter.is_empty() || l.to_lowercase().contains(&filter))
        .collect()
}

/// Doc-view title, extended with the active search query and its match count
/// (` title · /query [n] `), mirroring the logs title.
fn doc_title(view: &crate::app::Scrollable, shown: usize) -> String {
    if view.filter.is_empty() {
        format!(" {} ", view.title)
    } else {
        format!(" {} · /{} [{}] ", view.title, view.filter, shown)
    }
}

/// Overlay search-match highlights on an already-styled line, preserving each
/// span's own style for the unmatched stretches. A needle spanning two spans
/// (e.g. across a YAML key/value boundary) is not highlighted — the line is
/// still *shown* (filtering matches on the raw text), just not marked.
fn highlight_matches(line: Line<'static>, needle: &str) -> Line<'static> {
    if needle.is_empty() {
        return line;
    }
    let mut spans = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        push_highlighted(&mut spans, &span.content, needle, span.style);
    }
    Line::from(spans)
}

/// Concatenated plain text of a styled line, for filtering render-time-built
/// views (help) where no raw string backs the line.
fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// YAML / `kubectl describe` colorization: comments dimmed, section headers in
/// mauve, keys in sky, and values tinted by kind (numbers, booleans, statuses).
fn highlight_yaml(line: &str) -> Vec<Span<'static>> {
    let trimmed = line.trim_start();

    // Comments.
    if trimmed.starts_with('#') {
        return vec![Span::styled(line.to_string(), theme::dim())];
    }

    // `key: value` — color the key, keep alignment, tint the value.
    if let Some(idx) = line.find(": ") {
        let (key, rest) = line.split_at(idx);
        if is_keyish(key) {
            let after = &rest[2..]; // value text after the first ": "
            let ws = after.len() - after.trim_start().len();
            let value = &after[ws..];
            let mut spans = vec![
                Span::styled(key.to_string(), Style::default().fg(theme::sky())),
                Span::styled(": ".to_string(), theme::dim()),
            ];
            if ws > 0 {
                spans.push(Span::raw(after[..ws].to_string())); // alignment padding
            }
            if !value.is_empty() {
                spans.push(Span::styled(value.to_string(), value_style(value)));
            }
            return spans;
        }
    }

    // Section header, e.g. `Containers:` / `Events:` (a bare key + colon).
    if let Some(head) = trimmed.strip_suffix(':')
        && is_keyish(head)
    {
        return vec![Span::styled(
            line.to_string(),
            Style::default()
                .fg(theme::mauve())
                .add_modifier(Modifier::BOLD),
        )];
    }

    vec![Span::styled(
        line.to_string(),
        Style::default().fg(theme::text()),
    )]
}

/// A bare identifier (allowing spaces, as in `Start Time`) — used to tell a
/// real key/header from arbitrary text or URLs.
fn is_keyish(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | ' '))
}

/// Tint a value: numbers peach, booleans/null mauve, status words by their
/// status color, everything else default text.
fn value_style(value: &str) -> Style {
    let t = value.trim_end();
    if matches!(
        t,
        "true" | "false" | "null" | "<none>" | "<unset>" | "<unknown>"
    ) {
        return Style::default().fg(theme::mauve());
    }
    if t.parse::<f64>().is_ok() {
        return Style::default().fg(theme::peach());
    }
    let sc = theme::status_color(t);
    if sc != theme::text() {
        return Style::default().fg(sc);
    }
    Style::default().fg(theme::text())
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let bind = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(format!("  {k:<14}"), Style::default().fg(theme::yellow())),
            Span::styled(d.to_string(), theme::dim()),
        ])
    };
    let mut lines = vec![
        Line::from(Span::styled("  Navigation", theme::title())),
        bind(
            ":<resource>",
            "command palette — fuzzy over kinds + commands (tab/↑↓)",
        ),
        bind(
            ":<res> <ns>",
            "switch kind and namespace at once (all/* = all namespaces)",
        ),
        bind("[ · ]", "view history — back · forward"),
        bind(":ctx · :pulse", "switch context · cluster-health dashboard"),
        bind(
            ":xray · :diff",
            "hierarchical tree · live-vs-last-applied diff",
        ),
        bind(":events · E", "events for the selected object"),
        bind(":pf", "view/stop background port-forwards"),
        bind(":skin", "switch color skin live"),
        bind(
            ":reload · :config",
            "reload config from disk · config sources + warnings",
        ),
        bind(
            "enter",
            "drill down (deploy→pods, pod→containers, ns→re-scope)",
        ),
        bind("shift-j", "jump to owner (controller)"),
        bind("o", "show node hosting the pod"),
        bind("esc", "go back / pop view / clear filter"),
        bind("j/k g/G", "move · top/bottom"),
        bind("S · I", "sort by column (cycle) · invert direction"),
        bind("w", "toggle wide columns (kubectl -o wide)"),
        bind(
            "/",
            "filter: fuzzy · !inverse · -l/-f selectors (server-side on ⏎) · col=val cpu>500m age<2h",
        ),
        bind("n · 0-9", "namespace switcher · 0 = all namespaces"),
        bind("ctrl-r", "refresh watch"),
        Line::from(""),
        Line::from(Span::styled("  Inspect", theme::title())),
        bind("y · d", "view YAML · describe (kubectl)"),
        bind("l · p", "logs (workload = all pods) · previous logs"),
        bind(
            "shift-l · :vlogs",
            "VictoriaLogs history (autodiscovered or [providers.logs]) — pods/workloads/ns",
        ),
        bind("c", "copy resource name · in doc views: copy the document"),
        bind("/", "search within YAML/describe/diff/events/help"),
        bind("x", "secrets: show data base64-decoded"),
        bind(
            "shift-x · :explain",
            "explain why the selection is unhealthy (evidence-backed)",
        ),
        bind(
            "shift-t · :timeline",
            "session-local state-change history for the selection",
        ),
        Line::from(""),
        Line::from(Span::styled("  Act", theme::title())),
        bind("e", "edit in $EDITOR (kubectl edit)"),
        bind("s", "shell into pod / scale workload"),
        bind("a", "attach to pod"),
        bind("i", "set container image"),
        bind(
            "r",
            "rollout restart (deploy/sts/ds) · force-sync (external secrets)",
        ),
        bind(
            "f / shift-f",
            "port-forward (pod/svc) — runs in the background",
        ),
        bind(
            "t",
            "flux: suspend/resume/reconcile menu (ks/hr/repos/buckets…)",
        ),
        bind("C · U · D", "nodes: cordon · uncordon · drain"),
        bind("space", "mark/unmark row for bulk actions (esc clears)"),
        bind(
            "ctrl-d · ctrl-k",
            "delete · force-delete (in confirm: f force, c cascade)",
        ),
        Line::from(""),
        Line::from(Span::styled("  Logs view", theme::title())),
        bind("/ · s · w · t", "search · autoscroll · wrap · timestamps"),
        bind("x · c · ctrl-s", "stop/resume · copy · save to file"),
        bind(
            "shift-t",
            "provider logs: change lookback period (30m, 4h, 2d)",
        ),
        Line::from(""),
        bind(":q / ctrl-c", "quit"),
        bind("?", "toggle help"),
    ];
    // Config-defined plugins, with their (possibly modified) key chords.
    if !app.plugins.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  Plugins", theme::title())));
        for p in &app.plugins {
            let key = crate::keys::KeyChord::parse(&p.key)
                .map(|c| c.label())
                .unwrap_or_else(|_| format!("{}?", p.key));
            let scope = if p.scopes.is_empty() {
                "all resources".to_string()
            } else {
                p.scopes.join(", ")
            };
            lines.push(bind(&key, &format!("{} ({scope})", p.name)));
        }
    }
    // `/` search: keep only matching binding lines (section headers and
    // spacers match like any other text), highlighting the matched runs.
    let needle = app.help_filter.to_lowercase();
    let (lines, title) = if needle.is_empty() {
        (lines, " Help ".to_string())
    } else {
        let shown: Vec<Line> = lines
            .into_iter()
            .filter(|l| line_text(l).to_lowercase().contains(&needle))
            .map(|l| highlight_matches(l, &app.help_filter))
            .collect();
        let title = format!(" Help · /{} [{}] ", app.help_filter, shown.len());
        (shown, title)
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::border_focused())
                .title(Span::styled(title, theme::title())),
        ),
        area,
    );
}

fn draw_namespaces(frame: &mut Frame, app: &mut App, area: Rect) {
    let names = app.filtered_namespaces();
    let items: Vec<ListItem> = names
        .iter()
        .map(|n| {
            let color = if n == "<all>" {
                theme::teal()
            } else {
                theme::text()
            };
            ListItem::new(Span::styled(n.clone(), Style::default().fg(color)))
        })
        .collect();
    // Show the type-to-filter buffer in the title so it reads like an input.
    let title = if app.ns_filter.is_empty() {
        " Namespaces (type to filter · ⏎ switch) ".to_string()
    } else {
        format!(" Namespaces · /{}_ ", app.ns_filter)
    };
    render_popup_list(
        frame,
        area,
        40,
        60,
        items,
        Span::styled(title, theme::title()),
        &mut app.ns_state,
    );
}

fn draw_contexts(frame: &mut Frame, app: &mut App, area: Rect) {
    let current = app.cluster.context.clone();
    let items: Vec<ListItem> = app
        .filtered_contexts()
        .iter()
        .map(|c| {
            let marker = if *c == current { "● " } else { "  " };
            ListItem::new(Span::styled(
                format!("{marker}{c}"),
                Style::default().fg(if *c == current {
                    theme::green()
                } else {
                    theme::text()
                }),
            ))
        })
        .collect();
    // Show the type-to-filter buffer in the title so it reads like an input.
    let title = if app.ctx_filter.is_empty() {
        " Contexts (type to filter · ⏎ switch) ".to_string()
    } else {
        format!(" Contexts · /{}_ ", app.ctx_filter)
    };
    render_popup_list(
        frame,
        area,
        50,
        60,
        items,
        Span::styled(title, theme::title()),
        &mut app.ctx_state,
    );
}

/// Flux suspend/resume action menu (`t`). Deliberately a menu rather than a
/// single-key toggle, so acting on a live resource always takes an explicit,
/// visible choice.
fn draw_flux_menu(frame: &mut Frame, app: &mut App, area: Rect) {
    let count = app.marked.len().max(1);
    let target = if count == 1 {
        "current selection".to_string()
    } else {
        format!("{count} marked {}", app.kind_plural)
    };
    let items: Vec<ListItem> = crate::app::FLUX_MENU_ITEMS
        .iter()
        .map(|label| {
            let color = match *label {
                "Suspend" => theme::peach(),
                "Resume" => theme::green(),
                _ => theme::overlay1(),
            };
            ListItem::new(Span::styled(*label, Style::default().fg(color)))
        })
        .collect();
    render_popup_list(
        frame,
        area,
        36,
        24,
        items,
        Span::styled(format!(" Flux: {target} "), theme::title()),
        &mut app.flux_menu_state,
    );
}

/// Background port-forwards (`:pf`). A full-width view, not a popup — closing
/// it (`esc`) does not stop the forwards; only `x`/`s` on a row does.
fn draw_port_forwards(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .port_forwards
        .iter()
        .map(|pf| {
            ListItem::new(Line::from(vec![
                Span::styled("● ", Style::default().fg(theme::green())),
                Span::styled(pf.label(), Style::default().fg(theme::text())),
            ]))
        })
        .collect();
    let title = format!(
        " Port-forwards [{}]  (x/s stop · esc close — others keep running) ",
        app.port_forwards.len()
    );
    render_framed_list(
        frame,
        area,
        items,
        Span::styled(title, theme::title()),
        &mut app.pf_state,
    );
}

fn draw_skins(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .skin_list
        .iter()
        .map(|name| {
            ListItem::new(Span::styled(
                name.clone(),
                Style::default().fg(theme::text()),
            ))
        })
        .collect();
    render_popup_list(
        frame,
        area,
        42,
        58,
        items,
        Span::styled(" Skins (enter apply · esc close) ", theme::title()),
        &mut app.skin_state,
    );
}

/// Color a resource utilization percentage against the configured band: close
/// to the base (a request or, more importantly, a limit) is dangerous. A
/// missing base dims to a muted tone so it reads as "not set" rather than
/// "healthy"; a present percentage below the warning line reads green.
fn util_color(pct: Option<i64>, band: crate::thresholds::Band) -> Color {
    use crate::thresholds::Severity;
    match pct {
        None => theme::overlay1(),
        Some(p) => match band.severity(p) {
            Some(Severity::Critical) => theme::red(),
            Some(Severity::Warn) => theme::yellow(),
            None => theme::green(),
        },
    }
}

/// Build the `%req/%lim` utilization cell for one resource, plus the color that
/// reflects the worse (limit-first) utilization. `usage` is `None` when Metrics
/// Server data is unavailable, in which case percentages cannot be computed.
fn util_cell(
    usage: Option<i64>,
    request: Option<i64>,
    limit: Option<i64>,
    band: crate::thresholds::Band,
) -> (String, Color) {
    use crate::columns::{fmt_pct, usage_pct};
    let Some(usage) = usage else {
        return ("-/-".into(), theme::overlay1());
    };
    let req_pct = usage_pct(usage, request);
    let lim_pct = usage_pct(usage, limit);
    let text = format!("{}/{}", fmt_pct(req_pct), fmt_pct(lim_pct));
    (text, util_color(lim_pct.or(req_pct), band))
}

// Numeric column widths for the container table, shared by the header and the
// data rows so they line up exactly. `CPU%`/`MEM%` hold a `%req/%lim` pair.
const C_CPU: usize = 7;
const C_CPU_PCT: usize = 9;
const C_MEM: usize = 8;
const C_MEM_PCT: usize = 9;
const C_GAP: usize = 2;

/// Truncate to `max` display columns with a trailing ellipsis (character-based;
/// container names are ASCII in practice).
fn truncate_cols(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    match max {
        0 => String::new(),
        _ => {
            let mut t: String = s.chars().take(max - 1).collect();
            t.push('…');
            t
        }
    }
}

fn draw_containers(frame: &mut Frame, app: &mut App, area: Rect) {
    let gap = " ".repeat(C_GAP);
    // Keep the name column readable but bounded so long names can't push the
    // numeric columns off the right edge; anything longer is ellipsized.
    let name_cap = (area.width as usize).saturating_sub(40).max(8);
    let util_band = app.resolved_thresholds().utilization;
    let name_width = app
        .container_list
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, name_cap);

    let header = Line::from(format!(
        "{name:<name_width$}{gap}{cpu:>C_CPU$}{gap}{cpu_pct:>C_CPU_PCT$}{gap}{mem:>C_MEM$}{gap}{mem_pct:>C_MEM_PCT$}",
        name = "NAME",
        cpu = "CPU",
        cpu_pct = "%R/L",
        mem = "MEM",
        mem_pct = "%R/L",
    ))
    .style(theme::dim());

    let items: Vec<ListItem> = app
        .container_list
        .iter()
        .map(|container| {
            let usage = app.selected_pod_container_metrics(container);
            let (cpu, memory) = usage
                .map(|(cpu, memory)| {
                    (
                        crate::columns::fmt_cpu(cpu),
                        crate::columns::fmt_mem(memory),
                    )
                })
                .unwrap_or_else(|| ("-".into(), "-".into()));
            let res = app
                .container_resources
                .get(container)
                .cloned()
                .unwrap_or_default();
            let (cpu_pct, cpu_pct_color) = util_cell(
                usage.map(|(c, _)| c),
                res.cpu_request,
                res.cpu_limit,
                util_band,
            );
            let (mem_pct, mem_pct_color) = util_cell(
                usage.map(|(_, m)| m),
                res.mem_request,
                res.mem_limit,
                util_band,
            );
            let name = truncate_cols(container, name_width);
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{name:<name_width$}"),
                    Style::default().fg(theme::text()),
                ),
                Span::styled(
                    format!("{gap}{cpu:>C_CPU$}"),
                    Style::default().fg(theme::yellow()),
                ),
                Span::styled(
                    format!("{gap}{cpu_pct:>C_CPU_PCT$}"),
                    Style::default().fg(cpu_pct_color),
                ),
                Span::styled(
                    format!("{gap}{memory:>C_MEM$}"),
                    Style::default().fg(theme::teal()),
                ),
                Span::styled(
                    format!("{gap}{mem_pct:>C_MEM_PCT$}"),
                    Style::default().fg(mem_pct_color),
                ),
            ]))
        })
        .collect();

    let qos = if app.container_qos.is_empty() {
        String::new()
    } else {
        format!(" · {}", app.container_qos)
    };
    let title = format!(" Containers{qos} ");
    let footer = " ⏎ logs · p previous · s shell · L provider ";

    // Size the box to its contents: header + rows + borders, and wide enough
    // for the columns, the title, or the footer — whichever needs the most.
    let content_w = 2 // list highlight symbol ("▌ ")
        + name_width
        + C_GAP + C_CPU
        + C_GAP + C_CPU_PCT
        + C_GAP + C_MEM
        + C_GAP + C_MEM_PCT;
    let inner_w = content_w
        .max(title.chars().count())
        .max(footer.chars().count());
    // +2 borders, +1 so the last column doesn't touch the right border.
    let popup_w = (inner_w as u16 + 3).min(area.width);
    let rows = app.container_list.len() as u16;
    let popup_h = (rows + 3).clamp(5, area.height); // header + rows + 2 borders

    let popup = centered_rect_exact(popup_w, popup_h, area);
    clear_region(frame, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border_focused())
        .title(Span::styled(title, theme::title()))
        .title_bottom(Line::from(Span::styled(footer, theme::dim())).right_aligned());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [header_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    // Indent the header past the 2-column highlight gutter so it lines up with
    // the rows underneath it.
    frame.render_widget(
        Paragraph::new(header),
        Rect {
            x: header_area.x + 2,
            width: header_area.width.saturating_sub(2),
            ..header_area
        },
    );
    let list = List::new(items)
        .highlight_style(theme::selected_row())
        .highlight_symbol("▌ ")
        .highlight_spacing(HighlightSpacing::Always);
    frame.render_stateful_widget(list, list_area, &mut app.container_state);
}

fn draw_prompt_popup(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect_with_min(60, 34, 44, 8, area);
    clear_region(frame, popup);
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", app.prompt_label),
            Style::default().fg(theme::text()),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ▸ ", Style::default().fg(theme::peach())),
            Span::styled(app.prompt_input.clone(), Style::default().fg(theme::text())),
            Span::styled("█", Style::default().fg(theme::peach())),
        ]),
        Line::from(""),
        Line::from(Span::styled("  enter: apply    esc: cancel", theme::dim())),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme::peach()))
                .title(Span::styled(" Input ", Style::default().fg(theme::peach()))),
        ),
        popup,
    );
}

fn draw_set_image(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .container_list
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let img = app.image_values.get(i).map(String::as_str).unwrap_or("");
            ListItem::new(Line::from(vec![
                Span::styled(format!("{c}  "), Style::default().fg(theme::text())),
                Span::styled("→ ", theme::dim()),
                Span::styled(img.to_string(), Style::default().fg(theme::peach())),
            ]))
        })
        .collect();
    render_popup_list(
        frame,
        area,
        70,
        60,
        items,
        Span::styled(" Set Image (⏎ to edit container) ", theme::title()),
        &mut app.container_state,
    );
}

fn draw_confirm(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect_with_min(50, 20, 56, 7, area);
    clear_region(frame, popup);
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", app.confirm_label),
            Style::default().fg(theme::text()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            confirm_action_hint(app.confirm_allows_force_toggle(), ConfirmHintStyle::Popup),
            Style::default().fg(theme::yellow()),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme::red()))
                .title(Span::styled(" Confirm ", Style::default().fg(theme::red()))),
        ),
        popup,
    );
}

/// Command-palette suggestion list, anchored bottom-left over the table.
fn draw_palette(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.cmd_suggestions.is_empty() {
        return;
    }
    let shown = app.cmd_suggestions.len().min(12) as u16;
    let h = shown + 2;
    let w = area.width.saturating_sub(4).min(46);
    let rect = Rect {
        x: area.x + 1,
        y: area.y + area.height.saturating_sub(h + 1),
        width: w,
        height: h,
    };
    clear_region(frame, rect);
    let items: Vec<ListItem> = app
        .cmd_suggestions
        .iter()
        .map(|s| match s.kind {
            // Commands stand out (peach `:name` + a tag) so they read as actions
            // rather than resource kinds.
            SuggestKind::Command => ListItem::new(Line::from(vec![
                Span::styled(format!(":{}", s.label), Style::default().fg(theme::peach())),
                Span::styled("  cmd", theme::dim()),
            ])),
            SuggestKind::Resource => ListItem::new(Span::styled(
                s.label.clone(),
                Style::default().fg(theme::text()),
            )),
            // Argument completions echo the header colors (namespace green,
            // context mauve) with a tag, so they read as an argument choice.
            SuggestKind::Namespace => ListItem::new(Line::from(vec![
                Span::styled(s.label.clone(), Style::default().fg(theme::green())),
                Span::styled("  ns", theme::dim()),
            ])),
            SuggestKind::Context => ListItem::new(Line::from(vec![
                Span::styled(s.label.clone(), Style::default().fg(theme::mauve())),
                Span::styled("  ctx", theme::dim()),
            ])),
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.cmd_sel));
    render_framed_list(
        frame,
        rect,
        items,
        Span::styled(" commands & resources (tab/↑↓ · ⏎) ", theme::title()),
        &mut state,
    );
}

/// Xray hierarchical tree (owner → children → containers).
fn draw_xray(frame: &mut Frame, app: &mut App, area: Rect) {
    let glyph = |kind: &str| match kind {
        "deployment" => ("◈", theme::blue()),
        "replicaset" => ("◇", theme::sapphire()),
        "statefulset" => ("◈", theme::mauve()),
        "daemonset" => ("◈", theme::pink()),
        "pod" => ("●", theme::green()),
        "container" => ("▪", theme::teal()),
        _ => ("◆", theme::peach()),
    };
    let items: Vec<ListItem> = app
        .xray_items
        .iter()
        .map(|it| {
            let (g, color) = glyph(&it.kind);
            let indent = "  ".repeat(it.depth);
            let label = it.container.clone().unwrap_or_else(|| it.name.clone());
            let mut spans = vec![
                Span::raw(indent),
                Span::styled(format!("{g} "), Style::default().fg(color)),
                Span::styled(label, Style::default().fg(theme::text())),
            ];
            if !it.status.is_empty() {
                let sc = theme::status_color(&it.status);
                spans.push(Span::styled(
                    format!("  {}", it.status),
                    Style::default().fg(sc),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let title = format!(
        " Xray [{}]  (⏎ logs · r refresh · esc back) ",
        app.xray_items.len()
    );
    render_framed_list(
        frame,
        area,
        items,
        Span::styled(title, theme::title()),
        &mut app.xray_state,
    );
}

/// Explain-unhealthy view: a ranked, evidence-backed list of findings for the
/// selected object. Lines carrying a navigation target are marked with a `→`.
fn draw_explain(frame: &mut Frame, app: &mut App, area: Rect) {
    use crate::explain::Level;
    let color = |level: Level| match level {
        Level::Heading => theme::yellow(),
        Level::Info => theme::text(),
        Level::Good => theme::green(),
        Level::Warn => theme::peach(),
        Level::Critical => theme::red(),
        Level::Evidence => theme::subtext0(),
    };

    if app.explain_items.is_empty() {
        let items = vec![ListItem::new(Line::from(Span::styled(
            "gathering evidence…",
            theme::dim(),
        )))];
        let mut state = ListState::default();
        render_framed_list(
            frame,
            area,
            items,
            Span::styled(format!(" {} ", app.explain_title), theme::title()),
            &mut state,
        );
        return;
    }

    let items: Vec<ListItem> = app
        .explain_items
        .iter()
        .map(|f| {
            let indent = "  ".repeat(f.indent as usize);
            let mut spans = vec![Span::raw(indent)];
            let style = match f.level {
                Level::Heading => Style::default()
                    .fg(color(f.level))
                    .add_modifier(Modifier::BOLD),
                _ => Style::default().fg(color(f.level)),
            };
            spans.push(Span::styled(f.text.clone(), style));
            // A trailing arrow marks a line you can jump into (evidence nav).
            if f.target.is_some() {
                spans.push(Span::styled("  →", theme::dim()));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = format!(
        " {}  ({} findings · ⏎/E/l evidence · r refresh) ",
        app.explain_title,
        app.explain_items.len()
    );
    render_framed_list(
        frame,
        area,
        items,
        Span::styled(title, theme::title()),
        &mut app.explain_state,
    );
}

/// Session-local timeline: the state changes observed for one object while
/// sofka has been watching, oldest first.
fn draw_timeline(frame: &mut Frame, app: &mut App, area: Rect) {
    use crate::timeline::Level;
    let color = |level: Level| match level {
        Level::Info => theme::text(),
        Level::Good => theme::green(),
        Level::Warn => theme::peach(),
        Level::Bad => theme::red(),
    };
    let (target, entries) = match &app.timeline_target {
        Some((plural, rk)) => (rk.clone(), app.timeline.entries(plural, rk)),
        None => (String::new(), None),
    };
    let count = entries.map(|e| e.len()).unwrap_or(0);

    let items: Vec<ListItem> = match entries {
        Some(e) if !e.is_empty() => e
            .iter()
            .map(|entry| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{}  ", crate::timeline::clock(entry.at)),
                        theme::dim(),
                    ),
                    Span::styled(entry.text.clone(), Style::default().fg(color(entry.level))),
                ]))
            })
            .collect(),
        _ => vec![ListItem::new(Line::from(Span::styled(
            "no changes observed yet — the timeline records what happens while sofka watches",
            theme::dim(),
        )))],
    };

    let title = format!(" {target} — timeline  ({count} events · session-local) ");
    render_framed_list(
        frame,
        area,
        items,
        Span::styled(title, theme::title()),
        &mut app.timeline_state,
    );
}

/// Pulse dashboard: cluster-health tiles.
fn draw_pulse(frame: &mut Frame, app: &App, area: Rect) {
    let p = &app.pulse;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let cols = |r: Rect| {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ])
            .split(r)
    };
    let top = cols(rows[0]);
    let bot = cols(rows[1]);

    gauge_tile(frame, top[0], "Nodes Ready", p.nodes_ready, p.nodes_total);
    pods_tile(frame, top[1], p);
    gauge_tile(
        frame,
        top[2],
        "Deployments",
        p.deploys_ready,
        p.deploys_total,
    );
    gauge_tile(frame, bot[0], "StatefulSets", p.sts_ready, p.sts_total);
    gauge_tile(frame, bot[1], "DaemonSets", p.ds_ready, p.ds_total);
    counts_tile(frame, bot[2], p);
}

fn gauge_tile(frame: &mut Frame, area: Rect, label: &str, ready: usize, total: usize) {
    let ratio = if total == 0 {
        1.0
    } else {
        ready as f64 / total as f64
    };
    let color = if total == 0 {
        theme::overlay1()
    } else if ready == total {
        theme::green()
    } else if ratio >= 0.5 {
        theme::yellow()
    } else {
        theme::red()
    };
    let g = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::border())
                .title(Span::styled(format!(" {label} "), theme::title())),
        )
        .gauge_style(Style::default().fg(color).bg(theme::surface0()))
        .ratio(ratio.clamp(0.0, 1.0))
        .label(format!("{ready}/{total}"));
    frame.render_widget(g, area);
}

fn pods_tile(frame: &mut Frame, area: Rect, p: &crate::store::Pulse) {
    let row = |label: &str, n: usize, color| {
        Line::from(vec![
            Span::styled(format!("  {label:<11}"), Style::default().fg(color)),
            Span::styled(n.to_string(), Style::default().fg(theme::text())),
        ])
    };
    let lines = vec![
        row("Running", p.pods_running, theme::green()),
        row("Pending", p.pods_pending, theme::yellow()),
        row("Failed", p.pods_failed, theme::red()),
        row("Succeeded", p.pods_succeeded, theme::blue()),
        row("Total", p.pods_total, theme::subtext0()),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::border())
                .title(Span::styled(" Pods ", theme::title())),
        ),
        area,
    );
}

fn counts_tile(frame: &mut Frame, area: Rect, p: &crate::store::Pulse) {
    let lines = vec![
        Line::from(vec![
            Span::styled("  PVCs Bound  ", Style::default().fg(theme::teal())),
            Span::styled(
                format!("{}/{}", p.pvc_bound, p.pvc_total),
                Style::default().fg(theme::text()),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Jobs        ", Style::default().fg(theme::mauve())),
            Span::styled(p.jobs_total.to_string(), Style::default().fg(theme::text())),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::border())
                .title(Span::styled(" Storage / Batch ", theme::title())),
        ),
        area,
    );
}

fn draw_prompt(frame: &mut Frame, app: &App, area: Rect) {
    let line = match app.mode {
        Mode::Command => Line::from(vec![
            Span::styled(
                ":",
                Style::default()
                    .fg(theme::mauve())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(app.command.clone(), Style::default().fg(theme::text())),
            Span::styled("█", Style::default().fg(theme::mauve())),
        ]),
        Mode::Filter => {
            let mut spans = vec![
                Span::styled(
                    "/",
                    Style::default()
                        .fg(theme::teal())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(app.filter.clone(), Style::default().fg(theme::text())),
                Span::styled("█", Style::default().fg(theme::teal())),
            ];
            // Structured-grammar feedback: a parse error, a `-l`/`-f`
            // selector waiting for ⏎ to restart the watch server-side, or
            // confirmation that the watch is already selector-scoped.
            if let Some(err) = app.filter_error() {
                spans.push(Span::styled(
                    format!("  ✗ {err}"),
                    Style::default().fg(theme::red()),
                ));
            } else if app.filter_selectors_pending() {
                spans.push(Span::styled(
                    "  ⏎ apply server-side",
                    Style::default().fg(theme::yellow()),
                ));
            } else if app.filter_server_side() {
                spans.push(Span::styled("  ·server", theme::dim()));
            }
            Line::from(spans)
        }
        Mode::LogFilter => Line::from(vec![
            Span::styled(
                "log search /",
                Style::default()
                    .fg(theme::teal())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(app.logs.filter.clone(), Style::default().fg(theme::text())),
            Span::styled("█", Style::default().fg(theme::teal())),
        ]),
        Mode::DocFilter => {
            let query = if app.doc_filter_return == Mode::Help {
                app.help_filter.clone()
            } else {
                app.detail.filter.clone()
            };
            Line::from(vec![
                Span::styled(
                    "search /",
                    Style::default()
                        .fg(theme::teal())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(query, Style::default().fg(theme::text())),
                Span::styled("█", Style::default().fg(theme::teal())),
            ])
        }
        Mode::Confirm => Line::from(Span::styled(
            confirm_action_hint(app.confirm_allows_force_toggle(), ConfirmHintStyle::Prompt),
            Style::default().fg(theme::yellow()),
        )),
        Mode::Logs => {
            let hint = if app.provider_logs_active() {
                "  /search  s:autoscroll  w:wrap  t:timestamps  T:period  x:stop/resume  c:copy  ^s:save  esc:back"
            } else {
                "  /search  s:autoscroll  w:wrap  t:timestamps  x:stop/resume  c:copy  ^s:save  esc:back"
            };
            Line::from(Span::styled(hint, theme::dim()))
        }
        Mode::Detail | Mode::Events | Mode::Diff => {
            let hint = "  j/k:scroll  h/l:← →  g/G:top/bottom  /:search  w:wrap  c:copy  esc:back";
            Line::from(Span::styled(hint, theme::dim()))
        }
        Mode::Help => Line::from(Span::styled("  /:search  ?/esc:back", theme::dim())),
        Mode::Explain => Line::from(Span::styled(
            "  j/k: move   ⏎: go to resource   E: events   l: logs   r: refresh   esc: back",
            theme::dim(),
        )),
        Mode::Timeline => Line::from(Span::styled(
            "  j/k: move   g/G: top/bottom   esc: back",
            theme::dim(),
        )),
        Mode::FluxMenu => Line::from(Span::styled(
            "  j/k: move   enter: confirm   esc: cancel",
            theme::dim(),
        )),
        Mode::PortForwards => Line::from(Span::styled(
            "  j/k: move   x/s: stop   esc: close (others keep running)",
            theme::dim(),
        )),
        _ => {
            // Per-resource verbs live in the header hint column when it
            // fits; only repeat the full line when the header dropped it.
            let hint = if header_hints_fit(frame.area().width) {
                "  :resource  /filter  S:sort I:invert  w:wide  space:mark  [ ]:history  0:all-ns  ?:help"
            } else {
                "  :resource  /filter  S:sort I:invert  w:wide  ⏎drill  y:yaml d:describe l:logs e:edit s:shell/scale i:image r:restart f:fwd ^d:del  ?:help"
            };
            Line::from(Span::styled(hint, theme::dim()))
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let style = if app.flash_err {
        Style::default().fg(theme::red())
    } else {
        Style::default().fg(theme::subtext0())
    };
    let synced = if app.store.synced {
        "● live"
    } else {
        "○ syncing"
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(12)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {}", app.flash), style))),
        cols[0],
    );
    let sync_color = if app.store.synced {
        theme::green()
    } else {
        theme::yellow()
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            synced,
            Style::default().fg(sync_color),
        )))
        .alignment(Alignment::Right),
        cols[1],
    );
}

#[derive(Clone, Copy)]
enum ConfirmHintStyle {
    Popup,
    Prompt,
}

fn confirm_action_hint(allows_force: bool, style: ConfirmHintStyle) -> &'static str {
    match (allows_force, style) {
        (true, ConfirmHintStyle::Popup) => {
            "  [y] confirm    [f] toggle force    [c] cascade    [n] cancel"
        }
        (false, ConfirmHintStyle::Popup) => "  [y] confirm    [n] cancel",
        (true, ConfirmHintStyle::Prompt) => {
            "  y/enter: confirm   f: toggle force   c: cascade   n/esc: cancel"
        }
        (false, ConfirmHintStyle::Prompt) => "  y/enter: confirm   n/esc: cancel",
    }
}

/// Clear a popup region before drawing on top of it. `Clear` resets the cells
/// to the terminal default; with the skin background enabled that would punch a
/// transparent hole through the fill, so repaint `base` over the cleared cells.
fn clear_region(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);
    if let Some(bg) = theme::background() {
        frame.buffer_mut().set_style(area, Style::default().bg(bg));
    }
}

fn render_popup_list<'a, T>(
    frame: &mut Frame,
    area: Rect,
    percent_x: u16,
    percent_y: u16,
    items: Vec<ListItem<'a>>,
    title: T,
    state: &mut ListState,
) where
    T: Into<Line<'a>>,
{
    let popup = centered_rect_with_min(percent_x, percent_y, 32, 8, area);
    clear_region(frame, popup);
    render_framed_list(frame, popup, items, title, state);
}

fn render_framed_list<'a, T>(
    frame: &mut Frame,
    area: Rect,
    items: Vec<ListItem<'a>>,
    title: T,
    state: &mut ListState,
) where
    T: Into<Line<'a>>,
{
    let list = List::new(items)
        .highlight_style(theme::selected_row())
        .highlight_symbol("▌ ")
        .highlight_spacing(HighlightSpacing::Always)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::border_focused())
                .title(title.into()),
        );
    frame.render_stateful_widget(list, area, state);
}

/// Center a fixed-size rectangle within `r`, clamped to `r`'s bounds. Used by
/// popups that size themselves to their content rather than a percentage.
fn centered_rect_exact(width: u16, height: u16, r: Rect) -> Rect {
    let width = width.min(r.width);
    let height = height.min(r.height);
    Rect {
        x: r.x + (r.width - width) / 2,
        y: r.y + (r.height - height) / 2,
        width,
        height,
    }
}

fn centered_rect_with_min(
    percent_x: u16,
    percent_y: u16,
    min_width: u16,
    min_height: u16,
    r: Rect,
) -> Rect {
    let pct_w = (u32::from(r.width) * u32::from(percent_x.min(100)) / 100) as u16;
    let pct_h = (u32::from(r.height) * u32::from(percent_y.min(100)) / 100) as u16;
    let width = pct_w.max(min_width).min(r.width);
    let height = pct_h.max(min_height).min(r.height);
    Rect {
        x: r.x + (r.width - width) / 2,
        y: r.y + (r.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_background(true)` fills the whole frame — including cells no widget
    /// draws on and popup regions cleared by `Clear` — with the skin's `base`,
    /// while `false` leaves the terminal background (Reset) untouched.
    #[tokio::test]
    async fn background_fill_paints_base_when_enabled() {
        use crate::app::Suggestion;
        use crate::k8s::Cluster;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut app = App::new(Cluster::fake(), tx);
        app.command = "de".into();
        app.mode = Mode::Command; // draw a popup so a Clear region is exercised
        app.cmd_suggestions = vec![Suggestion {
            label: "deployments".into(),
            kind: SuggestKind::Resource,
        }];

        let render = |app: &mut App| {
            let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
            term.draw(|f| draw(f, app)).unwrap();
            term.backend().buffer().clone()
        };

        // Assertions avoid the exact palette value (theme state is a shared
        // global that parallel tests mutate); they check the fill *behavior*.
        theme::set_background(false);
        let off = render(&mut app);
        // Off: an untouched corner keeps the terminal default background.
        assert_eq!(off[(0, 0)].bg, ratatui::style::Color::Reset);

        theme::set_background(true);
        let on = render(&mut app);
        // On: the corner (no widget draws there) is now a solid fill, and at
        // least one full row — popup interior included — shares that one color.
        let fill = on[(0, 0)].bg;
        assert_ne!(fill, ratatui::style::Color::Reset);
        assert!(
            (0..on.area.height).any(|y| (0..on.area.width).all(|x| on[(x, y)].bg == fill)),
            "expected a row uniformly filled with the background color"
        );

        theme::set_background(false); // don't leak global state to other tests
    }

    #[test]
    fn all_ready_requires_full_fraction() {
        assert!(all_ready("2/2"));
        assert!(all_ready("0/0"));
        assert!(!all_ready("1/2"));
        assert!(!all_ready("0/1"));
        // Non-fraction cells (other kinds' status columns) never trigger it.
        assert!(all_ready("Ready"));
    }

    /// The scroll math (`wrapped_height`) and the renderer (`wrap_line`) must
    /// produce the same row count for any input, or follow/clamping drifts.
    #[test]
    fn wrapped_height_matches_wrap_line() {
        let cases = [
            "",
            "short",
            "exactly-ten",
            "a much longer plain ascii log line that wraps a few times over",
            // ANSI escapes are zero-width.
            "\x1b[33mwarn\x1b[0m something colorful happened in the reconcile loop",
            // Wide CJK glyphs take two columns and never straddle a break.
            "日本語のログ行 with mixed ascii ワイド文字",
            // Combining mark (zero width) + multi-byte.
            "cafe\u{301} naïve élan über — dash",
            // Lone ESC and non-SGR CSI are swallowed.
            "\x1bodd \x1b[2Kcleared line",
        ];
        for w in [1usize, 3, 10, 37, 120] {
            for raw in cases {
                let rendered = render_log_line(raw, "");
                let rows = wrap_line(rendered, w).len();
                assert_eq!(
                    wrapped_height(raw, w),
                    rows,
                    "height/split disagree for {raw:?} at width {w}"
                );
            }
        }
    }

    #[test]
    fn wrapped_height_counts_columns_not_bytes() {
        assert_eq!(wrapped_height("", 10), 1); // empty line still takes a row
        assert_eq!(wrapped_height("aaaaaaaaaa", 10), 1); // exact fit
        assert_eq!(wrapped_height("aaaaaaaaaab", 10), 2);
        // 5 wide chars = 10 columns → one row at width 10, not "5 chars fit".
        assert_eq!(wrapped_height("五五五五五", 10), 1);
        assert_eq!(wrapped_height("五五五五五五", 10), 2);
        // ANSI escapes don't consume columns.
        assert_eq!(wrapped_height("\x1b[31maaaaaaaaaa\x1b[0m", 10), 1);
    }

    #[test]
    fn visible_line_window_clamps_to_viewport() {
        assert_eq!(visible_line_window(100, 10, 20), (10, 30));
        assert_eq!(visible_line_window(100, 95, 20), (95, 100));
        assert_eq!(visible_line_window(100, 150, 20), (100, 100));
        assert_eq!(visible_line_window(100, 10, 0), (10, 10));
    }

    #[test]
    fn centered_rect_with_min_keeps_popups_readable() {
        let area = Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 20,
        };
        assert_eq!(
            centered_rect_with_min(50, 20, 56, 7, area),
            Rect {
                x: 32,
                y: 26,
                width: 56,
                height: 7,
            }
        );

        let tiny = Rect {
            x: 3,
            y: 4,
            width: 40,
            height: 5,
        };
        assert_eq!(centered_rect_with_min(50, 20, 56, 7, tiny), tiny);
    }

    #[test]
    fn confirm_hint_mentions_force_only_when_supported() {
        assert!(confirm_action_hint(true, ConfirmHintStyle::Popup).contains("toggle force"));
        assert!(confirm_action_hint(true, ConfirmHintStyle::Prompt).contains("toggle force"));
        assert!(!confirm_action_hint(false, ConfirmHintStyle::Popup).contains("toggle force"));
        assert!(!confirm_action_hint(false, ConfirmHintStyle::Prompt).contains("toggle force"));
        assert!(confirm_action_hint(true, ConfirmHintStyle::Popup).contains("cascade"));
        assert!(confirm_action_hint(true, ConfirmHintStyle::Prompt).contains("cascade"));
        assert!(!confirm_action_hint(false, ConfirmHintStyle::Popup).contains("cascade"));
        assert!(!confirm_action_hint(false, ConfirmHintStyle::Prompt).contains("cascade"));
    }

    #[test]
    fn log_levels_colorize() {
        // Space-delimited level, with "error" later in the message: warn wins.
        assert_eq!(
            log_level_color("pod vmagent 2026-06-30T12:00:26.985Z warn lib: the last error: x"),
            theme::peach()
        );
        // Glued-after-timestamp info (config-reloader style) stays default.
        assert_eq!(
            log_level_color("[config-reloader] 2026-06-27T04:56:24.216Zinfo k8s_watch.go:153 x"),
            theme::text()
        );
        // Tab-delimited info.
        assert_eq!(
            log_level_color("ts 2026\tinfo\tVictoriaMetrics added targets"),
            theme::text()
        );
        // Plain error level.
        assert_eq!(
            log_level_color("2026-06-30T12 error connection refused"),
            theme::red()
        );
        // klog prefix.
        assert_eq!(
            log_level_color("E0627 12:00:00.000 controller failed"),
            theme::red()
        );
        assert_eq!(
            log_level_color("W0627 12:00:00.000 retrying"),
            theme::peach()
        );
        // logfmt level=debug.
        assert_eq!(
            log_level_color("msg=hi level=debug caller=x"),
            theme::overlay1()
        );
    }

    #[test]
    fn json_log_levels_colorize() {
        let line = |lvl: &str, msg: &str| {
            format!(
                "[main] {{\"timestamp\":\"2026-06-30T12:52:20.876Z\",\"level\":\"{lvl}\",\"message\":\"{msg}\",\"service\":\"screenshoter\"}}"
            )
        };
        assert_eq!(
            log_level_color(&line("DEBUG", "request_started")),
            theme::overlay1()
        );
        assert_eq!(
            log_level_color(&line("INFO", "request_completed")),
            theme::text()
        );
        assert_eq!(
            log_level_color(&line("WARN", "unauthorized_request")),
            theme::peach()
        );
        assert_eq!(log_level_color(&line("ERROR", "boom")), theme::red());
        // JSON level is authoritative: "error" in the message can't override WARN.
        assert_eq!(
            log_level_color(&line("WARN", "the last error occurred")),
            theme::peach()
        );
        // Whitespace after the colon is tolerated.
        assert_eq!(log_level_color(r#"{"level": "warning"}"#), theme::peach());
        // Non-structured rod lines have no level → default color.
        assert_eq!(log_level_color("[rod] Killed PID: 25258"), theme::text());
    }

    #[test]
    fn source_prefix_detection() {
        // "[rod] " is 6 bytes including the trailing space.
        assert_eq!(source_prefix("[rod] Close ws://x").map(|(e, _)| e), Some(6));
        assert_eq!(
            source_prefix("[main] {\"level\":\"info\"}").map(|(e, _)| e),
            Some(7)
        );
        // No trailing space still detected.
        assert_eq!(source_prefix("[x]done").map(|(e, _)| e), Some(3));
        assert_eq!(source_prefix("no prefix here"), None);
        assert_eq!(source_prefix("[]empty"), None);
    }

    #[test]
    fn source_color_is_stable_and_distinct() {
        // Same label → same color across calls.
        assert_eq!(source_color("rod"), source_color("rod"));
        // Reserved severity/highlight colors are never used for a source.
        for label in ["rod", "main", "istio-proxy", "app", "vmagent"] {
            let c = source_color(label);
            assert_ne!(c, theme::red());
            assert_ne!(c, theme::peach());
            assert_ne!(c, theme::yellow());
        }
        // The two prefixes in the screenshot land on different colors.
        assert_ne!(source_color("rod"), source_color("main"));
    }

    #[test]
    fn render_colors_prefix_then_body() {
        let line = render_log_line("[rod] Killed PID: 25258", "");
        // First span is the colored source prefix, kept verbatim.
        assert_eq!(line.spans[0].content, "[rod] ");
        assert_eq!(line.spans[0].style.fg, Some(source_color("rod")));
    }

    #[test]
    fn leading_timestamp_detection() {
        // Space-terminated RFC3339 → dimmed.
        assert_eq!(
            leading_timestamp("2026-06-30T12:52:20.876Z hello"),
            Some(24)
        );
        assert_eq!(leading_timestamp("2026-06-30T12:52:20Z msg"), Some(20));
        assert_eq!(
            leading_timestamp("2026-06-30T12:52:20.5+02:00 msg"),
            Some(27)
        );
        // Glued to the message (config-reloader style) → NOT a timestamp.
        assert_eq!(
            leading_timestamp("2026-06-27T04:56:24.216Zinfo k8s_watch"),
            None
        );
        // Not a timestamp at all.
        assert_eq!(leading_timestamp("Close ws://127.0.0.1"), None);
    }

    #[test]
    fn strips_and_interprets_ansi() {
        // Caddy-style line: level token wrapped in an SGR color, escapes must
        // not survive into the rendered text.
        let raw = "2026/07/01 08:43:13 \x1b[34mINFO\x1b[0m WAF started";
        let line = render_log_line(raw, "");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "2026/07/01 08:43:13 INFO WAF started");
        assert!(!text.contains('\x1b') && !text.contains("[34m"));
        // The "INFO" run picked up the ANSI blue → theme blue.
        let info = line.spans.iter().find(|s| s.content == "INFO").unwrap();
        assert_eq!(info.style.fg, Some(theme::blue()));
    }

    #[test]
    fn ansi_runs_plain_string_is_single_run() {
        let runs = ansi_runs("plain text");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "plain text");
        assert_eq!(runs[0].color, None);
    }

    #[test]
    fn ansi_truecolor_passes_through() {
        let runs = ansi_runs("\x1b[38;2;10;20;30mX\x1b[0m");
        assert_eq!(runs[0].text, "X");
        assert_eq!(runs[0].color, Some(Color::Rgb(10, 20, 30)));
        assert_eq!(strip_ansi("\x1b[1;31mE\x1b[0mrror"), "Error");
    }

    #[test]
    fn render_dims_leading_timestamp() {
        let line = render_log_line("2026-06-30T12:52:20.876Z request done", "");
        assert_eq!(line.spans[0].content, "2026-06-30T12:52:20.876Z");
        assert_eq!(line.spans[0].style.fg, theme::dim().fg);
    }

    #[test]
    fn value_styling() {
        assert_eq!(value_style("3").fg, Some(theme::peach()));
        assert_eq!(value_style("true").fg, Some(theme::mauve()));
        assert_eq!(value_style("<none>").fg, Some(theme::mauve()));
        assert_eq!(value_style("Running").fg, Some(theme::green()));
        assert_eq!(value_style("nginx:1.25").fg, Some(theme::text()));
    }

    #[test]
    fn yaml_highlighting() {
        // Comment dimmed.
        assert_eq!(highlight_yaml("  # note")[0].style.fg, theme::dim().fg);
        // Section header in mauve.
        assert_eq!(
            highlight_yaml("Containers:")[0].style.fg,
            Some(theme::mauve())
        );
        // key: value — key in sky, value tinted by status.
        let spans = highlight_yaml("Status:    Running");
        assert_eq!(spans[0].content, "Status");
        assert_eq!(spans[0].style.fg, Some(theme::sky()));
        assert_eq!(spans.last().unwrap().content, "Running");
        assert_eq!(spans.last().unwrap().style.fg, Some(theme::green()));
    }
}
