//! All ratatui rendering.

use crate::keymap::Action;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Gauge, HighlightSpacing, List, ListItem, ListState,
    Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Sparkline,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, DEFAULT_SORT_LABEL, Mode, Pane, SuggestKind, TRANSFER_MENU_ITEMS};
use crate::{columns, theme};

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct RenderCell<'a> {
    content: Text<'a>,
    style: Style,
}

impl<'a, T: Into<Text<'a>>> From<T> for RenderCell<'a> {
    fn from(content: T) -> Self {
        Self {
            content: content.into(),
            style: Style::default(),
        }
    }
}

impl RenderCell<'_> {
    fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    fn render(&self, area: Rect, buf: &mut ratatui::buffer::Buffer, row: Style, selected: bool) {
        use ratatui::widgets::Widget;
        buf.set_style(area, row);
        buf.set_style(area, self.style);
        (&self.content).render(area, buf);
        if selected {
            buf.set_style(area, theme::selected_row());
        }
    }
}

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

    fn into_cell(self) -> RenderCell<'a> {
        match self {
            TableCellText::Borrowed(value) => RenderCell::from(value),
            TableCellText::Owned(value) => RenderCell::from(value),
        }
    }

    /// Like [`Self::into_cell`], honoring a custom column's alignment.
    fn into_cell_aligned(self, align: Option<Alignment>) -> RenderCell<'a> {
        let Some(align) = align else {
            return self.into_cell();
        };
        match self {
            TableCellText::Borrowed(value) => RenderCell::from(Text::from(value).alignment(align)),
            TableCellText::Owned(value) => RenderCell::from(Text::from(value).alignment(align)),
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

/// Refresh layout after a resize before another input event can use its dimensions.
pub fn resize<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    app: &mut App,
) -> Result<(), B::Error> {
    terminal.draw(|frame| draw(frame, app))?;
    Ok(())
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let show_scrollbars = app.scrollbars_visible();
    // Fill the whole frame with the skin's background first (when enabled), so
    // every view that only sets foreground colors sits on it. Widgets that set
    // their own background (the selection bar, gauges, search highlights) still
    // win where they draw.
    if let Some(bg) = theme::background() {
        let area = frame.area();
        frame.buffer_mut().set_style(area, Style::default().bg(bg));
    }

    // Compact mode (ctrl-e) trades the 7-line header + footer for a single
    // header line, so a small tiled pane is almost all table. The prompt line
    // still appears while typing a command/filter; the status line and hint
    // crumbs are folded away (a flash + sync dot ride in the compact header).
    let compact = app.compact;
    let needs_prompt = matches!(
        app.mode,
        Mode::Command | Mode::Filter | Mode::LogFilter | Mode::DocFilter
    );

    // Fullscreen logs (F): the pane takes the whole frame — no header, status
    // line, or crumbs — so terminal text selection copies clean lines. The
    // prompt line stays while typing a filter, and the lookback prompt still
    // pops up over the logs.
    if app.logs.fullscreen
        && (matches!(app.mode, Mode::Logs | Mode::LogFilter)
            || (app.mode == Mode::Prompt && app.prompt_over_logs()))
    {
        let mut constraints = vec![Constraint::Min(3)];
        if needs_prompt {
            constraints.push(Constraint::Length(1));
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(frame.area());
        draw_logs(frame, app, chunks[0]);
        if app.mode == Mode::Prompt {
            draw_prompt_popup(frame, app, chunks[0]);
        }
        if needs_prompt {
            draw_prompt(frame, app, chunks[1]);
        }
        return;
    }
    let document_mode = match app.mode {
        Mode::DocFilter => app.doc_filter_return,
        Mode::Command => app.palette_return,
        mode => mode,
    };
    if app.document_fullscreen && matches!(document_mode, Mode::Detail | Mode::Diff | Mode::Events)
    {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(u16::from(needs_prompt)),
            ])
            .split(frame.area());
        if document_mode == Mode::Diff {
            draw_diff(frame, show_scrollbars, true, &mut app.detail, chunks[0]);
        } else {
            let accent = if document_mode == Mode::Events {
                theme::peach()
            } else {
                theme::sky()
            };
            draw_scrollable(
                frame,
                show_scrollbars,
                true,
                &mut app.detail,
                chunks[0],
                accent,
            );
        }
        if app.mode == Mode::Command {
            draw_palette(frame, app, chunks[0]);
        }
        if needs_prompt {
            draw_prompt(frame, app, chunks[1]);
        }
        return;
    }
    let mut constraints = vec![
        Constraint::Length(if app.hide_header {
            0
        } else if compact {
            1
        } else {
            7
        }), // header
        Constraint::Min(3), // body
    ];
    let prompt_idx = if !compact || needs_prompt {
        constraints.push(Constraint::Length(1));
        Some(constraints.len() - 1)
    } else {
        None
    };
    let status_idx = if !compact {
        constraints.push(Constraint::Length(1));
        Some(constraints.len() - 1)
    } else {
        None
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    if !app.hide_header {
        if compact {
            draw_compact_header(frame, app, chunks[0]);
        } else {
            draw_header(frame, app, chunks[0]);
        }
    }

    match app.mode {
        Mode::Detail => draw_scrollable(
            frame,
            show_scrollbars,
            false,
            &mut app.detail,
            chunks[1],
            theme::sky(),
        ),
        Mode::Diff => draw_diff(frame, show_scrollbars, false, &mut app.detail, chunks[1]),
        Mode::Events => draw_scrollable(
            frame,
            show_scrollbars,
            false,
            &mut app.detail,
            chunks[1],
            theme::peach(),
        ),
        Mode::Logs | Mode::LogFilter => draw_logs(frame, app, chunks[1]),
        // The lookback prompt opens from the logs view — keep it underneath.
        Mode::Prompt if app.prompt_over_logs() => draw_logs(frame, app, chunks[1]),
        // While typing a doc search, keep drawing the view it was opened from
        // so the matches narrow live under the prompt.
        Mode::DocFilter => match app.doc_filter_return {
            Mode::Diff => draw_diff(frame, show_scrollbars, false, &mut app.detail, chunks[1]),
            Mode::Events => draw_scrollable(
                frame,
                show_scrollbars,
                false,
                &mut app.detail,
                chunks[1],
                theme::peach(),
            ),
            Mode::Help => draw_help(frame, app, chunks[1]),
            _ => draw_scrollable(
                frame,
                show_scrollbars,
                false,
                &mut app.detail,
                chunks[1],
                theme::sky(),
            ),
        },
        Mode::Help => draw_help(frame, app, chunks[1]),
        Mode::Pulse => draw_pulse(frame, app, chunks[1]),
        Mode::Xray => draw_xray(frame, app, chunks[1]),
        Mode::Explain => draw_explain(frame, app, chunks[1]),
        Mode::Gitops => draw_gitops(frame, app, chunks[1]),
        Mode::Argocd => draw_argocd(frame, app, chunks[1]),
        Mode::Adjacent => draw_adjacent(frame, app, chunks[1]),
        Mode::Timeline => draw_timeline(frame, app, chunks[1]),
        Mode::PortForwards => draw_port_forwards(frame, app, chunks[1]),
        Mode::Fleet => draw_fleet(frame, app, chunks[1]),
        Mode::Find => draw_find(frame, app, chunks[1]),
        Mode::PvcExplore => draw_pvc_explore(frame, app, chunks[1]),
        // A transfer confirmation or a guardrail prompt raised from the PVC
        // browser keeps the two panes underneath it, so you can still see what
        // is being copied where. Only that dialog: drawing an unrelated one
        // over the panes would suggest it was about them.
        Mode::Confirm | Mode::Prompt if app.over_pvc_browser() => {
            draw_pvc_explore(frame, app, chunks[1])
        }
        // While the palette is open, keep drawing the view it was opened
        // from, so a global `:` never flashes the table underneath it.
        Mode::Command => match app.palette_return {
            Mode::Diff => draw_diff(frame, show_scrollbars, false, &mut app.detail, chunks[1]),
            Mode::Events => draw_scrollable(
                frame,
                show_scrollbars,
                false,
                &mut app.detail,
                chunks[1],
                theme::peach(),
            ),
            Mode::Detail => draw_scrollable(
                frame,
                show_scrollbars,
                false,
                &mut app.detail,
                chunks[1],
                theme::sky(),
            ),
            Mode::Logs => draw_logs(frame, app, chunks[1]),
            Mode::Help => draw_help(frame, app, chunks[1]),
            Mode::Pulse => draw_pulse(frame, app, chunks[1]),
            Mode::Xray => draw_xray(frame, app, chunks[1]),
            Mode::Explain => draw_explain(frame, app, chunks[1]),
            Mode::Gitops => draw_gitops(frame, app, chunks[1]),
            Mode::Argocd => draw_argocd(frame, app, chunks[1]),
            Mode::Adjacent => draw_adjacent(frame, app, chunks[1]),
            Mode::Timeline => draw_timeline(frame, app, chunks[1]),
            Mode::PortForwards => draw_port_forwards(frame, app, chunks[1]),
            Mode::Fleet => draw_fleet(frame, app, chunks[1]),
            Mode::Find => draw_find(frame, app, chunks[1]),
            Mode::PvcExplore => draw_pvc_explore(frame, app, chunks[1]),
            Mode::Containers => {
                draw_table(frame, app, chunks[1]);
                draw_containers(frame, app, chunks[1]);
            }
            Mode::Confirm => {
                draw_table(frame, app, chunks[1]);
                draw_confirm(frame, app, chunks[1]);
            }
            Mode::FluxMenu => {
                draw_table(frame, app, chunks[1]);
                draw_flux_menu(frame, app, chunks[1]);
            }
            Mode::TransferMenu => {
                draw_table(frame, app, chunks[1]);
                draw_transfer_menu(frame, app, chunks[1]);
            }
            Mode::Skins => {
                draw_table(frame, app, chunks[1]);
                draw_skins(frame, app, chunks[1]);
            }
            Mode::Snapshots => {
                draw_table(frame, app, chunks[1]);
                draw_snapshots(frame, app, chunks[1]);
            }
            _ => draw_table(frame, app, chunks[1]),
        },
        _ => draw_table(frame, app, chunks[1]),
    }

    match app.mode {
        Mode::Drain => draw_drain(frame, app, chunks[1]),
        Mode::Confirm | Mode::Prompt if app.drain_confirmation() => {
            draw_drain(frame, app, chunks[1])
        }
        Mode::Namespaces => draw_namespaces(frame, app, chunks[1]),
        Mode::Contexts => draw_contexts(frame, app, chunks[1]),
        Mode::SortPicker => draw_sort_picker(frame, app, chunks[1]),
        Mode::CopyPicker => draw_copy_picker(frame, app, chunks[1]),
        Mode::Containers => draw_containers(frame, app, chunks[1]),
        Mode::SetImage => draw_set_image(frame, app, chunks[1]),
        Mode::Confirm => draw_confirm(frame, app, chunks[1]),
        // The rename prompt opens from the context switcher — keep the
        // picker visible underneath it.
        Mode::Prompt if app.prompt_over_contexts() => {
            draw_contexts(frame, app, chunks[1]);
            draw_prompt_popup(frame, app, chunks[1]);
        }
        Mode::Prompt => draw_prompt_popup(frame, app, chunks[1]),
        Mode::Command => draw_palette(frame, app, chunks[1]),
        Mode::FluxMenu => draw_flux_menu(frame, app, chunks[1]),
        Mode::TransferMenu => draw_transfer_menu(frame, app, chunks[1]),
        Mode::Skins => draw_skins(frame, app, chunks[1]),
        Mode::Snapshots => draw_snapshots(frame, app, chunks[1]),
        Mode::PortForwardPicker => draw_port_forward_picker(frame, app, chunks[1]),
        _ => {}
    }

    if let Some(i) = prompt_idx {
        draw_prompt(frame, app, chunks[i]);
    }
    if let Some(i) = status_idx {
        draw_status(frame, app, chunks[i]);
    }
}

/// Width reserved for the per-kind key-hint column inside the header box:
/// Three fixed columns with two spaces between columns.
const HEADER_HINT_COLUMNS: [usize; 3] = [16, 13, 13];
const HEADER_HINTS_WIDTH: u16 = 46;
/// Minimum width the info cluster keeps before the hint column may appear.
const HEADER_INFO_MIN: u16 = 44;

fn header_title(server_version: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(" sofka ", theme::title())];
    if !server_version.is_empty() {
        spans.push(Span::styled("· K8s Rev: ", theme::dim()));
        spans.push(Span::styled(
            server_version.to_string(),
            Style::default().fg(theme::sapphire()),
        ));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn diagnostic_value<'a>(app: &App, value: &'a str) -> std::borrow::Cow<'a, str> {
    let mode = match app.mode {
        Mode::Command => app.palette_return,
        Mode::DocFilter => app.doc_filter_return,
        mode => mode,
    };
    if mode == Mode::Detail && app.detail.redact_header {
        crate::redact::text(value)
    } else {
        std::borrow::Cow::Borrowed(value)
    }
}

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
            Span::styled(
                diagnostic_value(app, &val).into_owned(),
                Style::default().fg(color),
            ),
        ])
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(header_title(&app.cluster.server_version));
    let inner = block.inner(cols[0]);
    frame.render_widget(block, cols[0]);

    let hints = header_hints(app);
    let show_hints = !hints.is_empty() && header_hints_fit(area.width);
    let info_width = if show_hints {
        inner.width.saturating_sub(HEADER_HINTS_WIDTH)
    } else {
        inner.width
    };

    let mut context_line = field("Context:", app.cluster.context.clone(), theme::mauve());
    if app.readonly {
        context_line.push_span(Span::styled(
            "  [read-only]",
            Style::default().fg(theme::red()),
        ));
    }
    let mut namespace_line = field("Namespace:", ns.clone(), theme::green());
    let favorites_width = usize::from(info_width).saturating_sub(12 + ns.width());
    for span in favorite_namespace_spans(app, favorites_width) {
        namespace_line.push_span(span);
    }
    let info = vec![
        context_line,
        field(
            "Cluster:",
            app.cluster.cluster_url.clone(),
            theme::sapphire(),
        ),
        namespace_line,
        field("Resource:", kind, theme::peach()),
        field("Count:", app.store.len().to_string(), theme::text()),
    ];

    // Per-kind key hints share the box with the info cluster (k9s-style);
    // narrow terminals collapse back to info-only and keep the full hint
    // line at the bottom instead.
    if show_hints {
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

fn favorite_namespace_spans(app: &App, width: usize) -> Vec<Span<'static>> {
    let key_style = Style::default()
        .fg(theme::sky())
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut used = 0;
    for (namespace, action) in app
        .namespace_favorites
        .iter()
        .zip(Action::FAVORITE_NAMESPACES)
    {
        if namespace.is_empty() || app.keymap.chords("table", action).is_empty() {
            continue;
        }
        let key = app.keymap.first_label("table", action);
        let entry_width = 3 + key.width() + namespace.width();
        if used + entry_width > width {
            if used + 3 <= width {
                spans.push(Span::styled("  …", theme::dim()));
            }
            break;
        }
        used += entry_width;
        let name_style = if *namespace == app.namespace {
            Style::default().fg(theme::green())
        } else {
            theme::dim()
        };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(key.to_string(), key_style));
        spans.push(Span::styled(format!(" {namespace}"), name_style));
    }
    spans
}

/// The single-line header for compact mode (`ctrl-e`): kind · count ·
/// namespace · context on the left; a transient flash and the live/sync dot on
/// the right. Everything the full header shows that still matters when you've
/// traded it for screen space.
fn draw_compact_header(frame: &mut Frame, app: &App, area: Rect) {
    let ns = if app.all_namespaces() {
        "<all>".to_string()
    } else if app.namespace.is_empty() {
        "<none>".to_string()
    } else {
        app.namespace.clone()
    };
    let mut kind = app.resource_title();
    if let Some(scope) = &app.scope_label {
        kind = format!("{kind} ‹ {scope}");
    }

    let mut spans = vec![
        Span::styled(" sofka ", theme::title()),
        Span::styled(kind, Style::default().fg(theme::peach())),
        Span::styled(format!(" [{}]", app.store.len()), theme::dim()),
        Span::styled("  ns:", theme::dim()),
        Span::styled(ns, Style::default().fg(theme::green())),
        Span::styled("  ", theme::dim()),
        Span::styled(
            diagnostic_value(app, &app.cluster.context).into_owned(),
            Style::default().fg(theme::mauve()),
        ),
    ];
    if app.readonly {
        spans.push(Span::styled(" [ro]", Style::default().fg(theme::red())));
    }
    // A flash is transient but can carry errors — surface it inline since the
    // status line is hidden in compact mode.
    if !app.flash.is_empty() {
        let style = if app.flash_err {
            Style::default().fg(theme::red())
        } else {
            Style::default().fg(theme::subtext0())
        };
        spans.push(Span::styled("  — ", theme::dim()));
        spans.push(Span::styled(
            diagnostic_value(app, &app.flash).into_owned(),
            style,
        ));
    }

    let (synced, sync_color) = if app.refresh_task.is_some() {
        ("● refresh", theme::sky())
    } else if app.resource_refresh_available() {
        ("○ stopped", theme::overlay1())
    } else {
        sync_indicator(app.mode, app.doc_filter_return, app.store.synced)
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(10)])
        .split(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), cols[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            synced,
            Style::default().fg(sync_color),
        )))
        .alignment(Alignment::Right),
        cols[1],
    );
}

/// Whether the frame is wide enough for the header's key-hint column:
/// logo (26) + box borders (2) + info cluster + hints.
fn header_hints_fit(frame_width: u16) -> bool {
    frame_width.saturating_sub(26 + 2) >= HEADER_INFO_MIN + HEADER_HINTS_WIDTH
}

/// Show the first effective binding for each action.
fn key_hint(app: &App, scope: &str, actions: &[(Action, &str)]) -> String {
    actions
        .iter()
        .map(|&(action, label)| {
            let key = app.keymap.first_label(scope, action);
            format!("{key}:{label}")
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn hint_line(app: &App, pairs: &[(Action, &str)]) -> Line<'static> {
    let key_style = Style::default()
        .fg(theme::sky())
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    for (&(action, label), cell_width) in pairs.iter().zip(HEADER_HINT_COLUMNS) {
        let key = app.keymap.first_label("table", action);
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        let key = truncate_cols(key, cell_width);
        let remaining = cell_width.saturating_sub(key.width());
        let label = truncate_cols(&format!(" {label}"), remaining);
        let padding = " ".repeat(remaining.saturating_sub(label.width()));
        spans.push(Span::styled(key, key_style));
        spans.push(Span::styled(format!("{label}{padding}"), theme::dim()));
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
            | Mode::Gitops
            | Mode::Argocd
            | Mode::Adjacent
            | Mode::PortForwards
    ) {
        return Vec::new();
    }
    let mut lines = match app.kind_plural.as_str() {
        "pods" => vec![
            hint_line(
                app,
                &[
                    (Action::Open, "containers"),
                    (Action::Logs, "logs"),
                    (Action::PreviousLogs, "prev logs"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::ShellOrScale, "shell"),
                    (Action::ActionMenu, "transfer"),
                    (Action::PortForward, "port-fwd"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Yaml, "yaml"),
                    (Action::Describe, "describe"),
                    (Action::Events, "events"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Edit, "edit"),
                    (Action::Node, "node"),
                    (Action::Owner, "owner"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Explain, "explain"),
                    (Action::Timeline, "timeline"),
                    (Action::Delete, "delete"),
                ],
            ),
        ],
        "deployments" | "statefulsets" => vec![
            hint_line(
                app,
                &[
                    (Action::Open, "pods"),
                    (Action::Logs, "logs"),
                    (Action::Events, "events"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::ShellOrScale, "scale"),
                    (Action::RestartOrRefresh, "restart"),
                    (Action::SetImage, "image"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Yaml, "yaml"),
                    (Action::Describe, "describe"),
                    (Action::Edit, "edit"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Explain, "explain"),
                    (Action::Timeline, "timeline"),
                    (Action::PortForward, "port-fwd"),
                ],
            ),
            hint_line(app, &[(Action::Delete, "delete")]),
        ],
        "daemonsets" => vec![
            hint_line(
                app,
                &[
                    (Action::Open, "pods"),
                    (Action::Logs, "logs"),
                    (Action::Events, "events"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::RestartOrRefresh, "restart"),
                    (Action::SetImage, "image"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Yaml, "yaml"),
                    (Action::Describe, "describe"),
                    (Action::Edit, "edit"),
                ],
            ),
            hint_line(
                app,
                &[(Action::Explain, "explain"), (Action::Delete, "delete")],
            ),
        ],
        "replicasets" | "jobs" => vec![
            hint_line(
                app,
                &[
                    (Action::Open, "pods"),
                    (Action::Logs, "logs"),
                    (Action::Events, "events"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Yaml, "yaml"),
                    (Action::Describe, "describe"),
                    (Action::Edit, "edit"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Explain, "explain"),
                    (Action::Owner, "owner"),
                    (Action::Delete, "delete"),
                ],
            ),
        ],
        "services" => vec![
            hint_line(
                app,
                &[(Action::Open, "pods"), (Action::PortForward, "port-fwd")],
            ),
            hint_line(
                app,
                &[
                    (Action::Yaml, "yaml"),
                    (Action::Describe, "describe"),
                    (Action::Edit, "edit"),
                ],
            ),
            hint_line(
                app,
                &[(Action::CopyCell, "copy cell"), (Action::Delete, "delete")],
            ),
        ],
        "nodes" => vec![
            hint_line(
                app,
                &[
                    (Action::Open, "pods"),
                    (Action::Yaml, "yaml"),
                    (Action::Describe, "describe"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Cordon, "cordon"),
                    (Action::Uncordon, "uncordon"),
                    (Action::Drain, "drain"),
                ],
            ),
            hint_line(app, &[(Action::Delete, "delete")]),
        ],
        "namespaces" => vec![
            hint_line(
                app,
                &[
                    (Action::Open, "switch to"),
                    (Action::Yaml, "yaml"),
                    (Action::Describe, "describe"),
                ],
            ),
            hint_line(app, &[(Action::Edit, "edit"), (Action::Delete, "delete")]),
        ],
        "helm" => vec![
            hint_line(app, &[(Action::Open, "history")]),
            hint_line(
                app,
                &[(Action::Yaml, "yaml"), (Action::Describe, "describe")],
            ),
            hint_line(app, &[(Action::Delete, "uninstall")]),
        ],
        "helmhistory" => vec![
            hint_line(
                app,
                &[
                    (Action::Open, "values"),
                    (Action::RestartOrRefresh, "rollback"),
                ],
            ),
            hint_line(app, &[(Action::Delete, "uninstall")]),
        ],
        "customresourcedefinitions" => vec![
            hint_line(
                app,
                &[
                    (Action::Open, "resources"),
                    (Action::Yaml, "yaml"),
                    (Action::Describe, "describe"),
                ],
            ),
            hint_line(app, &[(Action::Edit, "edit"), (Action::Delete, "delete")]),
        ],
        "secrets" => vec![
            hint_line(
                app,
                &[
                    (Action::Inspect, "decode"),
                    (Action::Yaml, "yaml"),
                    (Action::Describe, "describe"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Edit, "edit"),
                    (Action::Events, "events"),
                    (Action::CopyName, "copy name"),
                ],
            ),
            hint_line(app, &[(Action::Delete, "delete")]),
        ],
        "persistentvolumeclaims" => vec![
            hint_line(
                app,
                &[
                    (Action::Inspect, "browse"),
                    (Action::ShellOrScale, "shell"),
                    (Action::Describe, "describe"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Yaml, "yaml"),
                    (Action::Events, "events"),
                    (Action::CopyName, "copy name"),
                ],
            ),
            hint_line(app, &[(Action::Delete, "delete")]),
        ],
        _ => vec![
            hint_line(
                app,
                &[
                    (Action::Open, "yaml"),
                    (Action::Describe, "describe"),
                    (Action::Events, "events"),
                ],
            ),
            hint_line(
                app,
                &[
                    (Action::Edit, "edit"),
                    (Action::CopyName, "copy name"),
                    (Action::CopyCell, "copy cell"),
                ],
            ),
            hint_line(app, &[(Action::Delete, "delete")]),
        ],
    };
    if app.kind_plural == "machinedeployments"
        && app
            .kind
            .as_ref()
            .is_some_and(|k| k.ar.group == "cluster.x-k8s.io")
    {
        lines = vec![
            hint_line(
                app,
                &[
                    (Action::Open, "machines"),
                    (Action::Yaml, "yaml"),
                    (Action::Describe, "describe"),
                ],
            ),
            hint_line(app, &[(Action::Edit, "edit"), (Action::Delete, "delete")]),
        ];
    }
    if app.kind.as_ref().is_some_and(|kind| kind.scalable)
        && !matches!(
            app.kind_plural.as_str(),
            "deployments" | "statefulsets" | "pods" | "persistentvolumeclaims"
        )
    {
        lines.insert(1, hint_line(app, &[(Action::ShellOrScale, "scale")]));
    }
    if app.flux_suspendable() {
        lines.push(hint_line(app, &[(Action::ActionMenu, "flux menu")]));
    }
    if app.argocd_kind() {
        lines.push(hint_line(app, &[(Action::ActionMenu, "suspend/sync")]));
    }
    if app.kind_plural == "helmreleases" {
        lines.push(hint_line(app, &[(Action::Open, "helm history")]));
    }
    if app.cronjob_kind() {
        lines.push(hint_line(app, &[(Action::ActionMenu, "trigger/suspend")]));
    }
    if app.external_secret_kind() {
        lines.push(hint_line(app, &[(Action::RestartOrRefresh, "force-sync")]));
    }
    // The header box has 5 inner rows.
    lines.truncate(5);
    lines
}

fn draw_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let show_ns = app.show_namespace_column();
    let headers = app.display_headers();
    let sort_col = app.sort_column;
    let sort_arrow = if app.sort_desc { " ↓" } else { " ↑" };
    // The namespace column is added before the view columns.
    let ns_off = usize::from(show_ns);
    let name_col = (0..headers.len())
        .find(|i| {
            i.checked_sub(ns_off)
                .and_then(|si| app.view_spec().canonical_header(si))
                == Some("NAME")
        })
        .unwrap_or(ns_off);
    // Per-column custom alignment, precomputed so cells don't re-borrow app.
    let aligns: Vec<Option<Alignment>> = (0..headers.len())
        .map(|i| {
            i.checked_sub(ns_off)
                .and_then(|si| app.view_spec().align_at(si))
                .map(cell_alignment)
        })
        .collect();
    let align_of = |i: usize| aligns.get(i).copied().flatten();

    let header_cells = headers
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
                RenderCell::from(line)
            } else {
                match align_of(i) {
                    Some(a) => RenderCell::from(Text::from(h.clone()).alignment(a)),
                    None => RenderCell::from(h.clone()),
                }
            }
        })
        .collect::<Vec<_>>();

    // Column indices (fixed for the whole table) for the columns that get
    // their own visibility treatment below, computed once rather than
    // string-compared per cell.
    let age_idx = (0..headers.len()).find(|i| {
        i.checked_sub(ns_off)
            .and_then(|si| app.view_spec().canonical_header(si))
            == Some("AGE")
    });
    let ready_idx = (0..headers.len()).find(|i| {
        i.checked_sub(ns_off)
            .and_then(|si| app.view_spec().canonical_header(si))
            == Some("READY")
    });
    let restarts_idx = (0..headers.len()).find(|i| {
        i.checked_sub(ns_off)
            .and_then(|si| app.view_spec().canonical_header(si))
            == Some("RESTARTS")
    });
    let metric_columns: Vec<_> = (0..headers.len())
        .map(|i| {
            i.checked_sub(ns_off)
                .and_then(|si| app.view_spec().metric_at(si))
        })
        .collect();

    let count = app.row_count();
    let visible_rows = area.height.saturating_sub(3).max(1) as usize;
    app.table_page_rows = visible_rows;
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

    let mut needed = app.table_column_widths();
    if let Some(i) = sort_col {
        needed[i] = needed[i].max(cell_width(&headers[i]).saturating_add(2));
    }
    let status_width = if app.kind_plural == "nodes" { 27 } else { 26 };
    // Reserve space for the "● " port-forward marker on the NAME column when
    // any live forward matches the current context+cluster.
    if !app.port_forwards.is_empty()
        && let Some(n) = needed.get_mut(name_col)
    {
        *n = n.saturating_add(2);
    }
    // Compute widths from all columns before applying the viewport offset.
    let col_rules: Vec<(ColWidth, u16)> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            // A custom column's configured width wins over the curated rules.
            let rule = if let Some(w) = i
                .checked_sub(ns_off)
                .and_then(|si| app.view_spec().width_at(si))
            {
                ColWidth::Exact(w)
            } else if let Some(metric) = metric_columns[i] {
                ColWidth::Exact(match metric {
                    columns::MetricColumn::NodePods
                    | columns::MetricColumn::NodeCpuUtilization
                    | columns::MetricColumn::NodeMemoryUtilization => 5,
                    _ if metric.percentage() => 7,
                    _ => 8,
                })
            } else {
                match h.as_str() {
                    // NAME is the column you actually read — its weight takes
                    // most of a wide window's surplus, and most of the shared
                    // space when the window can't fit everything.
                    "NAME" => ColWidth::Flex(6),
                    "NAMESPACE" => ColWidth::Flex(2),
                    "NODE" | "CLAIM" | "VOLUME" | "HOSTS" => ColWidth::Flex(1),
                    "AGE" => ColWidth::Cap(7),
                    // Volatile numerics keep a fixed width so a metrics tick
                    // never reflows the whole table.
                    "CPU" | "MEM" => ColWidth::Exact(8),
                    "%CPU" | "%MEM" => ColWidth::Exact(5),
                    "PODS" => ColWidth::Exact(5),
                    // Keep status changes from moving the other columns. Nodes
                    // need one extra cell for NotReady,SchedulingDisabled.
                    "STATUS" => ColWidth::Exact(status_width),
                    "READY" | "RESTARTS" => ColWidth::Cap(10),
                    // CRD view: group domains run long (e.g.
                    // "kustomize.toolkit.fluxcd.io"), so GROUP/KIND/VERSIONS
                    // get generous ceilings NAME's weight can't crush.
                    "GROUP" => ColWidth::Cap(30),
                    "KIND" | "VERSIONS" => ColWidth::Cap(20),
                    "SCOPE" => ColWidth::Cap(12),
                    // Flux views: the Ready condition message and git/chart
                    // revision are the columns you read — they split the
                    // leftover space with NAME.
                    "MESSAGE" => ColWidth::Flex(4),
                    "REVISION" => ColWidth::Flex(2),
                    "SUSPENDED" => ColWidth::Cap(9),
                    _ => ColWidth::Flex(1),
                }
            };
            (rule, needed[i])
        })
        .collect();
    // Mirror the Table widget's fixed overhead: borders, the always-reserved
    // 2-cell highlight symbol, and the 2-cell spacing between columns.
    let ncols = col_rules.len() as u16;
    let content_budget = area
        .width
        .saturating_sub(2)
        .saturating_sub(2)
        .saturating_sub(2 * ncols.saturating_sub(1));
    let mut widths = distribute_column_widths(content_budget, &col_rules);
    for (i, (rule, needed)) in col_rules.iter().enumerate() {
        if matches!(rule, ColWidth::Flex(_)) {
            let minimum = if i <= name_col {
                (*needed).min(area.width.saturating_sub(4) / (2 * (name_col + 1) as u16))
            } else {
                *needed
            };
            widths[i] = widths[i].max(minimum);
        }
    }
    let inner = area.inner(ratatui::layout::Margin::new(1, 1));
    let viewport = TableViewport::new(&widths, name_col + 1, inner.width);
    app.col_scroll_max = viewport.max_offset;
    app.col_offset = app.col_offset.min(app.col_scroll_max);
    let col_offset = app.col_offset;

    let visible_objects = app.rows_window(offset, visible_rows);
    let spec = app.view_spec();
    let thresholds = app.resolved_thresholds();
    // One clock reading for the whole frame. Every visible AGE/DURATION cell
    // used to call `Timestamp::now()` for itself, so a full table took one
    // reading per volatile cell and could show two rows a second apart.
    let now = crate::columns::now_secs();
    app.ensure_table_cell_cache_at(&visible_objects, now);
    let cell_cache = app.table_cell_cache();

    let rows: Vec<Vec<RenderCell>> = visible_objects
        .iter()
        .map(|obj| {
            let row_key = crate::store::row_key(obj);
            let marked_row = !app.marked.is_empty() && app.marked.contains(&row_key);
            let pf_ns = obj.metadata.namespace.as_deref().unwrap_or_default();
            let pf_name = obj.metadata.name.as_deref().unwrap_or_default();
            let forwarded = app.has_port_forward(pf_ns, pf_name, &app.kind_plural);
            let (base_cells, status_idx) = cell_cache
                .get(&row_key)
                .expect("visible rows are warmed in the table cell cache");
            let helm_updated = cell_cache.helm_updated(&row_key);
            let mut style_idx = status_idx;
            let mut cells = Vec::with_capacity(headers.len());
            if show_ns {
                cells.push(TableCellText::Borrowed(
                    obj.metadata.namespace.as_deref().unwrap_or_default(),
                ));
                style_idx = status_idx.map(|i| i + 1);
            }
            for (i, cell) in base_cells.iter().enumerate() {
                if let Some(value) = app
                    .live_cell(obj, i)
                    .or_else(|| spec.volatile_cached(obj, &app.kind_plural, i, now, helm_updated))
                {
                    cells.push(TableCellText::Owned(value));
                } else {
                    cells.push(TableCellText::Borrowed(cell.as_str()));
                }
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
                && (ready_idx
                    .and_then(|i| cells.get(i))
                    .is_some_and(|r| !all_ready(r.as_str()))
                    || (app.kind_plural == "pods" && pod_readiness_blocked(obj)));
            let status_key = if running_not_ready {
                "PodInitializing"
            } else {
                status_val
            };
            let row_color = theme::row_color(status_key);
            let status_badge = theme::status_color(status_key);
            let render_cells: Vec<RenderCell> = cells
                .into_iter()
                .enumerate()
                .map(|(i, c)| {
                    let align = align_of(i);
                    if marked_row {
                        if i == name_col {
                            render_name_cell(app, c.as_str(), theme::mark(), forwarded).style(
                                Style::default()
                                    .fg(theme::mark())
                                    .add_modifier(Modifier::BOLD),
                            )
                        } else {
                            c.into_cell_aligned(align).style(
                                Style::default()
                                    .fg(theme::mark())
                                    .add_modifier(Modifier::BOLD),
                            )
                        }
                    } else if Some(i) == style_idx {
                        c.into_cell_aligned(align)
                            .style(Style::default().fg(status_badge))
                    } else if i == name_col {
                        render_name_cell(app, c.as_str(), row_color, forwarded)
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
                    } else if let Some(metric) = metric_columns[i] {
                        let value = app.metric_value(obj, metric);
                        let color = if metric.percentage() {
                            util_color(value, thresholds.utilization)
                        } else if metric == columns::MetricColumn::NodePods {
                            row_color
                        } else {
                            let band = if metric.cpu() {
                                thresholds.cpu
                            } else {
                                thresholds.memory
                            };
                            value
                                .and_then(|v| band.severity(v))
                                .map(theme::severity_fg)
                                .unwrap_or(row_color)
                        };
                        c.into_cell_aligned(align).style(Style::default().fg(color))
                    } else {
                        c.into_cell_aligned(align)
                            .style(Style::default().fg(row_color))
                    }
                })
                .collect();
            render_cells
        })
        .collect();

    let kind_label = app.list_title();
    // k9s title: resource name (teal, bold) then a yellow [count].
    let mut title = vec![
        Span::styled(format!(" {kind_label} "), theme::title()),
        Span::styled(format!("[{count}]"), Style::default().fg(theme::counter())),
    ];
    if app.faults_filter_active() {
        title.push(Span::styled(" [faults]", Style::default().fg(theme::red())));
    }
    if col_offset > 0 {
        title.push(Span::styled(" ←", theme::dim()));
    }
    if col_offset < app.col_scroll_max {
        title.push(Span::styled(" →", theme::dim()));
    }
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
        title.push(Span::styled(app.filter_location(), theme::dim()));
    }
    title.push(Span::raw(" "));

    let render_selected = selected
        .filter(|_| count > 0)
        .map(|i| i.saturating_sub(offset));
    app.record_table_hit(
        inner.y,
        inner.y.saturating_add(1),
        inner.height.saturating_sub(1),
        inner.x,
        inner.x.saturating_add(inner.width),
        viewport
            .ranges(col_offset)
            .iter()
            .map(|&(start, end, i, _)| (inner.x + start, inner.x + end, i))
            .collect(),
    );
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border_focused())
            .title(Line::from(title)),
        area,
    );
    frame.render_widget(
        ScrollingTable {
            header: header_cells,
            rows,
            widths,
            viewport,
            offset: col_offset,
            selected: render_selected,
        },
        inner,
    );
    draw_border_scrollbar(
        frame,
        show_scrollbars,
        Rect {
            y: area.y.saturating_add(1),
            height: area.height.saturating_sub(1),
            ..area
        },
        offset,
        count.saturating_sub(visible_rows),
        visible_rows,
        false,
    );
    draw_border_scrollbar(
        frame,
        show_scrollbars,
        area,
        col_offset,
        app.col_scroll_max,
        usize::from(inner.width),
        true,
    );
}

/// Positions in the full table, measured from the selection marker.
struct TableViewport {
    columns: Vec<(usize, usize)>,
    anchored: usize,
    frozen_width: usize,
    width: usize,
    max_offset: usize,
}

impl TableViewport {
    fn new(widths: &[u16], anchored: usize, width: u16) -> Self {
        let mut end = 2usize;
        let columns: Vec<_> = widths
            .iter()
            .map(|&width| {
                let start = end;
                end += usize::from(width);
                let range = (start, end);
                end += 2;
                range
            })
            .collect();
        let frozen_width = columns.get(anchored).map_or(end.saturating_sub(2), |c| c.0);
        let width = usize::from(width);
        let max_offset = if frozen_width < width {
            end.saturating_sub(2).saturating_sub(width)
        } else {
            0
        };
        Self {
            columns,
            anchored,
            frozen_width,
            width,
            max_offset,
        }
    }

    /// Visible ranges plus the source offset within each cell.
    fn ranges(&self, offset: usize) -> Vec<(u16, u16, usize, u16)> {
        self.columns
            .iter()
            .enumerate()
            .filter_map(|(i, &(start, end))| {
                let (left, right, shift) = if i < self.anchored {
                    (0, self.width, 0)
                } else {
                    (self.frozen_width + offset, self.width + offset, offset)
                };
                let visible_start = start.max(left);
                let visible_end = end.min(right);
                (visible_start < visible_end).then_some((
                    visible_start.saturating_sub(shift) as u16,
                    visible_end.saturating_sub(shift) as u16,
                    i,
                    visible_start.saturating_sub(start) as u16,
                ))
            })
            .collect()
    }
}

struct ScrollingTable<'a> {
    header: Vec<RenderCell<'a>>,
    rows: Vec<Vec<RenderCell<'a>>>,
    widths: Vec<u16>,
    viewport: TableViewport,
    offset: usize,
    selected: Option<usize>,
}

impl ratatui::widgets::Widget for ScrollingTable<'_> {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let ranges = self.viewport.ranges(self.offset);
        let max_width = ranges
            .iter()
            .map(|&(_, _, i, _)| self.widths[i])
            .max()
            .unwrap_or(0);
        // Reuse one row of storage. Long values do not allocate a full table.
        let mut source = ratatui::buffer::Buffer::empty(Rect::new(0, 0, max_width, 1));
        for (y, cells) in std::iter::once(self.header)
            .chain(self.rows)
            .take(usize::from(area.height))
            .enumerate()
        {
            let y_pos = area.y + y as u16;
            let selected = y > 0 && self.selected == Some(y - 1);
            let style = if y == 0 {
                theme::header_row()
            } else if selected {
                theme::selected_row()
            } else {
                Style::default()
            };
            buf.set_style(Rect::new(area.x, y_pos, area.width, 1), style);
            if selected {
                buf.set_stringn(area.x, y_pos, "▌ ", usize::from(area.width), style);
            }
            for &(start, end, index, source_start) in &ranges {
                source.reset();
                if let Some(bg) = theme::background() {
                    source.set_style(source.area, Style::default().bg(bg));
                }
                let cell_area = Rect::new(0, 0, self.widths[index], 1);
                cells[index].render(cell_area, &mut source, style, selected);
                let source_end = source_start + (end - start);
                let mut x = 0;
                while x < source_end {
                    let cell = &source[(x, 0)];
                    let symbol_width = cell.symbol().width().max(1) as u16;
                    if x >= source_start && x.saturating_add(symbol_width) <= source_end {
                        let target = &mut buf[(area.x + start + (x - source_start), y_pos)];
                        target.set_symbol(cell.symbol());
                        target.set_style(cell.style());
                    }
                    x = x.saturating_add(symbol_width);
                }
            }
        }
    }
}

/// How a table column's width is decided when splitting the frame (#166).
enum ColWidth {
    /// User-configured width, honored exactly.
    Exact(u16),
    /// Sized to the widest visible value, never above the cap.
    Cap(u16),
    /// Sized to the widest visible value when it fits; surplus and deficit
    /// are shared between Flex columns proportionally to the weight.
    Flex(u16),
}

/// Display width of a table cell in terminal columns.
fn cell_width(s: &str) -> u16 {
    u16::try_from(s.width()).unwrap_or(u16::MAX)
}

/// Split `budget` cells across columns. Exact/Cap columns take their width
/// first; each Flex column then gets its full content width whenever its
/// weight-share covers it (a waterfall, so a short NAME frees space for a
/// long EXTERNAL-IP), and the final surplus or deficit is shared by weight.
/// Padding is always trimmed before data.
fn distribute_column_widths(budget: u16, cols: &[(ColWidth, u16)]) -> Vec<u16> {
    let mut widths: Vec<u16> = cols
        .iter()
        .map(|(rule, needed)| match rule {
            ColWidth::Exact(w) => *w,
            ColWidth::Cap(cap) => (*needed).min(*cap),
            ColWidth::Flex(_) => 0,
        })
        .collect();
    let fixed: u32 = widths.iter().map(|&w| u32::from(w)).sum();
    let mut left = u32::from(budget).saturating_sub(fixed);

    let flex: Vec<(usize, u32, u32)> = cols
        .iter()
        .enumerate()
        .filter_map(|(i, (rule, needed))| match rule {
            ColWidth::Flex(w) => Some((i, u32::from(*w), u32::from(*needed))),
            _ => None,
        })
        .collect();

    // Waterfall: grant the full content width to any column whose weight-share
    // covers it, then let the freed remainder raise the others' shares.
    let mut unsat = flex.clone();
    loop {
        let total: u32 = unsat.iter().map(|&(_, w, _)| w).sum();
        if total == 0 {
            break;
        }
        let Some(p) = unsat
            .iter()
            .position(|&(_, w, need)| left * w / total >= need)
        else {
            break;
        };
        let (i, _, need) = unsat.swap_remove(p);
        widths[i] = need as u16;
        left -= need;
    }

    if unsat.is_empty() {
        // Everyone fits: spread the surplus by weight so NAME still takes the
        // lion's share of a wide window.
        share_by_weight(left, &flex, &mut widths);
    } else {
        // Deficit: the columns that can't be satisfied split what's left by
        // weight — exactly the old Fill behavior, but only once padding is
        // already gone.
        share_by_weight(left, &unsat, &mut widths);
    }
    widths
}

/// Add `left` extra cells to `widths` proportionally to each column's weight,
/// handing out the integer-division remainder one cell at a time.
fn share_by_weight(mut left: u32, cols: &[(usize, u32, u32)], widths: &mut [u16]) {
    let total: u32 = cols.iter().map(|&(_, w, _)| w).sum();
    if total == 0 {
        return;
    }
    let budget = left;
    for &(i, w, _) in cols {
        let share = budget * w / total;
        widths[i] = widths[i].saturating_add(share as u16);
        left -= share;
    }
    // remainder < cols.len(), so a single pass hands it all out.
    for &(i, _, _) in cols {
        if left == 0 {
            break;
        }
        widths[i] = widths[i].saturating_add(1);
        left -= 1;
    }
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

fn pod_readiness_blocked(obj: &kube::core::DynamicObject) -> bool {
    let conditions = obj
        .data
        .pointer("/status/conditions")
        .and_then(serde_json::Value::as_array);
    let condition = |name: &str| {
        conditions
            .and_then(|conditions| conditions.iter().find(|c| c["type"].as_str() == Some(name)))
    };
    condition("Ready").is_some_and(|c| c["status"].as_str() != Some("True"))
        || obj
            .data
            .pointer("/spec/readinessGates")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|gates| {
                gates.iter().any(|gate| {
                    gate["conditionType"]
                        .as_str()
                        .and_then(condition)
                        .is_none_or(|c| c["status"].as_str() != Some("True"))
                })
            })
}

/// Render the NAME cell, highlighting characters that matched the active
/// fuzzy row filter (bold yellow) so a scan across many filtered results is
/// faster — every visible row already matched, this just shows *where*.
/// Falls back to a flat `base`-colored cell when there's no active filter.
fn render_name_cell(app: &App, name: &str, base: Color, forwarded: bool) -> RenderCell<'static> {
    // A teal ● prepended when a port-forward is active for this row.
    let marker = if forwarded {
        vec![Span::styled("● ", Style::default().fg(theme::teal()))]
    } else {
        Vec::new()
    };
    let Some(matched) = app.filter_match_indices(name).filter(|idx| !idx.is_empty()) else {
        let mut spans = marker;
        spans.push(Span::styled(name.to_string(), Style::default().fg(base)));
        return RenderCell::from(Line::from(spans));
    };
    let matched: std::collections::HashSet<usize> = matched.iter().copied().collect();
    let plain = Style::default().fg(base);
    let hl = Style::default()
        .fg(theme::yellow())
        .add_modifier(Modifier::BOLD);

    let mut spans = marker;
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
    RenderCell::from(Line::from(spans))
}

fn draw_scrollable(
    frame: &mut Frame,
    show_scrollbars: bool,
    fullscreen: bool,
    view: &mut crate::app::Scrollable,
    area: Rect,
    accent: ratatui::style::Color,
) {
    let inner_w = area.width.saturating_sub(if fullscreen { 0 } else { 2 }) as usize;
    let inner_h = area.height.saturating_sub(if fullscreen { 1 } else { 2 }) as usize;
    view.set_viewport(inner_w, inner_h);
    let (start, end, row_offset) = view.visible_source_window();
    let text: Vec<Line> = view
        .lines
        .iter()
        .skip(start)
        .take(end - start)
        .map(|l| {
            let line = strip_ansi_if_present(l);
            highlight_matches(Line::from(highlight_yaml(&line)), &view.filter)
        })
        .collect();
    let text = if view.wrap {
        visible_wrapped_rows(text, inner_w, row_offset, inner_h)
    } else {
        text
    };
    let block = Block::default()
        .borders(if fullscreen {
            Borders::NONE
        } else {
            Borders::ALL
        })
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Span::styled(doc_title(view), theme::title()));
    let p = Paragraph::new(text).block(block);
    // Wrapped lines are already sliced to the exact visible display rows;
    // otherwise honor the horizontal offset for content past the right edge.
    let p = if view.wrap {
        p
    } else {
        p.scroll((0, view.hscroll.min(u16::MAX as usize) as u16))
    };
    frame.render_widget(p, area);
    if !fullscreen {
        draw_document_scrollbars(frame, show_scrollbars, view, area);
    }
}

fn visible_wrapped_rows(
    lines: Vec<Line<'static>>,
    width: usize,
    row_offset: usize,
    height: usize,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .flat_map(|line| wrap_line(line, width))
        .skip(row_offset)
        .take(height)
        .collect()
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
    let show_scrollbars = app.scrollbars_visible();
    // Fullscreen drops the borders (side glyphs would end up in every
    // terminal-selection copy); the title still takes the top row.
    let fullscreen = app.logs.fullscreen;
    let (inner_w, inner_h) = if fullscreen {
        (
            area.width.max(1) as usize,
            area.height.saturating_sub(1) as usize,
        )
    } else {
        (
            area.width.saturating_sub(2).max(1) as usize,
            area.height.saturating_sub(2) as usize,
        )
    };

    let filter = app.logs.filter.clone();
    let active = !filter.is_empty();
    let bad_regex = app.logs.matcher.is_error();
    // Highlight matches only for a plain substring filter — not inverse (`!…`,
    // which hides matches) or regex (`/…/`, whose spans we don't track).
    let is_plain = active
        && !filter.starts_with('!')
        && !(filter.len() >= 2 && filter.starts_with('/') && filter.ends_with('/'));
    let highlight = if is_plain { filter.as_str() } else { "" };

    // Which lines pass the filter, and where each starts in display rows.
    // Maintained incrementally across frames (see `LogIndex`) rather than
    // rebuilt: the buffer runs to 100k lines while paused, and the viewport
    // shows ~40.
    let wrap = app.logs.wrap;
    let wrap_width = if wrap { inner_w } else { 0 };
    let total_rows = app.logs.refresh_index(wrap_width).total_rows();

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
    // The first one is found by binary search over the index's cumulative row
    // ends, not by walking the buffer from the top.
    let mut rows: Vec<Line> = Vec::with_capacity(inner_h);
    {
        let index = app.logs.index();
        let first = index.first_at_row(scroll);
        for i in first..index.shown_len() {
            let row = index.start_row(i);
            if row >= scroll + inner_h {
                break;
            }
            let Some(buf_idx) = index.line_at(i) else {
                rows.push(Line::styled("-".repeat(inner_w), theme::dim()));
                continue;
            };
            let Some(l) = app.logs.view.lines.get(buf_idx) else {
                break;
            };
            let line = render_log_line(l, highlight);
            if wrap {
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
        }
    }

    let flags = format!(
        "{}{}{}{}{}",
        if app.logs.warnings_only {
            " [warn/error]"
        } else {
            ""
        },
        if app.logs.stopped {
            " ⏹stopped"
        } else if app.logs.follow {
            " ▶follow"
        } else {
            " ⏸paused"
        },
        if app.logs.wrap { " wrap" } else { "" },
        if app.logs.timestamps { " ts" } else { "" },
        // Provider views manage the window in their own title suffix.
        match app.logs.anchor_label() {
            Some(l) if !app.provider_logs_active() => format!(" ⏱{l}"),
            _ => String::new(),
        },
    );
    let title = if bad_regex {
        format!(
            " {} · /{} [invalid regex]{} ",
            app.logs.view.title, filter, flags
        )
    } else if active {
        format!(
            " {} · /{} [{}]{} ",
            app.logs.view.title,
            filter,
            app.logs.index().matched_lines(),
            flags
        )
    } else {
        format!(" {}{} ", app.logs.view.title, flags)
    };

    // The rows are already the exact viewport slice — no Paragraph scroll or
    // wrap, so ratatui can't re-lay-out (and disagree with) the math above.
    let block = if fullscreen {
        // Borderless: `Block::inner` still reserves one row for the top title,
        // matching the fullscreen `inner_h` above.
        Block::default().title(Span::styled(title, theme::title()))
    } else {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::green()))
            .title(Span::styled(title, theme::title()))
    };
    frame.render_widget(Paragraph::new(rows).block(block), area);
    if !fullscreen {
        draw_border_scrollbar(
            frame,
            show_scrollbars,
            area,
            scroll,
            max_scroll,
            inner_h,
            false,
        );
    }
}

/// Display rows `raw` occupies when char-wrapped to `width` columns: ANSI
/// escapes are zero-width (they're stripped at render time) and East-Asian
/// wide glyphs take two columns. Must stay the exact greedy fill
/// [`wrap_line`] performs — the scroll math depends on them agreeing.
pub(crate) fn wrapped_height(raw: &str, width: usize) -> usize {
    let width = width.max(1);
    // Fast path: printable ASCII wraps at exactly `width` bytes. Control
    // characters stay on the general path because ratatui assigns them no
    // display width; counting a tab as one byte would drift from `wrap_line`.
    if raw.is_ascii() && !raw.bytes().any(|b| b.is_ascii_control()) {
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
    let base = log_level_color(line);
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
    // One palette snapshot, not ten accessor calls: this runs per prefixed
    // log line, and nine of the ten swatches are discarded every time.
    let p = theme::snapshot();
    let palette: [Color; 10] = [
        p.mauve,
        p.blue,
        p.green,
        p.teal,
        p.pink,
        p.sapphire,
        p.lavender,
        p.flamingo,
        p.sky,
        p.rosewater,
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

pub(crate) fn strip_ansi_if_present(s: &str) -> std::borrow::Cow<'_, str> {
    if memchr::memchr(0x1b, s.as_bytes()).is_some() {
        std::borrow::Cow::Owned(strip_ansi(s))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
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

fn log_level_color(line: &str) -> Color {
    match crate::logfilter::severity(line) {
        crate::logfilter::Severity::Error => theme::red(),
        crate::logfilter::Severity::Warning => theme::peach(),
        crate::logfilter::Severity::Debug => theme::overlay1(),
        crate::logfilter::Severity::Other => theme::text(),
    }
}

/// Unified-diff view with +/- line coloring.
fn draw_diff(
    frame: &mut Frame,
    show_scrollbars: bool,
    fullscreen: bool,
    view: &mut crate::app::Scrollable,
    area: Rect,
) {
    let inner_w = area.width.saturating_sub(if fullscreen { 0 } else { 2 }) as usize;
    let inner_h = area.height.saturating_sub(if fullscreen { 1 } else { 2 }) as usize;
    view.set_viewport(inner_w, inner_h);
    let (start, end, row_offset) = view.visible_source_window();
    let lines: Vec<Line> = view
        .lines
        .iter()
        .skip(start)
        .take(end - start)
        .map(|l| {
            let line = strip_ansi_if_present(l);
            let color = match line.chars().next() {
                Some('+') => theme::green(),
                Some('-') => theme::red(),
                _ => theme::overlay1(),
            };
            let line = Line::from(Span::styled(line.into_owned(), Style::default().fg(color)));
            highlight_matches(line, &view.filter)
        })
        .collect();
    let lines = if view.wrap {
        visible_wrapped_rows(lines, inner_w, row_offset, inner_h)
    } else {
        lines
    };
    let block = Block::default()
        .borders(if fullscreen {
            Borders::NONE
        } else {
            Borders::ALL
        })
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::peach()))
        .title(Span::styled(doc_title(view), theme::title()));
    let p = Paragraph::new(lines).block(block);
    let p = if view.wrap {
        p
    } else {
        p.scroll((0, view.hscroll.min(u16::MAX as usize) as u16))
    };
    frame.render_widget(p, area);
    if !fullscreen {
        draw_document_scrollbars(frame, show_scrollbars, view, area);
    }
}

/// Doc-view title, extended with the active search query and the current
/// match position (` title · /query [2/5] `, or `[no matches]`), vim-style.
fn doc_title(view: &crate::app::Scrollable) -> String {
    if view.filter.is_empty() {
        return format!(" {} ", view.title);
    }
    let matches = view.match_lines();
    if matches.is_empty() {
        format!(" {} · /{} [no matches] ", view.title, view.filter)
    } else {
        let cur = view.match_idx.min(matches.len() - 1) + 1;
        format!(
            " {} · /{} [{}/{}] ",
            view.title,
            view.filter,
            cur,
            matches.len()
        )
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

fn wrap_help_span(span: Span<'static>, width: usize, needle: &str) -> Vec<Line<'static>> {
    let highlighted = highlight_matches(Line::from(span.clone()), needle);
    let render = |start: usize, end: usize| {
        let mut offset = 0;
        let mut spans = Vec::new();
        for part in &highlighted.spans {
            let part_end = offset + part.content.len();
            let from = start.max(offset);
            let to = end.min(part_end);
            if from < to {
                spans.push(Span::styled(
                    part.content[from - offset..to - offset].to_string(),
                    part.style,
                ));
            }
            offset = part_end;
        }
        wrap_line(Line::from(spans), width)
    };
    let mut rows = Vec::new();
    let mut start = 0;
    let mut end = 0;
    for word in span.content.split_whitespace() {
        let word_start = end + span.content[end..].find(word).unwrap();
        let word_end = word_start + word.len();
        if start < end && span.content[start..word_end].width() > width {
            rows.extend(render(start, end));
            start = word_start;
        }
        end = word_end;
    }
    rows.extend(render(start, end));
    rows
}

fn build_help(app: &App, width: usize) -> (Vec<Line<'static>>, String) {
    let bind = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(k.to_string(), Style::default().fg(theme::yellow())),
            Span::styled(d.to_string(), theme::dim()),
        ])
    };
    let mut lines = Vec::new();
    let mut previous_scope = "";
    for (scope, action, _) in app.keymap.entries() {
        let owned;
        if scope != previous_scope {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  Keys: {scope}"),
                theme::title(),
            )));
            previous_scope = scope;
        }
        let description = if scope == "table" && action == Action::Drain {
            "open node drain options for the current or marked nodes"
        } else if scope == "drain" {
            match action {
                Action::Accept => "review options; close a completed drain",
                Action::Back | Action::Quit => {
                    "cancel the form or active drain; accepted requests cannot be reversed"
                }
                Action::Toggle => "toggle the selected drain option",
                Action::Down => "select the next drain option",
                Action::Up => "select the previous drain option",
                _ => action.description(),
            }
        } else if scope == "table" && action == Action::Logs {
            "logs (marked pods, or current row)"
        } else if scope == "port_forward_picker" && action == Action::Edit {
            "edit local port of the selected mapping"
        } else if action == Action::LogMarker {
            "add visual marker at the log tail (excluded from copy/save)"
        } else if action == Action::Fullscreen {
            "toggle fullscreen for text selection (no borders or scrollbars)"
        } else if action == Action::AutoRefresh && scope == "detail" {
            "toggle refresh (YAML, decoded Secret, describe)"
        } else if action == Action::AutoRefresh && scope == "diff" {
            "toggle refresh (keep the comparison baseline)"
        } else if action == Action::Filter && scope == "table" {
            "filter rows; label:text searches label keys and values locally"
        } else if scope == "table"
            && (action == Action::AllNamespaces || Action::FAVORITE_NAMESPACES.contains(&action))
        {
            owned = format!(
                "{}; also in the namespace switcher while the filter is empty",
                action.description()
            );
            owned.as_str()
        } else {
            action.description()
        };
        lines.push(bind(app.keymap.label(scope, action), description));
    }
    lines.push(Line::from(
        "  Range selection keeps separate marks; other keys end the range.",
    ));
    lines.push(Line::from(Span::styled(
        "  Commands (enter in the command palette)",
        theme::title(),
    )));
    lines.push(bind(
        ":<resource>",
        "global command palette - fuzzy over kinds + commands",
    ));
    lines.push(bind(
        ":<res> <ns>",
        "switch kind and namespace at once (all/* = all namespaces)",
    ));
    lines.push(bind(
        ":ns <name>",
        "change namespace, keep resource (all/* = all); from Namespaces, return or open Pods",
    ));
    lines.push(bind(
        ":ctx · :pulse",
        "switch context (launch: sofka ctx) · cluster-health dashboard",
    ));
    lines.push(bind(
        ":fleet",
        "cross-context health dashboard for configured contexts",
    ));
    lines.push(bind(
        ":xray · :diff",
        "hierarchical tree · live-vs-last-applied diff",
    ));
    lines.push(bind(":events", "browse all events"));
    lines.push(bind(":pf", "view/stop background port-forwards"));
    lines.push(bind(":skin", "switch color skin live"));
    lines.push(bind(
        ":reload · :config · :info",
        "reload config · config sources + warnings · runtime diagnostics",
    ));
    lines.push(bind(
        ":can-i",
        "what you can do here · :can-i <verb> <resource> [ns] checks one action",
    ));
    lines.push(bind(
        ":resource -n ns --context ctx /filter",
        "query resource, namespace, context and filter together",
    ));
    lines.push(bind(
        ":resource @context [namespace]",
        "switch context and resource (select to fill context, then add namespace)",
    ));
    lines.push(bind(":rightsize", "historical right-sizing: P50/P95/P99 usage → suggested requests + patch (needs [providers.metrics])"));
    lines.push(bind(
        ":gitops · :flux",
        "Flux owner, source, revisions & reconciliation chain",
    ));
    lines.push(bind(
        ":argocd · :argo",
        "Argo CD Application sync/health, source, managed resources & what's blocking",
    ));
    lines.push(bind(
        ":journal · :audit",
        "session-local log of the mutating actions you've taken",
    ));
    lines.push(bind(
        ":debug",
        "pod: ephemeral debug container · node: privileged debug pod",
    ));
    lines.push(bind(
        ":debug-clean",
        "delete the node debugger pods launched this session",
    ));
    lines.push(bind(
        ":bundle · :bundle-save",
        "assemble a redacted diagnostic bundle for the selection · write it to a file",
    ));
    lines.push(bind(
        ":snapshot [fmt] · :snapshots",
        "capture the current view (text/json/yaml) · browse saved snapshots",
    ));
    lines.push(bind(
        ":pvc-clean",
        "delete helper pods left behind by a session that exited uncleanly (:pvc-cleanup)",
    ));
    lines.push(bind(
        ":plugin-cancel",
        "cancel the active plugin and its temporary forward",
    ));
    // Config-defined plugins, with their (possibly modified) key chords.
    if !app.plugins.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  Plugins", theme::title())));
        for p in &app.plugins {
            let mut bindings = Vec::new();
            if !p.key.is_empty() {
                bindings.push(
                    crate::keys::KeyChord::parse(&p.key)
                        .map(|c| c.label())
                        .unwrap_or_else(|_| format!("{}?", p.key)),
                );
            }
            if let Some(command) = &p.palette {
                bindings.push(format!(":{command}"));
            }
            let key = bindings.join(" / ");
            let scope = if p.scopes.is_empty() {
                "all resources".to_string()
            } else {
                p.scopes.join(", ")
            };
            lines.push(bind(&key, &format!("{} ({scope})", p.name)));
        }
    }
    lines.push(Line::from(Span::styled(
        "  Node drain controls",
        theme::title(),
    )));
    lines.push(bind(
        "PgUp/PgDn (node drain)",
        "scroll options, confirmation, or progress",
    ));
    // Saved bookmarks: their chord (if any) and where they jump.
    if !app.bookmarks.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  Bookmarks", theme::title())));
        for b in &app.bookmarks {
            let key = b
                .key
                .as_deref()
                .map(|k| {
                    crate::keys::KeyChord::parse(k)
                        .map(|c| c.label())
                        .unwrap_or_else(|_| format!("{k}?"))
                })
                .unwrap_or_else(|| ":".to_string());
            lines.push(bind(&key, &format!("★ {}", b.name)));
        }
    }
    // Saved workspaces: their chord (if any), view count, and Tab hint.
    if !app.workspaces.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  Workspaces", theme::title())));
        for w in &app.workspaces {
            let key = w
                .key
                .as_deref()
                .map(|k| {
                    crate::keys::KeyChord::parse(k)
                        .map(|c| c.label())
                        .unwrap_or_else(|_| format!("{k}?"))
                })
                .unwrap_or_else(|| ":".to_string());
            lines.push(bind(
                &key,
                &format!("▦ {} ({} views · Tab to cycle)", w.name, w.views.len()),
            ));
        }
    }
    lines.push(Line::from(Span::styled(
        "  Help navigation",
        theme::title(),
    )));
    lines.push(bind(
        app.keymap.label("help", Action::Back),
        "clear the search or close help",
    ));
    lines.push(bind(
        app.keymap.label("help", Action::Close),
        "close help and return to the previous screen",
    ));
    let key_width = lines
        .iter()
        .filter(|line| line.spans.len() == 2)
        .map(|line| line.spans[0].width())
        .max()
        .unwrap_or(14)
        .min(width.saturating_sub(4) / 2)
        .max(1);
    // Keep section headings with matching entries so each binding has context.
    let needle = app.help_filter.to_lowercase();
    let (lines, title) = if needle.is_empty() {
        (lines, " Help ".to_string())
    } else {
        let mut shown = Vec::new();
        let mut heading = None;
        let mut matches = 0;
        for line in lines {
            let text = line_text(&line);
            if text.is_empty() {
                continue;
            }
            let matched = text.to_lowercase().contains(&needle);
            let binding = if line.spans.len() != 2 {
                heading = Some(line);
                None
            } else {
                Some(line)
            };
            if matched {
                matches += 1;
                if let Some(heading) = heading.take() {
                    if !shown.is_empty() {
                        shown.push(Line::default());
                    }
                    shown.push(heading);
                }
                if let Some(binding) = binding {
                    shown.push(binding);
                }
            }
        }
        let title = format!(" Help · /{} [{}] ", app.help_filter, matches);
        (shown, title)
    };
    let lines: Vec<Line> = lines
        .into_iter()
        .flat_map(|line| {
            if line.spans.len() != 2 {
                return wrap_line(highlight_matches(line, &app.help_filter), width);
            }
            let mut spans = line.spans.into_iter();
            let keys = wrap_help_span(spans.next().unwrap(), key_width, &app.help_filter);
            let descriptions = wrap_help_span(
                spans.next().unwrap(),
                width.saturating_sub(key_width + 4).max(1),
                &app.help_filter,
            );
            (0..keys.len().max(descriptions.len()))
                .map(|i| {
                    let mut row = vec![Span::raw("  ")];
                    let used = keys.get(i).map_or(0, Line::width);
                    if let Some(key) = keys.get(i) {
                        row.extend(key.spans.clone());
                    }
                    row.push(Span::raw(" ".repeat(key_width.saturating_sub(used) + 2)));
                    if let Some(description) = descriptions.get(i) {
                        row.extend(description.spans.clone());
                    }
                    Line::from(row)
                })
                .collect()
        })
        .collect();
    (lines, title)
}

pub(crate) struct HelpCache {
    key: u64,
    lines: Vec<Line<'static>>,
    title: String,
}

fn help_cache_key(app: &App, width: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    (
        width,
        &app.help_filter,
        theme::yellow(),
        theme::crust(),
        theme::dim(),
        theme::title(),
    )
        .hash(&mut hash);
    for (scope, action, _) in app.keymap.entries() {
        (scope, action.name(), app.keymap.label(scope, action)).hash(&mut hash);
    }
    app.plugins.len().hash(&mut hash);
    for plugin in &app.plugins {
        (&plugin.key, &plugin.palette, &plugin.scopes, &plugin.name).hash(&mut hash);
    }
    app.bookmarks.len().hash(&mut hash);
    for bookmark in &app.bookmarks {
        (&bookmark.key, &bookmark.name).hash(&mut hash);
    }
    app.workspaces.len().hash(&mut hash);
    for workspace in &app.workspaces {
        (&workspace.key, &workspace.name, workspace.views.len()).hash(&mut hash);
    }
    hash.finish()
}

fn draw_help(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let key = help_cache_key(app, width);
    if app.help_cache.as_ref().is_none_or(|cache| cache.key != key) {
        let (lines, title) = build_help(app, width);
        app.help_cache = Some(HelpCache { key, lines, title });
    }
    let cache = app.help_cache.as_ref().expect("help content is ready");
    // Record the content height for paging and clamp the offset after layout changes.
    let inner_h = area.height.saturating_sub(2);
    app.help_viewport_h = inner_h;
    let max_scroll = (cache.lines.len().min(usize::from(u16::MAX)) as u16).saturating_sub(inner_h);
    app.help_max_scroll = max_scroll;
    let scroll = app.help_scroll.min(max_scroll);
    app.help_scroll = scroll;
    let title = &cache.title;
    let title = if max_scroll > 0 {
        format!(
            "{title} {} ",
            key_hint(
                app,
                "help",
                &[(Action::Down, "scroll"), (Action::Filter, "search")]
            )
        )
    } else {
        title.clone()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border_focused())
        .title(Span::styled(title, theme::title()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    for (y, line) in cache
        .lines
        .iter()
        .skip(usize::from(scroll))
        .take(usize::from(inner.height))
        .enumerate()
    {
        frame.render_widget(line, Rect::new(inner.x, inner.y + y as u16, inner.width, 1));
    }
    draw_border_scrollbar(
        frame,
        show_scrollbars,
        area,
        usize::from(scroll),
        usize::from(max_scroll),
        usize::from(inner_h),
        false,
    );
}

fn draw_namespaces(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let names = app.filtered_namespaces();
    let browsing = app.ns_filter.is_empty();
    let shortcut = |n: &str| -> Option<String> {
        if !browsing {
            return None;
        }
        let action = if n == "<all>" {
            Action::AllNamespaces
        } else {
            let index = app
                .namespace_favorites
                .iter()
                .position(|favorite| favorite == n)?;
            *Action::FAVORITE_NAMESPACES.get(index)?
        };
        Some(format!("[{}]", app.keymap.label("table", action)))
    };
    let shortcut_width = names
        .iter()
        .filter_map(|n| shortcut(n))
        .map(|label| label.width())
        .max()
        .unwrap_or(0);
    let items: Vec<ListItem> = names
        .iter()
        .map(|n| {
            let mut spans = Vec::new();
            if shortcut_width > 0 {
                let label = shortcut(n).unwrap_or_default();
                spans.push(Span::styled(
                    format!(
                        "{label}{} ",
                        " ".repeat(shortcut_width.saturating_sub(label.width()))
                    ),
                    theme::dim(),
                ));
            }
            if n == "<all>" {
                spans.push(Span::styled(n.clone(), Style::default().fg(theme::teal())));
            } else {
                // Only tag favourites/recents while browsing (the pinned ordering);
                // a filtered list is ranked by match, so a tag there would mislead.
                let (tag, color) = if !browsing {
                    ("", theme::text())
                } else if app.is_favorite_namespace(n) {
                    ("★ ", theme::yellow())
                } else if app.is_recent_namespace(n) {
                    ("· ", theme::sky())
                } else {
                    ("  ", theme::text())
                };
                spans.push(Span::styled(tag.to_string(), theme::dim()));
                spans.push(Span::styled(n.clone(), Style::default().fg(color)));
            }
            let current = if app.namespace.is_empty() {
                n == "<all>"
            } else {
                n == &app.namespace
            };
            let context_default = n == &app.cluster.default_namespace;
            let label = match (current, context_default) {
                (true, true) => " (current, context default)",
                (true, false) => " (current)",
                (false, true) => " (context default)",
                (false, false) => "",
            };
            spans.push(Span::styled(label, theme::dim()));
            ListItem::new(Line::from(spans))
        })
        .collect();
    // Show the type-to-filter buffer in the title so it reads like an input.
    let title = if app.ns_filter.is_empty() {
        " Namespaces (★ favorites and recent) ".to_string()
    } else {
        format!(" Namespaces · /{}_ ", app.ns_filter)
    };
    render_popup_list(
        frame,
        show_scrollbars,
        area,
        (70, 60),
        items,
        Span::styled(title, theme::title()),
        &mut app.ns_state,
    );
}

fn draw_contexts(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let current = app.cluster.context.clone();
    let items: Vec<ListItem> = app
        .filtered_contexts()
        .iter()
        .map(|c| {
            let marker = if *c == current { "● " } else { "  " };
            // Fleet membership (`space` toggles) in the bulk-mark style.
            let fleet = if app.is_fleet_context(c) {
                "✓ "
            } else {
                "  "
            };
            ListItem::new(Line::from(vec![
                Span::styled(fleet, Style::default().fg(theme::mark())),
                Span::styled(
                    format!("{marker}{c}"),
                    Style::default().fg(if *c == current {
                        theme::green()
                    } else {
                        theme::text()
                    }),
                ),
            ]))
        })
        .collect();
    // While typing, show the filter buffer in the title so it reads like an
    // input.
    let title = if app.ctx_filtering {
        format!(" Contexts · /{}_ ", app.ctx_filter)
    } else if !app.ctx_filter.is_empty() {
        format!(" Contexts · /{} ", app.ctx_filter)
    } else {
        " Contexts (type to filter) ".to_string()
    };
    render_popup_list(
        frame,
        show_scrollbars,
        area,
        (50, 60),
        items,
        Span::styled(title, theme::title()),
        &mut app.ctx_state,
    );
}

/// Sort-column picker (`S`): the default ordering pinned first, then the
/// displayed columns in table order (so it doubles as a column reference).
/// The active sort is marked with its direction arrow in the sorter color.
fn draw_sort_picker(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let active = app.sort_column.and_then(|i| {
        app.display_headers()
            .get(i)
            .cloned()
            .map(|h| (h, app.sort_desc))
    });
    let items: Vec<ListItem> = app
        .filtered_sort_entries()
        .iter()
        .map(|e| {
            if e == DEFAULT_SORT_LABEL {
                return ListItem::new(Span::styled(e.clone(), Style::default().fg(theme::teal())));
            }
            match &active {
                Some((h, desc)) if h == e => ListItem::new(Span::styled(
                    format!("{e}{}", if *desc { " ↓" } else { " ↑" }),
                    Style::default().fg(theme::sorter()),
                )),
                _ => ListItem::new(Span::styled(e.clone(), Style::default().fg(theme::text()))),
            }
        })
        .collect();
    // Show the type-to-filter buffer in the title so it reads like an input.
    let title = if app.sort_picker_filter.is_empty() {
        " Sort by ".to_string()
    } else {
        format!(" Sort by · /{}_ ", app.sort_picker_filter)
    };
    render_popup_list(
        frame,
        show_scrollbars,
        area,
        (40, 60),
        items,
        Span::styled(title, theme::title()),
        &mut app.sort_picker_state,
    );
}

/// Copy-field picker (`Y`): each displayed column of the selected row with
/// its full value; ⏎ copies the value to the clipboard. Headers are padded
/// to a common width so the values read as a column.
fn draw_copy_picker(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let entries = app.filtered_copy_entries();
    let pad = entries
        .iter()
        .map(|(h, _)| h.chars().count())
        .max()
        .unwrap_or(0);
    let items: Vec<ListItem> = entries
        .iter()
        .map(|(h, v)| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{h:<pad$}  "), Style::default().fg(theme::teal())),
                Span::styled(v.clone(), Style::default().fg(theme::text())),
            ]))
        })
        .collect();
    // Show the type-to-filter buffer in the title so it reads like an input.
    let title = if app.copy_picker_filter.is_empty() {
        " Copy ".to_string()
    } else {
        format!(" Copy · /{}_ ", app.copy_picker_filter)
    };
    render_popup_list(
        frame,
        show_scrollbars,
        area,
        (60, 60),
        items,
        Span::styled(title, theme::title()),
        &mut app.copy_picker_state,
    );
}

/// Flux suspend/resume / CronJob trigger action menu (`t`). Deliberately a
/// menu rather than a single-key toggle, so acting on a live resource always
/// takes an explicit, visible choice.
fn draw_flux_menu(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let count = app.marked.len().max(1);
    let target = if count == 1 {
        "current selection".to_string()
    } else {
        format!("{count} marked {}", app.kind_plural)
    };
    let items: Vec<ListItem> = app
        .action_menu_items()
        .iter()
        .map(|label| {
            let color = match *label {
                "Suspend" => theme::peach(),
                "Resume" | "Trigger now" | "Sync now" => theme::green(),
                _ => theme::overlay1(),
            };
            ListItem::new(Span::styled(*label, Style::default().fg(color)))
        })
        .collect();
    let subject = if app.cronjob_kind() {
        "CronJob"
    } else if app.argocd_kind() {
        "ArgoCD"
    } else {
        "Flux"
    };
    render_popup_list(
        frame,
        show_scrollbars,
        area,
        (36, 24),
        items,
        Span::styled(format!(" {subject}: {target} "), theme::title()),
        &mut app.flux_menu_state,
    );
}

/// Port-forward picker (`f` on a pod/service): lists the object's declared
/// ports for single-select, plus a "Custom…" entry for manual input.
fn draw_port_forward_picker(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let target = app
        .pf_picker_target
        .as_ref()
        .map(|(_, name)| name.clone())
        .unwrap_or_default();
    let items: Vec<ListItem> = app
        .pf_picker_items
        .iter()
        .map(|label| {
            let color = if *label == "Custom…" {
                theme::overlay1()
            } else {
                theme::green()
            };
            ListItem::new(Span::styled(label.as_str(), Style::default().fg(color)))
        })
        .collect();
    render_popup_list(
        frame,
        show_scrollbars,
        area,
        (40, 24),
        items,
        Span::styled(format!(" Port-forward {target} "), theme::title()),
        &mut app.pf_picker_state,
    );
}

/// Pod file-transfer menu (`t` on a pod): download from or upload to the pod
/// via `kubectl cp`, then two prompts for the source and destination paths.
fn draw_transfer_menu(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let target = match &app.transfer_target {
        Some((_, pod, Some(c))) => format!("{pod}:{c}"),
        Some((_, pod, None)) => pod.clone(),
        None => String::new(),
    };
    let items: Vec<ListItem> = TRANSFER_MENU_ITEMS
        .iter()
        .map(|label| {
            let color = match *label {
                "Download from pod" => theme::green(),
                "Upload to pod" => theme::peach(),
                _ => theme::overlay1(),
            };
            ListItem::new(Span::styled(*label, Style::default().fg(color)))
        })
        .collect();
    render_popup_list(
        frame,
        show_scrollbars,
        area,
        (36, 24),
        items,
        Span::styled(format!(" Transfer: {target} "), theme::title()),
        &mut app.transfer_menu_state,
    );
}

/// Background port-forwards (`:pf`). A full-width view, not a popup — closing
/// it (`esc`) does not stop the forwards; only `x`/`s` on a row does.
fn draw_port_forwards(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    // Running forwards first, then the saved-but-stopped [[forwards]]
    // entries — one keystroke away instead of retyped.
    let mut items: Vec<ListItem> = app
        .port_forwards
        .iter()
        .map(|pf| {
            let name = pf
                .config_name
                .as_ref()
                .map(|n| format!("{n}: "))
                .unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::styled("● ", Style::default().fg(theme::green())),
                Span::styled(
                    format!("{name}{}", pf.label()),
                    Style::default().fg(theme::text()),
                ),
            ]))
        })
        .collect();
    for (_, f) in app.stopped_configured_forwards() {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("○ ", theme::dim()),
            Span::styled(
                format!(
                    "{}: {} {} -n {} (stopped)",
                    f.name, f.target, f.ports, f.namespace
                ),
                theme::dim(),
            ),
        ])));
    }
    let title = format!(" Port-forwards [{}] ", app.port_forwards.len());
    render_framed_list(
        frame,
        show_scrollbars,
        area,
        items,
        Span::styled(title, theme::title()),
        &mut app.pf_state,
    );
}

fn draw_find(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let items: Vec<ListItem> = app
        .find_items
        .iter()
        .map(|it| {
            let location = if it.ns.is_empty() {
                it.name.clone()
            } else {
                format!("{}/{}", it.ns, it.name)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<22} ", it.plural), theme::dim()),
                Span::styled(location, Style::default().fg(theme::text())),
            ]))
        })
        .collect();
    let title = format!(" Find '{}' [{}] ", app.find_query, app.find_items.len());
    render_framed_list(
        frame,
        show_scrollbars,
        area,
        items,
        Span::styled(title, theme::title()),
        &mut app.find_state,
    );
}

fn draw_adjacent(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let status = [
        app.child_status.clone(),
        app.adjacent_warning
            .as_ref()
            .map(|w| format!("adjacent: incomplete ({w})"))
            .unwrap_or_default(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join("\n");
    let area = if status.is_empty() {
        area
    } else {
        let height = status
            .lines()
            .map(|line| line.width().div_ceil(usize::from(area.width.max(1))))
            .sum::<usize>();
        let chunks = Layout::vertical([
            Constraint::Length(
                height
                    .min(4)
                    .min(usize::from(area.height.saturating_sub(3))) as u16,
            ),
            Constraint::Min(0),
        ])
        .split(area);
        frame.render_widget(
            Paragraph::new(status).wrap(ratatui::widgets::Wrap { trim: true }),
            chunks[0],
        );
        chunks[1]
    };
    let items: Vec<ListItem> = if app.adjacent_items.is_empty() {
        let msg = if app.adjacent_pending() {
            "gathering…"
        } else if app.adjacent_warning.is_some() || app.child_status.contains("incomplete") {
            "no results found; lookup is incomplete"
        } else if app.child_status.contains("searching") {
            "searching for children..."
        } else {
            "nothing connected to this object was found"
        };
        vec![ListItem::new(Span::styled(msg, theme::dim()))]
    } else {
        // Columns sized to their widest cell, so a long relation or kind name
        // never pushes its row out of line with the others.
        let relation =
            |it: &crate::store::AdjacentItem| format!("{} {}", it.direction.arrow(), it.relation);
        let relation_w = app
            .adjacent_items
            .iter()
            .map(|it| relation(it).chars().count())
            .max()
            .unwrap_or(0);
        let kind_w = app
            .adjacent_items
            .iter()
            .map(|it| it.kind.chars().count())
            .max()
            .unwrap_or(0);
        app.adjacent_items
            .iter()
            .map(|it| {
                let location = match &it.namespace {
                    Some(ns) if !ns.is_empty() => format!("{ns}/{}", it.name),
                    _ => it.name.clone(),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<relation_w$}  ", relation(it)), theme::dim()),
                    Span::styled(
                        format!("{:<kind_w$}  ", it.kind),
                        Style::default().fg(theme::text()),
                    ),
                    Span::styled(location, Style::default().fg(theme::text())),
                ]))
            })
            .collect()
    };
    let title = format!(
        " {} [{}] {} ",
        app.adjacent_title,
        app.adjacent_items.len(),
        if app.can_discover_children() {
            "(c discover children)"
        } else {
            ""
        },
    );
    render_framed_list(
        frame,
        show_scrollbars,
        area,
        items,
        Span::styled(title, theme::title()),
        &mut app.adjacent_state,
    );
}

fn draw_skins(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
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
        show_scrollbars,
        area,
        (42, 58),
        items,
        Span::styled(" Skins ", theme::title()),
        &mut app.skin_state,
    );
}

fn draw_snapshots(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let items: Vec<ListItem> = app
        .snapshot_list
        .iter()
        .map(|(_, label)| {
            ListItem::new(Span::styled(
                label.clone(),
                Style::default().fg(theme::text()),
            ))
        })
        .collect();
    render_popup_list(
        frame,
        show_scrollbars,
        area,
        (70, 70),
        items,
        Span::styled(" Snapshots ", theme::title()),
        &mut app.snapshot_state,
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

use crate::text::ellipsize as truncate_cols;

fn container_columns_width(widths: &[usize; 8]) -> usize {
    widths.iter().sum::<usize>()
        + widths.iter().filter(|&&w| w > 0).count().saturating_sub(1) * C_GAP
}

fn container_column_widths(app: &App, available: usize) -> [usize; 8] {
    let name_width = app
        .container_list
        .iter()
        .map(|s| s.width())
        .max()
        .unwrap_or(4)
        .clamp(8, 40);
    let state_width = app
        .container_details
        .values()
        .map(|c| c.state.width())
        .max()
        .unwrap_or(7)
        .clamp(7, 32);
    let restart_width = app
        .container_details
        .values()
        .filter_map(|c| c.restarts)
        .map(|n| n.to_string().len())
        .max()
        .unwrap_or(8)
        .max(8);
    let mut widths = [
        8,
        5,
        state_width,
        restart_width,
        C_CPU,
        C_CPU_PCT,
        C_MEM,
        C_MEM_PCT,
    ];
    for indices in [&[5, 7][..], &[4, 6][..], &[1][..]] {
        if container_columns_width(&widths) <= available {
            break;
        }
        for &i in indices {
            widths[i] = 0;
        }
    }
    if container_columns_width(&widths) > available {
        widths[2] = available
            .saturating_sub(8 + restart_width + 2 * C_GAP)
            .min(state_width);
    }
    widths[0] = 0;
    let other_width = container_columns_width(&widths);
    let gap = if other_width > 0 { C_GAP } else { 0 };
    widths[0] = available.saturating_sub(other_width + gap).min(name_width);
    widths
}

fn container_table_line(
    values: [&str; 8],
    colors: [Color; 8],
    widths: &[usize; 8],
) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, &width) in widths.iter().enumerate().filter(|(_, w)| **w > 0) {
        if !spans.is_empty() {
            spans.push(Span::raw(" ".repeat(C_GAP)));
        }
        let value = truncate_cols(values[i], width);
        let padding = " ".repeat(width.saturating_sub(value.width()));
        let text = if i >= 3 {
            format!("{padding}{value}")
        } else {
            format!("{value}{padding}")
        };
        spans.push(Span::styled(text, Style::default().fg(colors[i])));
    }
    Line::from(spans)
}

fn container_detail_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let selected = app
        .container_state
        .selected()
        .and_then(|i| app.container_list.get(i));
    let Some((name, details)) =
        selected.and_then(|name| app.container_details.get(name).map(|d| (name, d)))
    else {
        return vec![Line::styled(
            if app.container_list.is_empty() {
                "No containers available."
            } else {
                "Select a container to see its details."
            },
            theme::dim(),
        )];
    };
    let [startup, readiness, liveness] = details
        .probes
        .map(|present| if present { "configured" } else { "absent" });
    [
        Line::styled(
            format!("Container: {name} ({})", details.kind),
            theme::title(),
        ),
        Line::from(format!("Image: {}", details.image)),
        Line::from(format!("Ports: {}", details.ports)),
        Line::from(format!(
            "Probes: startup={startup}, readiness={readiness}, liveness={liveness}"
        )),
    ]
    .into_iter()
    .flat_map(|line| wrap_line(line, width))
    .collect()
}

fn draw_containers(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.width < 4 || area.height < 4 {
        return;
    }
    let show_scrollbars = app.scrollbars_visible();
    let thresholds = app.resolved_thresholds();
    let qos = if app.container_qos.is_empty() {
        String::new()
    } else {
        format!(" · {}", app.container_qos)
    };
    let title = format!(" Containers{qos} ");
    let footer = key_hint(
        app,
        "containers",
        &[
            (Action::Logs, "logs"),
            (Action::PreviousLogs, "previous"),
            (Action::Shell, "shell"),
            (Action::Transfer, "transfer"),
            (Action::Debug, "debug"),
            (Action::ProviderLogs, "provider"),
        ],
    );
    let desired_width = container_columns_width(&container_column_widths(app, usize::MAX)) + 4;
    let popup_w = desired_width
        .max(84)
        .max(footer.width() + 4)
        .min(usize::from(area.width)) as u16;
    let widths = container_column_widths(app, usize::from(popup_w.saturating_sub(4)));
    let details = container_detail_lines(app, usize::from(popup_w.saturating_sub(6)));
    let rows = app.container_list.len().max(1).min(usize::from(u16::MAX)) as u16;
    let content_height = area.height.saturating_sub(3);
    let min_rows = rows.min(3).min(content_height);
    let section_gap = u16::from(
        area.height >= 16 && details.len() < usize::from(content_height.saturating_sub(min_rows)),
    );
    let content_height = content_height.saturating_sub(section_gap);
    let detail_height = details
        .len()
        .min(usize::from(content_height.saturating_sub(min_rows))) as u16;
    let trend_height = if popup_w >= 84
        && !app.container_list.is_empty()
        && content_height >= min_rows + detail_height + section_gap + 3
    {
        3
    } else {
        0
    };
    let trend_gap = if trend_height > 0 { section_gap } else { 0 };
    let list_height =
        rows.min(content_height.saturating_sub(detail_height + trend_gap + trend_height));
    let popup_h = 3 + list_height + section_gap + detail_height + trend_gap + trend_height;
    let popup = centered_rect_exact(popup_w, popup_h, area);
    clear_region(frame, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border_focused())
        .title(Span::styled(title, theme::title()))
        .title_bottom(Line::from(Span::styled(format!(" {footer} "), theme::dim())).centered());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [header_area, list_area, _, detail_area, _, trend_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(list_height),
        Constraint::Length(section_gap),
        Constraint::Length(detail_height),
        Constraint::Length(trend_gap),
        Constraint::Length(trend_height),
    ])
    .areas(inner);
    let header = container_table_line(
        [
            "NAME", "READY", "STATE", "RESTARTS", "CPU", "%R/L", "MEM", "%R/L",
        ],
        [theme::overlay1(); 8],
        &widths,
    );
    frame.render_widget(
        Paragraph::new(header),
        Rect {
            x: header_area.x + 2,
            width: header_area.width.saturating_sub(2),
            ..header_area
        },
    );
    let items: Vec<ListItem> = app
        .container_list
        .iter()
        .map(|container| {
            let details = app.container_details.get(container);
            let ready = match details.and_then(|d| d.ready) {
                Some(true) => "true",
                Some(false) => "false",
                None => "-",
            };
            let state = details.map(|d| d.state.as_str()).unwrap_or("Unknown");
            let restarts = details.and_then(|d| d.restarts);
            let restart_text = restarts
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into());
            let restart_color = restarts
                .and_then(|n| i64::try_from(n).ok())
                .and_then(|n| thresholds.restarts.severity(n))
                .map(theme::severity_fg)
                .unwrap_or_else(theme::text);
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
                thresholds.utilization,
            );
            let (mem_pct, mem_pct_color) = util_cell(
                usage.map(|(_, m)| m),
                res.mem_request,
                res.mem_limit,
                thresholds.utilization,
            );
            ListItem::new(container_table_line(
                [
                    container,
                    ready,
                    state,
                    &restart_text,
                    &cpu,
                    &cpu_pct,
                    &memory,
                    &mem_pct,
                ],
                [
                    theme::text(),
                    match ready {
                        "true" => theme::green(),
                        "false" => theme::yellow(),
                        _ => theme::overlay1(),
                    },
                    theme::status_color(state),
                    restart_color,
                    theme::yellow(),
                    cpu_pct_color,
                    theme::teal(),
                    mem_pct_color,
                ],
                &widths,
            ))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(theme::selected_row())
        .highlight_symbol("▌ ")
        .highlight_spacing(HighlightSpacing::Always);
    frame.render_stateful_widget(list, list_area, &mut app.container_state);
    draw_border_scrollbar(
        frame,
        show_scrollbars,
        Rect {
            y: list_area.y.saturating_sub(1),
            height: list_area.height.saturating_add(2),
            ..popup
        },
        app.container_state.offset(),
        app.container_list
            .len()
            .saturating_sub(usize::from(list_area.height)),
        usize::from(list_area.height),
        false,
    );
    frame.render_widget(
        Paragraph::new(details).style(Style::default().fg(theme::text())),
        Rect {
            x: detail_area.x + 2,
            width: detail_area.width.saturating_sub(4),
            ..detail_area
        },
    );
    if trend_height > 0 {
        draw_container_trends(
            frame,
            app,
            Rect {
                x: trend_area.x + 2,
                width: trend_area.width.saturating_sub(4),
                ..trend_area
            },
        );
    }
}

fn draw_container_trends(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        Line::styled(
            "Selected container: last 5 min, 5 s bins; · = no sample",
            theme::dim(),
        ),
        Rect::new(area.x, area.y, area.width, 1),
    );
    for (row, cpu, name, color) in [
        (1, true, "CPU", theme::yellow()),
        (2, false, "MEM", theme::teal()),
    ] {
        let bars = app.container_trend_bars(cpu);
        let maximum = bars.iter().flatten().copied().max().unwrap_or(0);
        let scale = if cpu {
            format!("{maximum}m")
        } else if maximum == 0 {
            "0B".into()
        } else {
            crate::columns::fmt_mem(maximum as i64)
        };
        frame.render_widget(
            Line::styled(format!("{name} 0..{scale}"), theme::dim()),
            Rect::new(area.x, area.y + row, 18, 1),
        );
        let sparkline = Sparkline::default()
            .data(bars)
            .max(maximum.max(1))
            .style(Style::default().fg(color))
            .absent_value_symbol("·")
            .absent_value_style(theme::dim());
        frame.render_widget(sparkline, Rect::new(area.x + 18, area.y + row, 60, 1));
    }
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
        Line::from(Span::styled(
            key_hint(
                app,
                "prompt",
                &[(Action::Accept, "apply"), (Action::Back, "cancel")],
            ),
            theme::dim(),
        )),
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
    let show_scrollbars = app.scrollbars_visible();
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
        show_scrollbars,
        area,
        (70, 60),
        items,
        Span::styled(" Set Image ", theme::title()),
        &mut app.container_state,
    );
}

fn draw_drain(frame: &mut Frame, app: &mut App, area: Rect) {
    let styled = |text: String, style| Line::from(Span::styled(text, style));
    clear_region(frame, area);
    let review = app.drain_confirmation();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Node drain | PgUp/PgDn: scroll ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![
        Line::from(format!(
            "Context: {} | {} node(s)",
            app.cluster.context,
            app.drain.targets.len()
        )),
        Line::from(""),
    ];
    let footer;
    if app.drain.started() {
        lines.extend(app.drain.message.lines().map(|line| {
            styled(
                line.to_owned(),
                if app.drain.err {
                    Style::default().fg(theme::red())
                } else {
                    Style::default().fg(theme::text())
                },
            )
        }));
        footer = if app.drain.done {
            "Enter/Esc: close".to_string()
        } else {
            "Esc/Ctrl-C: cancel. Accepted requests cannot be reversed.".to_string()
        };
    } else {
        for (i, row) in app.drain.rows().into_iter().enumerate() {
            let selected = !review && app.drain.field == i;
            lines.push(styled(
                format!("{} {row}", if selected { ">" } else { " " }),
                if selected {
                    theme::selected_row()
                } else {
                    Style::default().fg(theme::text())
                },
            ));
        }
        lines.extend([
            Line::from(""),
            Line::from(
                "Grace: seconds; empty uses the pod default. Zero requests immediate termination.",
            ),
            Line::from("Timeout: 0 (unlimited), 30s, 5m, or 1h30m. One deadline covers all nodes."),
            Line::from(
                "Force does not ensure pod replacement. Nodes stay cordoned after this operation.",
            ),
        ]);
        if review {
            footer = if app.mode == Mode::Prompt {
                format!(
                    "{}\n> {}█\nEnter: confirm   Esc: cancel",
                    app.prompt_label, app.prompt_input
                )
            } else {
                format!(
                    "Confirm drain with these options?\n{}",
                    confirm_action_hint(app, false)
                )
            };
        } else {
            footer = format!(
                "{}\nType to edit. Backspace: remove character. Ctrl-U: clear.",
                key_hint(
                    app,
                    "drain",
                    &[
                        (Action::Down, "next"),
                        (Action::Up, "previous"),
                        (Action::Toggle, "toggle"),
                        (Action::Accept, "review"),
                        (Action::Back, "cancel")
                    ]
                )
            );
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from("Targets:"));
    lines.extend(
        app.drain
            .targets
            .iter()
            .map(|target| Line::from(format!("  {target}"))),
    );
    let footer = if !review && !app.drain.started() && !app.drain.error.is_empty() {
        format!("{}\n{footer}", app.drain.error)
    } else {
        footer
    };
    let footer_lines: Vec<_> = footer
        .lines()
        .flat_map(|line| wrap_line(Line::from(line.to_owned()), usize::from(inner.width).max(1)))
        .collect();
    let footer_height = footer_lines
        .len()
        .min(usize::from(inner.height.saturating_sub(3))) as u16;
    let parts =
        Layout::vertical([Constraint::Min(0), Constraint::Length(footer_height)]).split(inner);
    if app.drain.focus_field && !review && !app.drain.started() {
        let position: usize = lines
            .iter()
            .take(2 + app.drain.field)
            .map(|line| wrap_line(line.clone(), usize::from(inner.width).max(1)).len())
            .sum();
        let row_height = wrap_line(
            lines[2 + app.drain.field].clone(),
            usize::from(inner.width).max(1),
        )
        .len();
        let bottom = position
            .saturating_add(row_height)
            .saturating_sub(usize::from(parts[0].height));
        app.drain.scroll = app.drain.scroll.min(position as u16).max(bottom as u16);
        app.drain.focus_field = false;
    }
    let lines: Vec<_> = lines
        .into_iter()
        .flat_map(|line| wrap_line(line, usize::from(parts[0].width).max(1)))
        .collect();
    let max_scroll = lines
        .len()
        .saturating_sub(usize::from(parts[0].height))
        .min(usize::from(u16::MAX)) as u16;
    app.drain.scroll = app.drain.scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines).scroll((app.drain.scroll, 0)),
        parts[0],
    );
    frame.render_widget(Paragraph::new(footer_lines), parts[1]);
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
            confirm_action_hint(app, app.confirm_allows_force_toggle()),
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
    let show_scrollbars = app.scrollbars_visible();
    if app.cmd_suggestions.is_empty() {
        return;
    }
    let shown = app.cmd_suggestions.len().min(12) as u16;
    let h = shown + 2;
    let keys = key_hint(
        app,
        "command",
        &[
            (Action::Down, "next"),
            (Action::Up, "previous"),
            (Action::Accept, "run"),
        ],
    );
    let full_title = format!(" commands & resources ({keys}) ");
    let w = area
        .width
        .saturating_sub(4)
        .min(full_title.width().saturating_add(2).max(46) as u16);
    let title_width = usize::from(w.saturating_sub(2));
    let hint = [
        full_title,
        format!(" {keys} "),
        format!(" {} ", key_hint(app, "command", &[(Action::Accept, "run")])),
    ]
    .into_iter()
    .find(|title| title.width() <= title_width)
    .unwrap_or_else(|| clip_to_width(" commands ", title_width));
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
            // Saved bookmarks read as a distinct, high-value jump (a ★ tag).
            SuggestKind::Bookmark => ListItem::new(Line::from(vec![
                Span::styled(
                    format!("★ {}", s.label),
                    Style::default().fg(theme::yellow()),
                ),
                Span::styled("  bookmark", theme::dim()),
            ])),
            SuggestKind::Workspace => ListItem::new(Line::from(vec![
                Span::styled(format!("▦ {}", s.label), Style::default().fg(theme::sky())),
                Span::styled("  workspace", theme::dim()),
            ])),
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.cmd_sel));
    render_framed_list(
        frame,
        show_scrollbars,
        rect,
        items,
        Span::styled(hint, theme::title()),
        &mut state,
    );
}

/// Xray hierarchical tree (owner → children → containers).
fn draw_xray(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
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
    let title = format!(" Xray [{}] ", app.xray_items.len());
    render_framed_list(
        frame,
        show_scrollbars,
        area,
        items,
        Span::styled(title, theme::title()),
        &mut app.xray_state,
    );
}

fn draw_fleet(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    use crate::fleet::FleetStatus;
    let items: Vec<ListItem> = app
        .fleet_rows
        .iter()
        .map(|r| {
            let (glyph, gcolor) = match &r.status {
                FleetStatus::Connecting => ("◐", theme::overlay1()),
                FleetStatus::Error(_) => ("●", theme::red()),
                FleetStatus::Ok if r.is_healthy() => ("●", theme::green()),
                FleetStatus::Ok => ("●", theme::yellow()),
            };
            let mut spans = vec![
                Span::styled(format!("{glyph} "), Style::default().fg(gcolor)),
                Span::styled(
                    format!("{:<26}", truncate_cols(&r.context, 26)),
                    Style::default().fg(theme::text()),
                ),
            ];
            match &r.status {
                FleetStatus::Connecting => {
                    spans.push(Span::styled("connecting…", theme::dim()));
                }
                FleetStatus::Error(e) => {
                    spans.push(Span::styled(
                        format!("error: {e}"),
                        Style::default().fg(theme::red()),
                    ));
                }
                FleetStatus::Ok => {
                    let nodes_color = if r.nodes_ready == r.nodes_total {
                        theme::green()
                    } else {
                        theme::red()
                    };
                    let pods_color = if r.pods_unhealthy == 0 {
                        theme::subtext0()
                    } else {
                        theme::red()
                    };
                    spans.push(Span::styled(
                        format!("{:<12}", truncate_cols(&r.version, 12)),
                        theme::dim(),
                    ));
                    spans.push(Span::styled(
                        format!("nodes {}/{}", r.nodes_ready, r.nodes_total),
                        Style::default().fg(nodes_color),
                    ));
                    spans.push(Span::styled(
                        format!("   pods {}✗/{}", r.pods_unhealthy, r.pods_total),
                        Style::default().fg(pods_color),
                    ));
                    match r.flux_failed {
                        Some(0) => spans.push(Span::styled(
                            "   flux ok".to_string(),
                            Style::default().fg(theme::green()),
                        )),
                        Some(n) => spans.push(Span::styled(
                            format!("   flux {n}✗"),
                            Style::default().fg(theme::red()),
                        )),
                        None => spans.push(Span::styled("   flux —".to_string(), theme::dim())),
                    }
                    match r.argocd_degraded {
                        Some(0) => spans.push(Span::styled(
                            "   argo ok".to_string(),
                            Style::default().fg(theme::green()),
                        )),
                        Some(n) => spans.push(Span::styled(
                            format!("   argo {n}✗"),
                            Style::default().fg(theme::red()),
                        )),
                        None => spans.push(Span::styled("   argo —".to_string(), theme::dim())),
                    }
                    let (pol, pc) = if r.readonly {
                        ("   read-only", theme::yellow())
                    } else {
                        ("   write", theme::overlay1())
                    };
                    spans.push(Span::styled(pol.to_string(), Style::default().fg(pc)));
                }
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let title = format!(" Fleet [{}] ", app.fleet_rows.len());
    render_framed_list(
        frame,
        show_scrollbars,
        area,
        items,
        Span::styled(title, theme::title()),
        &mut app.fleet_state,
    );
}

/// Explain-unhealthy view: a ranked, evidence-backed list of findings for the
/// selected object. Lines carrying a navigation target are marked with a `→`.
/// Render a list of [`crate::explain::Finding`]s (shared by the explain and
/// GitOps views): coloured by level, indented, with a `→` on lines that carry
/// a jump target. Shows `empty_msg` while the findings are still gathering.
fn draw_findings(
    frame: &mut Frame,
    show_scrollbars: bool,
    area: Rect,
    title: String,
    findings: &[crate::explain::Finding],
    empty_msg: &str,
    state: &mut ListState,
) {
    use crate::explain::Level;
    let color = |level: Level| match level {
        Level::Heading => theme::yellow(),
        Level::Info => theme::text(),
        Level::Good => theme::green(),
        Level::Warn => theme::peach(),
        Level::Critical => theme::red(),
        Level::Evidence => theme::subtext0(),
    };

    let items: Vec<ListItem> = if findings.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            empty_msg.to_string(),
            theme::dim(),
        )))]
    } else {
        findings
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
                if f.target.is_some() {
                    spans.push(Span::styled("  →", theme::dim()));
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    };

    render_framed_list(
        frame,
        show_scrollbars,
        area,
        items,
        Span::styled(title, theme::title()),
        state,
    );
}

fn draw_explain(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let title = if app.explain_items.is_empty() {
        format!(" {} ", app.explain_title)
    } else {
        format!(
            " {} ({} findings) ",
            app.explain_title,
            app.explain_items.len()
        )
    };
    draw_findings(
        frame,
        show_scrollbars,
        area,
        title,
        &app.explain_items,
        "gathering evidence…",
        &mut app.explain_state,
    );
}

fn draw_argocd(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let title = format!(" {} ", app.argocd_title);
    draw_findings(
        frame,
        show_scrollbars,
        area,
        title,
        &app.argocd_items,
        "reading the Application…",
        &mut app.argocd_state,
    );
}

fn draw_gitops(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
    let title = format!(" {} ", app.gitops_title);
    draw_findings(
        frame,
        show_scrollbars,
        area,
        title,
        &app.gitops_items,
        "following the reconciliation chain…",
        &mut app.gitops_state,
    );
}

/// The PVC browser: local files on the left, the volume on the right, one
/// cursor per pane and a copy that always runs from the focused pane into the
/// other one.
fn draw_pvc_explore(frame: &mut Frame, app: &mut App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let local_title = format!(" local · {} ", app.pvc.local_path.display());
    let mut remote_title = format!(" {} · {} ", app.pvc.claim, app.pvc.remote_path);
    if let Some(mount) = &app.pvc.mount {
        if mount.helper {
            remote_title.push_str("· helper pod ");
        } else {
            remote_title.push_str(&format!("· via {} ", mount.pod));
        }
        if mount.read_only {
            remote_title.push_str("· read-only ");
        }
    }
    if app.pvc.loading {
        remote_title.push_str("· loading… ");
    }

    // A copy in flight replaces its row's size with a bar. Looked up per
    // pane *and* directory, so a copy out of a directory the browser has
    // since left does not leave a bar on a same-named row here.
    let local_dir = app.pvc.local_path.to_string_lossy().into_owned();
    let local_items = pvc_pane_items(
        &app.pvc.local,
        app.pvc.local_error.as_deref(),
        cols[0].width,
        app.pvc.local_truncated,
        &app.pane_transfers(Pane::Local, &local_dir),
    );
    let remote_items = pvc_pane_items(
        &app.pvc.remote,
        app.pvc.remote_error.as_deref(),
        cols[1].width,
        app.pvc.truncated,
        &app.pane_transfers(Pane::Remote, app.pvc.current_dir()),
    );
    let focus = app.pvc.focus;

    render_pvc_pane(
        frame,
        cols[0],
        local_items,
        local_title,
        &mut app.pvc.local_state,
        focus == Pane::Local,
    );
    render_pvc_pane(
        frame,
        cols[1],
        remote_items,
        remote_title,
        &mut app.pvc.remote_state,
        focus == Pane::Remote,
    );
}

fn render_pvc_pane(
    frame: &mut Frame,
    area: Rect,
    items: Vec<ListItem<'static>>,
    title: String,
    state: &mut ListState,
    focused: bool,
) {
    let (border, title_style) = if focused {
        (theme::border_focused(), theme::title())
    } else {
        (theme::border(), theme::dim())
    };
    let list = List::new(items)
        .highlight_style(if focused {
            theme::selected_row()
        } else {
            // The unfocused pane keeps its cursor visible but quiet, so you
            // can see where a copy would land without it competing for
            // attention with the pane you are driving.
            Style::default().add_modifier(Modifier::REVERSED)
        })
        .highlight_symbol("▌ ")
        .highlight_spacing(HighlightSpacing::Always)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(border)
                .title(Span::styled(title, title_style)),
        );
    frame.render_stateful_widget(list, area, state);
}

/// One pane's rows. Sizes are right-aligned against the pane width so both
/// sides line up as a pair of columns rather than two ragged lists.
fn pvc_pane_items(
    entries: &[crate::pvcexplore::Entry],
    error: Option<&str>,
    width: u16,
    truncated: bool,
    copying: &[(&str, u64, u64)],
) -> Vec<ListItem<'static>> {
    use crate::pvcexplore::EntryKind;

    if let Some(e) = error {
        return vec![ListItem::new(Line::from(Span::styled(
            e.to_string(),
            Style::default().fg(theme::red()),
        )))];
    }
    if entries.is_empty() {
        return vec![ListItem::new(Line::from(Span::styled(
            "empty".to_string(),
            theme::dim(),
        )))];
    }
    // 2 borders + the 2-cell highlight symbol.
    let inner = usize::from(width).saturating_sub(4);
    let size_width = 8usize;
    let name_width = inner.saturating_sub(size_width + 1).max(4);

    let mut items: Vec<ListItem<'static>> = entries
        .iter()
        .map(|e| {
            let (label, color) = match e.kind {
                EntryKind::Dir => (format!("{}/", e.name), theme::sapphire()),
                EntryKind::Link if !e.link_target.is_empty() => {
                    (format!("{} → {}", e.name, e.link_target), theme::teal())
                }
                EntryKind::Link => (e.name.clone(), theme::teal()),
                EntryKind::File => (e.name.clone(), theme::text()),
            };
            // Padded by terminal columns, not characters: a CJK filename is
            // twice as wide as it is long, and `{:<n}` would push the size
            // column through the pane border.
            let label = clip_to_width(&label, name_width);
            let pad = name_width.saturating_sub(label.width());
            let size = match (e.kind, e.size) {
                (EntryKind::Dir, _) => String::new(),
                (_, Some(bytes)) => crate::pvcexplore::human_size(bytes),
                // Nothing could stat it — not the same as an empty file.
                (_, None) => "?".to_string(),
            };
            let name = Span::styled(
                format!("{label}{:pad$}", "", pad = pad),
                Style::default().fg(color),
            );
            // A copy of this entry is running: the size is what the bar
            // measures against, so the bar goes where the size was. Both
            // halves together are exactly `size_width` columns, so the rows
            // around it stay aligned.
            match copying.iter().find(|(n, _, _)| *n == e.name) {
                Some(&(_, done, total)) => {
                    let (fill, track) = crate::pvcexplore::progress_bar(done, total, size_width);
                    ListItem::new(Line::from(vec![
                        name,
                        Span::raw(" "),
                        Span::styled(fill, Style::default().fg(theme::peach())),
                        Span::styled(track, Style::default().fg(theme::surface1())),
                    ]))
                }
                None => ListItem::new(Line::from(vec![
                    name,
                    Span::styled(format!(" {size:>size_width$}"), theme::dim()),
                ])),
            }
        })
        .collect();
    if truncated {
        // Not "N more": both panes stop at the cap without counting past it —
        // the volume side because `head` closes the pipe inside the container.
        items.push(ListItem::new(Line::from(Span::styled(
            format!(
                "… showing the first {} entries",
                crate::pvcexplore::MAX_ENTRIES
            ),
            Style::default().fg(theme::peach()),
        ))));
    }
    items
}

/// Truncate to `max` terminal columns, ending with an ellipsis when cut.
/// [`crate::text::ellipsize`] counts characters, which is the wrong unit for
/// arbitrary file names off a volume.
fn clip_to_width(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Session-local timeline: the state changes observed for one object while
/// sofka has been watching, oldest first.
fn draw_timeline(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_scrollbars = app.scrollbars_visible();
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
        show_scrollbars,
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

fn navigation_hint(app: &App, width: u16) -> String {
    let scope = app.key_scope();
    if scope == "table" {
        let cycle = format!(
            "{}/{}: {}",
            app.keymap.first_label(scope, Action::NextView),
            app.keymap.first_label(scope, Action::PreviousView),
            if app.active_workspace.is_some() {
                "workspace"
            } else {
                "resources"
            }
        );
        let mut actions = vec![
            (Action::Command, "command"),
            (Action::Help, "help"),
            (Action::Filter, "filter"),
        ];
        if app.hide_header || !header_hints_fit(width) {
            actions.extend([
                (Action::Yaml, "yaml"),
                (Action::Describe, "describe"),
                (Action::Logs, "logs"),
                (Action::Edit, "edit"),
                (Action::ShellOrScale, "shell/scale"),
                (Action::Delete, "delete"),
            ]);
        }
        actions.extend([
            (Action::Sort, "sort"),
            (Action::Mark, "mark"),
            (Action::Back, "back"),
        ]);
        return format!("{cycle}  {}", key_hint(app, scope, &actions));
    }
    if scope == "port_forward_picker" {
        return key_hint(
            app,
            scope,
            &[
                (Action::Accept, "start"),
                (Action::Edit, "edit local port"),
                (Action::Back, "back"),
            ],
        );
    }
    let preferred = match scope {
        "logs" => &[
            Action::Back,
            Action::Filter,
            Action::Follow,
            Action::LogMarker,
            Action::LogWarnings,
            Action::Wrap,
            Action::Stream,
            Action::Copy,
            Action::Save,
            Action::PageUp,
            Action::PageDown,
        ][..],
        "detail" | "diff" | "events" => &[
            Action::Back,
            Action::Filter,
            Action::NextMatch,
            Action::PreviousMatch,
            Action::Wrap,
            Action::Fullscreen,
            Action::Copy,
            Action::PageUp,
            Action::PageDown,
        ][..],
        "pvc_explore" => &[
            Action::Back,
            Action::SwitchPane,
            Action::Accept,
            Action::Parent,
            Action::Copy,
            Action::Shell,
            Action::Refresh,
        ][..],
        "port_forwards" => &[
            Action::Back,
            Action::Up,
            Action::Down,
            Action::Start,
            Action::Toggle,
        ][..],
        "contexts" => &[
            Action::Back,
            Action::Accept,
            Action::Rename,
            Action::FleetMark,
        ][..],
        _ => &[
            Action::Back,
            Action::Up,
            Action::Down,
            Action::Accept,
            Action::Logs,
            Action::Refresh,
            Action::Filter,
            Action::Delete,
            Action::Help,
        ][..],
    };
    let available: Vec<_> = preferred
        .iter()
        .filter(|&&action| {
            app.keymap
                .entries()
                .any(|(s, a, _)| s == scope && a == action)
        })
        .map(|&action| (action, action.description()))
        .collect();
    key_hint(app, scope, &available)
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
                    format!(
                        "  {} apply server-side",
                        app.keymap.label("filter", Action::Accept)
                    ),
                    Style::default().fg(theme::yellow()),
                ));
            } else {
                spans.push(Span::styled(app.filter_location(), theme::dim()));
            }
            Line::from(spans)
        }
        Mode::LogFilter => Line::from(vec![
            Span::styled(
                "log filter (text · /re/ · !invert) /",
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
            confirm_action_hint(app, app.confirm_allows_force_toggle()),
            Style::default().fg(theme::yellow()),
        )),
        _ => Line::from(Span::styled(
            navigation_hint(app, frame.area().width),
            theme::dim(),
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Status indicator for views without an active resource refresh source.
/// Document content is static; table data follows the watch.
fn sync_indicator(mode: Mode, doc_filter_return: Mode, synced: bool) -> (&'static str, Color) {
    let static_doc = match mode {
        Mode::Detail | Mode::Diff | Mode::Explain => true,
        // A directory listing is fetched once by exec, not watched — `r`
        // re-reads it. Calling it live would be a lie.
        Mode::PvcExplore => true,
        // `/` search over one of those documents — same underlying snapshot.
        Mode::DocFilter => matches!(doc_filter_return, Mode::Detail | Mode::Diff),
        _ => false,
    };
    if static_doc {
        ("○ static", theme::overlay1())
    } else if synced {
        ("● live", theme::green())
    } else {
        ("○ syncing", theme::yellow())
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let style = if app.flash_err {
        Style::default().fg(theme::red())
    } else {
        Style::default().fg(theme::subtext0())
    };
    let (synced, sync_color) = if app.refresh_task.is_some() {
        ("● refresh", theme::sky())
    } else if app.resource_refresh_available() {
        ("○ stopped", theme::overlay1())
    } else {
        sync_indicator(app.mode, app.doc_filter_return, app.store.synced)
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(12)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", diagnostic_value(app, &app.flash)),
            style,
        ))),
        cols[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            synced,
            Style::default().fg(sync_color),
        )))
        .alignment(Alignment::Right),
        cols[1],
    );
}

fn confirm_action_hint(app: &App, allows_force: bool) -> String {
    let mut actions = vec![(Action::Accept, "confirm"), (Action::Back, "cancel")];
    if allows_force {
        actions.extend([
            (Action::Force, "toggle force"),
            (Action::Cascade, "cascade"),
        ]);
    }
    key_hint(app, "confirm", &actions)
}

fn draw_border_scrollbar(
    frame: &mut Frame,
    show_scrollbars: bool,
    area: Rect,
    position: usize,
    max_offset: usize,
    visible: usize,
    horizontal: bool,
) {
    if !show_scrollbars || max_offset == 0 || visible == 0 || area.width < 3 || area.height < 3 {
        return;
    }
    let (orientation, track) = if horizontal {
        (
            ScrollbarOrientation::HorizontalBottom,
            Rect::new(area.x + 1, area.bottom() - 1, area.width - 2, 1),
        )
    } else {
        (
            ScrollbarOrientation::VerticalRight,
            Rect::new(area.right() - 1, area.y + 1, 1, area.height - 2),
        )
    };
    let track = track.intersection(frame.area());
    if track.is_empty() {
        return;
    }
    // Ratatui uses content_length - 1 as the maximum position, then adds
    // the viewport length to calculate the thumb size.
    let mut state = ScrollbarState::new(max_offset.saturating_add(1))
        .position(position.min(max_offset))
        .viewport_content_length(visible);
    let scrollbar = Scrollbar::new(orientation)
        .thumb_symbol(if horizontal { "─" } else { "│" })
        .track_symbol(Some(if horizontal { "─" } else { "│" }))
        .begin_symbol(None)
        .end_symbol(None)
        .thumb_style(Style::default().fg(theme::text()))
        .track_style(theme::dim());
    frame.render_stateful_widget(scrollbar, track, &mut state);
}

fn draw_document_scrollbars(
    frame: &mut Frame,
    show_scrollbars: bool,
    view: &crate::app::Scrollable,
    area: Rect,
) {
    let (rows, widest) = view.scroll_dimensions();
    let height = usize::from(area.height.saturating_sub(2));
    let width = usize::from(area.width.saturating_sub(2));
    draw_border_scrollbar(
        frame,
        show_scrollbars,
        area,
        view.scroll,
        rows.saturating_sub(height),
        height,
        false,
    );
    if !view.wrap && (widest > width || view.hscroll > 0) {
        draw_border_scrollbar(
            frame,
            show_scrollbars,
            area,
            view.hscroll,
            widest.saturating_sub(1),
            width,
            true,
        );
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
    show_scrollbars: bool,
    area: Rect,
    percent: (u16, u16),
    items: Vec<ListItem<'a>>,
    title: T,
    state: &mut ListState,
) where
    T: Into<Line<'a>>,
{
    let popup = centered_rect_with_min(percent.0, percent.1, 32, 8, area);
    clear_region(frame, popup);
    render_framed_list(frame, show_scrollbars, popup, items, title, state);
}

fn render_framed_list<'a, T>(
    frame: &mut Frame,
    show_scrollbars: bool,
    area: Rect,
    items: Vec<ListItem<'a>>,
    title: T,
    state: &mut ListState,
) where
    T: Into<Line<'a>>,
{
    let heights: Vec<_> = items.iter().map(ListItem::height).collect();
    let total: usize = heights.iter().sum();
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
    let visible = usize::from(area.height.saturating_sub(2));
    let position = heights.iter().take(state.offset()).sum();
    draw_border_scrollbar(
        frame,
        show_scrollbars,
        area,
        position,
        total.saturating_sub(visible),
        visible,
        false,
    );
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

    /// The PVC browser pads file names into a fixed column. A CJK name is
    /// twice as wide as it is long, so counting characters would push the size
    /// column through the pane border.
    #[test]
    fn clip_to_width_measures_terminal_columns() {
        assert_eq!(clip_to_width("abc", 5), "abc");
        assert_eq!(clip_to_width("abcdef", 4), "abc…");
        // Six characters, twelve columns wide.
        assert_eq!("日本語ファイル".width(), 14);
        let clipped = clip_to_width("日本語ファイル", 8);
        assert!(
            clipped.width() <= 8,
            "{clipped:?} is {} wide",
            clipped.width()
        );
        assert!(clipped.ends_with('…'));
        // No room even for the ellipsis.
        assert_eq!(clip_to_width("abc", 0), "");
    }

    #[tokio::test]
    async fn palette_title_fits_wide_and_narrow_terminals() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut app = App::new(crate::k8s::Cluster::fake(), tx);
        app.all_contexts = vec!["gke-west".into()];
        for c in ":pods @gke".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
        }
        for custom_keys in [false, true] {
            if custom_keys {
                let cfg: crate::config::Config = toml::from_str(
                    "[keys.command]\ndown = 'ctrl-n'\nup = 'ctrl-p'\naccept = 'ctrl-y'",
                )
                .unwrap();
                app.keymap = crate::keymap::Keymap::compile(&cfg.keys).unwrap();
            }
            let keys = key_hint(
                &app,
                "command",
                &[
                    (Action::Down, "next"),
                    (Action::Up, "previous"),
                    (Action::Accept, "run"),
                ],
            );
            for (width, expected) in [
                (100, format!(" commands & resources ({keys}) ")),
                (60, format!(" {keys} ")),
                (
                    30,
                    format!(
                        " {} ",
                        key_hint(&app, "command", &[(Action::Accept, "run")])
                    ),
                ),
            ] {
                let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
                terminal
                    .draw(|f| draw_palette(f, &mut app, f.area()))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let title = (0..width)
                    .map(|x| buffer[(x, 8)].symbol())
                    .collect::<String>();
                assert!(title.contains(&expected), "width {width}: {title:?}");
                assert!(title.contains('╮'), "missing right border: {title:?}");
            }
        }
    }

    #[tokio::test]
    async fn help_cache_tracks_inputs_and_reuses_lines_when_scrolling() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut app = App::new(crate::k8s::Cluster::fake(), tx);
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        app.handle_key(key(KeyCode::Char('?'))).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let initial = app.help_cache.as_ref().unwrap().lines.as_ptr();
        app.handle_key(key(KeyCode::Down)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(initial, app.help_cache.as_ref().unwrap().lines.as_ptr());
        for step in 0..7 {
            match step {
                0 => app.plugins.push(crate::config::Plugin {
                    name: "plugin sample".into(),
                    key: "ctrl-p".into(),
                    ..Default::default()
                }),
                1 => app.bookmarks.push(crate::config::Bookmark {
                    name: "bookmark sample".into(),
                    ..Default::default()
                }),
                2 => app.workspaces.push(crate::config::Workspace {
                    name: "workspace sample".into(),
                    ..Default::default()
                }),
                3 => app.workspaces[0]
                    .views
                    .push(crate::config::WorkspaceView::default()),
                4 => {
                    let cfg: crate::config::Config =
                        toml::from_str("[keys.table]\nlogs = [\"ctrl-l\"]").unwrap();
                    app.keymap = crate::keymap::Keymap::compile(&cfg.keys).unwrap();
                }
                5 => {
                    app.handle_key(key(KeyCode::Char('/'))).unwrap();
                    for c in "sample".chars() {
                        app.handle_key(key(KeyCode::Char(c))).unwrap();
                    }
                    app.handle_key(key(KeyCode::Enter)).unwrap();
                }
                _ => terminal.backend_mut().resize(60, 30),
            }
            let previous_key = app.help_cache.as_ref().unwrap().key;
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            let width = usize::from(terminal.backend().buffer().area.width.saturating_sub(2));
            let cache = app.help_cache.as_ref().unwrap();
            assert_ne!(previous_key, cache.key, "step {step}");
            assert_eq!(
                (cache.lines.clone(), cache.title.clone()),
                build_help(&app, width)
            );
            let cached = terminal.backend().buffer().clone();
            app.help_cache = None;
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            assert_eq!(&cached, terminal.backend().buffer());
        }
    }

    #[test]
    fn direct_cells_match_table_rendering() {
        use ratatui::buffer::Buffer;
        use ratatui::widgets::{Cell, Row, StatefulWidget, Table, TableState};
        for width in [0, 1, 2, 5, 20] {
            for alignment in [Alignment::Left, Alignment::Center, Alignment::Right] {
                for selected in [false, true] {
                    for value in [
                        "",
                        "plain",
                        "界e\u{301}界",
                        "first\nsecond",
                        "a long clipped value",
                    ] {
                        let text = Text::from(Line::from(vec![
                            Span::styled(value, Style::default().fg(Color::Red)),
                            Span::styled("!", Style::default().add_modifier(Modifier::BOLD)),
                        ]))
                        .alignment(alignment);
                        let area = Rect::new(0, 0, width, 1);
                        let mut expected = Buffer::empty(area);
                        let mut actual = Buffer::empty(area);
                        let row = theme::header_row();
                        let cell_style = Style::default().bg(Color::Blue);
                        let mut state = TableState::default().with_selected(selected.then_some(0));
                        StatefulWidget::render(
                            Table::new(
                                [
                                    Row::new([Cell::from(text.clone()).style(cell_style)])
                                        .style(row),
                                ],
                                [Constraint::Length(width)],
                            )
                            .row_highlight_style(theme::selected_row()),
                            area,
                            &mut expected,
                            &mut state,
                        );
                        RenderCell::from(text).style(cell_style).render(
                            area,
                            &mut actual,
                            row,
                            selected,
                        );
                        assert_eq!(
                            actual, expected,
                            "{width} {alignment:?} {selected} {value:?}"
                        );
                    }
                }
            }
        }
    }

    /// A describe/YAML/diff document is a snapshot — the status bar must not
    /// claim it's live (#175 follow-up report from Discord).
    #[test]
    fn sync_indicator_labels_static_documents() {
        assert_eq!(sync_indicator(Mode::Table, Mode::Detail, true).0, "● live");
        assert_eq!(
            sync_indicator(Mode::Table, Mode::Detail, false).0,
            "○ syncing"
        );
        assert_eq!(
            sync_indicator(Mode::Detail, Mode::Detail, true).0,
            "○ static"
        );
        assert_eq!(sync_indicator(Mode::Diff, Mode::Detail, true).0, "○ static");
        // `/` search inside a document keeps the static label…
        assert_eq!(
            sync_indicator(Mode::DocFilter, Mode::Detail, true).0,
            "○ static"
        );
        // …but searching help (not resource data) doesn't.
        assert_eq!(
            sync_indicator(Mode::DocFilter, Mode::Help, true).0,
            "● live"
        );
        // Events are watch-backed, genuinely live.
        assert_eq!(sync_indicator(Mode::Events, Mode::Detail, true).0, "● live");
    }

    #[test]
    fn header_title_shows_connected_kubernetes_revision() {
        assert_eq!(line_text(&header_title("")), " sofka ");
        assert_eq!(
            line_text(&header_title("v1.36.2-eks-bca9cf6")),
            " sofka · K8s Rev: v1.36.2-eks-bca9cf6 "
        );
    }

    /// Deficit: a Flex column whose content fits inside its weight-share takes
    /// only what it needs — the padding it would have hoarded under a pure
    /// Fill split goes to the column that's actually starved (#166).
    #[test]
    fn width_deficit_trims_padding_before_data() {
        // NAME needs 10 but weighs 6; EXTERNAL-IP needs 15 and weighs 1.
        let cols = [
            (ColWidth::Flex(6), 10),
            (ColWidth::Flex(1), 15),
            (ColWidth::Cap(7), 3),
        ];
        let widths = distribute_column_widths(28, &cols);
        // 28 - AGE(3) = 25 for the flex pair: NAME's share (25*6/7 = 21)
        // covers its 10, so EXTERNAL-IP gets the remaining 15 in full.
        assert_eq!(widths, vec![10, 15, 3]);
    }

    /// Surplus: everyone gets their content width, then the leftover spreads
    /// by weight so NAME still dominates a wide window.
    #[test]
    fn width_surplus_spreads_by_weight() {
        let cols = [(ColWidth::Flex(6), 10), (ColWidth::Flex(2), 5)];
        let widths = distribute_column_widths(55, &cols);
        // 55 - 15 needed = 40 surplus → 30/10 by weight.
        assert_eq!(widths, vec![40, 15]);
        assert_eq!(widths.iter().map(|&w| u32::from(w)).sum::<u32>(), 55);
    }

    /// A genuinely too-narrow window falls back to weight shares for the
    /// unsatisfiable columns — the old Fill behavior, minus the padding.
    #[test]
    fn width_hard_deficit_shares_by_weight() {
        let cols = [(ColWidth::Flex(6), 100), (ColWidth::Flex(1), 100)];
        let widths = distribute_column_widths(21, &cols);
        assert_eq!(widths, vec![18, 3]);
    }

    /// Exact widths are honored verbatim; caps shrink to content but never
    /// grow past the ceiling.
    #[test]
    fn width_exact_and_cap_rules() {
        let cols = [
            (ColWidth::Exact(12), 3),
            (ColWidth::Cap(19), 7),
            (ColWidth::Cap(19), 25),
            (ColWidth::Flex(1), 5),
        ];
        let widths = distribute_column_widths(60, &cols);
        assert_eq!(widths[0], 12, "user width kept even when content is short");
        assert_eq!(widths[1], 7, "cap shrinks to the widest visible value");
        assert_eq!(widths[2], 19, "cap still bounds long content");
        // Flex takes its 5 plus the whole surplus (60 - 12 - 7 - 19 = 22).
        assert_eq!(widths[3], 22);
    }

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

    #[tokio::test]
    async fn a_running_copy_draws_its_bar_in_the_size_column() {
        use crate::app::{TransferAnchor, TransferProgress};
        use crate::k8s::Cluster;
        use crate::pvcexplore::{Entry, EntryKind};
        use ratatui::{Terminal, backend::TestBackend};

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut app = App::new(Cluster::fake(), tx);
        let entry = |name: &str, size| Entry {
            name: name.into(),
            kind: EntryKind::File,
            size: Some(size),
            link_target: String::new(),
        };
        app.mode = Mode::PvcExplore;
        app.pvc.active = true;
        app.pvc.claim = "data".into();
        app.pvc.namespace = "default".into();
        app.pvc.remote_path = "/srv".into();
        app.pvc.remote = vec![entry("big.tar", 5_368_709_120), entry("small.txt", 12)];
        app.pvc.remote_state.select(Some(0));
        app.transfers.push(TransferProgress::fake(
            app.generation,
            TransferAnchor {
                pane: crate::app::Pane::Remote,
                claim: "default/data".into(),
                dir: "/srv".into(),
                name: "big.tar".into(),
            },
            5_368_709_120 / 2,
            5_368_709_120,
        ));

        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = term.backend().buffer().clone();
        let row = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        // The volume pane is the right half; find the two rows in it.
        let copying = (0..buffer.area.height)
            .map(row)
            .find(|line| line.contains("big.tar"))
            .unwrap_or_default();
        let idle = (0..buffer.area.height)
            .map(row)
            .find(|line| line.contains("small.txt"))
            .unwrap_or_default();

        // Half of a 5 GB copy: four filled cells and four of track, in the
        // eight columns the size would have occupied.
        assert!(
            copying.contains("\u{2588}\u{2588}\u{2588}\u{2588}\u{2591}\u{2591}\u{2591}\u{2591}"),
            "{copying:?}"
        );
        assert!(!copying.contains("5.0G"), "the bar replaces the size");
        // And the row next to it is untouched, so the column still lines up.
        assert!(idle.contains("12B"), "{idle:?}");
        // Char columns, not byte offsets: a box-drawing cell is three bytes.
        let column = |line: &str, needle: &str| {
            line.find(needle)
                .map(|at| line[..at].chars().count())
                .unwrap_or_default()
        };
        assert_eq!(
            column(&copying, "\u{2588}\u{2588}\u{2588}\u{2588}"),
            column(&idle, "     12B"),
            "the bar starts where the size column does"
        );
    }

    #[tokio::test]
    async fn a_local_row_wears_its_bar_too() {
        // The two panes are looked up by different strings — the volume's
        // directory and the local path — so a renderer that asked for one
        // when it meant the other would lose every upload's bar.
        use crate::app::{TransferAnchor, TransferProgress};
        use crate::k8s::Cluster;
        use crate::pvcexplore::{Entry, EntryKind};
        use ratatui::{Terminal, backend::TestBackend};

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut app = App::new(Cluster::fake(), tx);
        app.mode = Mode::PvcExplore;
        app.pvc.active = true;
        app.pvc.claim = "data".into();
        app.pvc.namespace = "default".into();
        app.pvc.remote_path = "/srv".into();
        app.pvc.local_path = std::path::PathBuf::from("/tmp/here");
        app.pvc.local = vec![Entry {
            name: "notes.txt".into(),
            kind: EntryKind::File,
            size: Some(4_000),
            link_target: String::new(),
        }];
        app.transfers.push(TransferProgress::fake(
            app.generation,
            TransferAnchor {
                pane: crate::app::Pane::Local,
                claim: "default/data".into(),
                dir: "/tmp/here".into(),
                name: "notes.txt".into(),
            },
            2_000,
            4_000,
        ));

        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = term.backend().buffer().clone();
        let row = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .find(|line| line.contains("notes.txt"))
            .unwrap_or_default();
        assert!(
            row.contains("\u{2588}\u{2588}\u{2588}\u{2588}\u{2591}\u{2591}\u{2591}\u{2591}"),
            "{row:?}"
        );
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
            // Tabs and other ASCII controls are zero-width.
            "column\tvalue\tthat wraps near a boundary",
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

    #[tokio::test]
    async fn confirm_hint_mentions_force_only_when_supported() {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let app = App::new(crate::k8s::Cluster::fake(), tx);
        assert!(confirm_action_hint(&app, true).contains("toggle force"));
        assert!(!confirm_action_hint(&app, false).contains("toggle force"));
        assert!(confirm_action_hint(&app, true).contains("cascade"));
        assert!(!confirm_action_hint(&app, false).contains("cascade"));
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

    /// Fullscreen logs (`F`) own the entire frame: no header above, no status
    /// line below, and no border glyphs anywhere — so a terminal text
    /// selection copies clean log lines.
    #[tokio::test]
    async fn fullscreen_logs_take_the_whole_frame_without_borders() {
        use crate::k8s::Cluster;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut app = App::new(Cluster::fake(), tx);
        app.mode = Mode::Logs;
        app.logs.view.title = "web — logs".into();
        app.logs.view.lines = (0..3).map(|i| format!("log line {i}")).collect();

        let render = |app: &mut App| {
            let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
            term.draw(|f| draw(f, app)).unwrap();
            let buffer = term.backend().buffer().clone();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<String>>()
        };

        // Bordered by default: the pane sits under the 7-line header.
        let normal = render(&mut app);
        assert!(
            normal.iter().any(|r| r.contains('╭')),
            "normal logs view should draw its border"
        );
        assert!(!normal[0].contains("web — logs"), "header owns the top row");

        app.logs.fullscreen = true;
        let full = render(&mut app);
        assert!(
            full[0].contains("web — logs"),
            "fullscreen title owns the top row: {:?}",
            full[0]
        );
        assert!(
            full.iter().any(|r| r.starts_with("log line")),
            "lines start at column 0 (no left border)"
        );
        for r in &full {
            assert!(
                !r.contains('╭') && !r.contains('│') && !r.contains('╰'),
                "no border glyphs in fullscreen: {r:?}"
            );
        }
    }
}
