use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::{App, Mode, SearchResult};
use crate::model::{ServiceState, VpnState, VpnStatus};

const TEXT: Color = Color::Rgb(198, 208, 245);
const GRAY: Color = Color::Rgb(131, 139, 167);
const BLUE: Color = Color::Rgb(140, 170, 238);
const TEAL: Color = Color::Rgb(129, 200, 190);
const LAVENDER: Color = Color::Rgb(186, 187, 241);
const GREEN: Color = Color::Rgb(166, 209, 137);
const RED: Color = Color::Rgb(231, 130, 132);
const YELLOW: Color = Color::Rgb(229, 200, 144);
const SELECTED_BACKGROUND: Color = Color::Rgb(35, 38, 52);
const INSET: u16 = 2;
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.height < 5 || area.width < 24 {
        frame.render_widget(
            Paragraph::new("Resize to at least 24 × 5").style(Style::default().fg(GRAY)),
            area,
        );
        return;
    }
    let has_error = app.notice.as_ref().is_some_and(|notice| notice.is_error);
    let error_height = u16::from(has_error);
    let content = Rect::new(area.x, area.y, area.width, area.height - 2 - error_height);
    let separator = Rect::new(area.x, area.bottom() - 2, area.width, 1);
    let footer = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    let body = body_area(content);
    match app.mode {
        Mode::Panel => {
            let panel_body = body_area(Rect::new(area.x, area.y, area.width, area.height - 2));
            render_panel(frame, app, panel_body, error_height);
        }
        Mode::Search => render_search(frame, app, body),
        Mode::Help => render_help(frame, app, body),
    }
    if let Some(notice) = app.notice.as_ref().filter(|notice| notice.is_error) {
        let error = Rect::new(body.x, separator.y - 1, body.width, 1);
        frame.render_widget(
            Paragraph::new(truncate(&notice.message, error.width)).style(Style::default().fg(RED)),
            error,
        );
    }
    frame.render_widget(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(GRAY)),
        separator,
    );
    render_footer(frame, app, footer);
}

fn body_area(area: Rect) -> Rect {
    let top_padding = u16::from(area.height >= 4);
    Rect::new(
        area.x + INSET,
        area.y + top_padding,
        area.width.saturating_sub(INSET * 2),
        area.height.saturating_sub(top_padding),
    )
}

fn render_panel(frame: &mut Frame, app: &App, area: Rect, error_height: u16) {
    let lines = panel_lines(app, area.width);
    let line_count = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let upper_space = (area.height.saturating_sub(line_count) / 2).min(3);
    let panel = Rect::new(
        area.x,
        area.y + upper_space,
        area.width,
        area.height.saturating_sub(upper_space + error_height),
    );
    frame.render_widget(Paragraph::new(lines), panel);
}

fn panel_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let Some(snapshot) = app.panel_snapshot() else {
        return Vec::new();
    };
    match &snapshot.service {
        ServiceState::Inactive => {
            let mut lines = message_lines("VPN is off", "Connect when you need it.", width);
            if app.locations.is_empty() {
                lines.push(Line::default());
                lines.extend(wrapped_lines(
                    "Start service, then open search to load locations.",
                    width,
                    GRAY,
                ));
            }
            lines
        }
        ServiceState::Failed => {
            message_lines("Service needs attention", "Try starting it again.", width)
        }
        ServiceState::Active => {
            let Some(vpn) = &snapshot.vpn else {
                return Vec::new();
            };
            match vpn.state {
                VpnState::Connected | VpnState::Paused => connection_lines(vpn, width),
                VpnState::Disconnected => message_lines(
                    "Ready to connect",
                    "Choose a location or connect to a recommended server.",
                    width,
                ),
                VpnState::Connecting | VpnState::Unavailable(_) => Vec::new(),
            }
        }
        ServiceState::Activating | ServiceState::Deactivating | ServiceState::Unavailable(_) => {
            Vec::new()
        }
    }
}

fn connection_lines(vpn: &VpnStatus, width: u16) -> Vec<Line<'static>> {
    let city = readable(vpn.city.as_deref());
    let country = readable(vpn.country.as_deref());
    let fallback = if vpn.state == VpnState::Paused {
        "VPN paused"
    } else {
        "Connection active"
    };
    let headline = city.as_deref().or(country.as_deref()).unwrap_or(fallback);
    let mut lines = vec![headline_line(headline, width)];
    if city.is_some()
        && let Some(country) = country
    {
        lines.push(Line::styled(
            truncate(&country, width),
            Style::default().fg(GRAY),
        ));
    }
    let technology = match (
        nonempty(vpn.technology.as_deref()),
        nonempty(vpn.protocol.as_deref()),
    ) {
        (Some(technology), Some(protocol)) => Some(format!("{technology} · {protocol}")),
        (Some(value), None) | (None, Some(value)) => Some(value.to_owned()),
        (None, None) => None,
    };
    let transfer = match (
        nonempty(vpn.received.as_deref()),
        nonempty(vpn.sent.as_deref()),
    ) {
        (Some(received), Some(sent)) => Some(format!("↓ {received}   ↑ {sent}")),
        (Some(received), None) => Some(format!("↓ {received}")),
        (None, Some(sent)) => Some(format!("↑ {sent}")),
        (None, None) => None,
    };
    let session = vpn.state == VpnState::Connected && (vpn.uptime.is_some() || transfer.is_some());
    let metadata: Vec<_> = [
        ("VPN IP", nonempty(vpn.ip.as_deref()), TEAL),
        ("Server", nonempty(vpn.server.as_deref()), TEXT),
        ("Technology", technology.as_deref(), LAVENDER),
    ]
    .into_iter()
    .filter_map(|(label, value, color)| match value {
        Some(value) => Some(metadata_line(label, value, width, color)),
        None if session => Some(Line::default()),
        None => None,
    })
    .collect();
    if !metadata.is_empty() {
        lines.push(Line::default());
        lines.extend(metadata);
    }
    if session {
        lines.push(Line::default());
        lines.push(vpn.uptime.map_or_else(Line::default, |duration| {
            metadata_line("Uptime", &format_uptime(duration), width, TEAL)
        }));
        lines.push(
            transfer
                .as_deref()
                .map_or_else(Line::default, |value| transfer_line(value, width)),
        );
    }
    lines
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn format_uptime(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3_600;
    let minutes = seconds / 60 % 60;
    let seconds = seconds % 60;
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

fn readable(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.replace('_', " "))
}

fn headline_line(headline: &str, width: u16) -> Line<'static> {
    Line::styled(
        truncate(headline, width),
        Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
    )
}

fn message_lines(headline: &str, description: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<_> = wrapped_lines(headline, width, BLUE)
        .into_iter()
        .map(|line| line.patch_style(Style::default().add_modifier(Modifier::BOLD)))
        .collect();
    lines.extend(wrapped_lines(description, width, GRAY));
    lines
}

fn wrapped_lines(text: &str, width: u16, color: Color) -> Vec<Line<'static>> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && Line::from(format!("{line} {word}")).width() > width {
            lines.push(Line::styled(
                std::mem::take(&mut line),
                Style::default().fg(color),
            ));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        for grapheme in word.graphemes(true) {
            if !line.is_empty() && Line::from(format!("{line}{grapheme}")).width() > width {
                lines.push(Line::styled(
                    std::mem::take(&mut line),
                    Style::default().fg(color),
                ));
            }
            line.push_str(grapheme);
        }
    }
    if !line.is_empty() {
        lines.push(Line::styled(line, Style::default().fg(color)));
    }
    lines
}

fn metadata_line(label: &str, value: &str, width: u16, color: Color) -> Line<'static> {
    let label = format!("{label:<10}  ");
    let value_width =
        width.saturating_sub(u16::try_from(Line::from(label.as_str()).width()).unwrap_or(u16::MAX));
    Line::from(vec![
        Span::styled(label, Style::default().fg(GRAY)),
        Span::styled(truncate(value, value_width), Style::default().fg(color)),
    ])
}

fn transfer_line(value: &str, width: u16) -> Line<'static> {
    let mut line = metadata_line("Transfer", value, width, TEXT);
    if let Some(value) = line.spans.pop() {
        line.spans
            .extend(value.content.graphemes(true).map(|grapheme| {
                let color = match grapheme {
                    "↓" => GREEN,
                    "↑" => RED,
                    _ => TEXT,
                };
                Span::styled(grapheme.to_owned(), Style::default().fg(color))
            }));
    }
    line
}

fn render_help(frame: &mut Frame, app: &mut App, area: Rect) {
    let lines = help_lines(area.width);
    let maximum_scroll = lines.len().saturating_sub(usize::from(area.height));
    app.help_scroll = app
        .help_scroll
        .min(u16::try_from(maximum_scroll).unwrap_or(u16::MAX));
    frame.render_widget(Paragraph::new(lines).scroll((app.help_scroll, 0)), area);
}

fn help_lines(width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![headline_line("Shortcuts", width)];
    lines.extend(help_entry("? / Esc", "Back to panel", width));
    lines.extend(help_entry("↑↓ / j/k", "Scroll help", width));
    lines.push(Line::default());
    for (section, entries) in [
        (
            "Panel",
            vec![
                ("i or /", "Search countries and cities"),
                ("c", "Connect or reconnect"),
                ("d", "Disconnect"),
                ("s", "Start or stop service"),
                ("r / Ctrl-R", "Refresh status; locations on next search"),
                ("q / Esc / Ctrl-C", "Quit"),
                ("?", "Open shortcut help"),
            ],
        ),
        (
            "Location search",
            vec![
                ("↑↓ / Ctrl-J/K", "Select a result"),
                ("Ctrl-N/P", "Select a result"),
                ("PgUp / PgDn", "Move ten results"),
                ("Ctrl-A / Home", "Query start"),
                ("Ctrl-E / End", "Query end"),
                ("← / →", "Move query cursor"),
                ("Backspace", "Delete before cursor"),
                ("Ctrl-U", "Clear query"),
                ("Enter", "Connect and return to panel"),
                ("Esc", "Return to panel"),
                ("Ctrl-R", "Refresh status and locations"),
                ("Ctrl-C", "Quit"),
            ],
        ),
        (
            "Help navigation",
            vec![
                ("↑↓ / j/k", "Scroll help"),
                ("Ctrl-J/K", "Scroll help"),
                ("PgUp / PgDn", "Scroll ten rows"),
                ("Home / End", "First or last page"),
            ],
        ),
    ] {
        lines.push(Line::styled(
            section,
            Style::default().fg(GRAY).add_modifier(Modifier::BOLD),
        ));
        for (keys, description) in entries {
            lines.extend(help_entry(keys, description, width));
        }
        lines.push(Line::default());
    }
    lines.pop();
    lines
}

fn help_entry(keys: &str, description: &str, width: u16) -> Vec<Line<'static>> {
    let keys_width = if width >= 44 { 18 } else { 2 };
    let mut lines = wrapped_lines(description, width.saturating_sub(keys_width), TEXT);
    for (index, line) in lines.iter_mut().enumerate() {
        let prefix = if index == 0 && keys_width == 18 {
            Span::styled(format!("{keys:<18}"), Style::default().fg(BLUE))
        } else {
            Span::raw(" ".repeat(usize::from(keys_width)))
        };
        line.spans.insert(0, prefix);
    }
    if keys_width == 2 {
        lines.insert(0, Line::styled(keys.to_owned(), Style::default().fg(BLUE)));
    }
    lines
}

fn render_search(frame: &mut Frame, app: &mut App, area: Rect) {
    let search_height = area.height.min(2);
    let search = Rect::new(area.x, area.y, area.width, search_height);
    let results = Rect::new(
        area.x,
        area.y + search_height,
        area.width,
        area.height - search_height,
    );
    let (query, cursor_column) = query_viewport(
        &app.query,
        app.query_cursor(),
        search.width.saturating_sub(3),
    );
    let search_text = Line::from(vec![
        Span::styled("› ", Style::default().fg(BLUE)),
        if app.query.is_empty() {
            Span::styled(
                truncate(
                    "Search countries and cities…",
                    search.width.saturating_sub(2),
                ),
                Style::default().fg(GRAY),
            )
        } else {
            Span::styled(query, Style::default().fg(TEXT))
        },
    ]);
    frame.render_widget(
        Paragraph::new(search_text).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(GRAY)),
        ),
        search,
    );
    frame.set_cursor_position((search.x + 2 + cursor_column, search.y));
    if app.results.is_empty() {
        let message = if app.query.trim().is_empty() {
            "No cached locations"
        } else {
            "No matches"
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(GRAY)),
            results,
        );
        if app.query.trim().is_empty()
            && app.snapshot.service == ServiceState::Inactive
            && results.height > 1
        {
            let guidance = Rect::new(results.x, results.y + 1, results.width, results.height - 1);
            frame.render_widget(
                Paragraph::new(wrapped_lines(
                    "Start service, then reopen search.",
                    guidance.width,
                    GRAY,
                )),
                guidance,
            );
        }
    } else {
        let rows: Vec<_> = app
            .results
            .iter()
            .map(|row| list_item(row, results.width))
            .collect();
        let list = List::new(rows)
            .style(Style::default().fg(TEXT))
            .highlight_symbol("› ")
            .highlight_style(
                Style::default()
                    .bg(SELECTED_BACKGROUND)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(list, results, &mut app.list_state);
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let status = truncate(&app.status_text(), area.width.saturating_sub(3));
    let status_width = u16::try_from(Line::from(status.as_str()).width()).unwrap_or(0) + 2;
    let status_area = Rect::new(area.right() - 1 - status_width, area.y, status_width, 1);
    let left_width = status_area.x.saturating_sub(area.x + INSET + 2);
    let (activity, color) = footer_activity(app);
    if left_width > 0 {
        let activity = if app.is_working() {
            format!("{} {activity}", spinner_frame())
        } else {
            activity
        };
        frame.render_widget(
            Paragraph::new(truncate(&activity, left_width)).style(Style::default().fg(color)),
            Rect::new(area.x + INSET, area.y, left_width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("⦁ ", Style::default().fg(status_color(app))),
            Span::styled(status, Style::default().fg(GRAY)),
        ])),
        status_area,
    );
}

fn spinner_frame() -> &'static str {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    let elapsed = STARTED.get_or_init(Instant::now).elapsed();
    let index = (elapsed.as_millis() / 60) % (SPINNER_FRAMES.len() as u128);
    SPINNER_FRAMES[usize::try_from(index).unwrap_or(0)]
}

fn footer_activity(app: &App) -> (String, Color) {
    if app.is_initializing() {
        return (String::new(), GRAY);
    }
    if let Some(action) = app.running_action() {
        return (action.label(), GRAY);
    }
    if let Some((finished, total)) = app.loading {
        return (
            if total == 0 {
                "Loading countries…".into()
            } else {
                format!("Loading cities · {finished}/{total} countries")
            },
            GRAY,
        );
    }
    let transition = match app.snapshot.service {
        ServiceState::Activating => Some("Starting service…"),
        ServiceState::Deactivating => Some("Stopping service…"),
        ServiceState::Active
            if app
                .snapshot
                .vpn
                .as_ref()
                .is_some_and(|vpn| vpn.state == VpnState::Connecting) =>
        {
            Some("Connecting…")
        }
        _ => None,
    };
    if let Some(transition) = transition {
        return (transition.into(), GRAY);
    }
    if let Some(notice) = app.notice.as_ref().filter(|notice| !notice.is_error) {
        return (notice.message.clone(), GRAY);
    }
    (String::new(), GRAY)
}

fn truncate(text: &str, width: u16) -> String {
    let width = usize::from(width);
    if Line::from(text).width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut clipped = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let next_width = Line::from(grapheme).width();
        if used + next_width > width - 1 {
            break;
        }
        clipped.push_str(grapheme);
        used += next_width;
    }
    clipped.push('…');
    clipped
}

fn query_viewport(query: &str, cursor: usize, width: u16) -> (String, u16) {
    if Line::from(query).width() <= usize::from(width) {
        let cursor_column = u16::try_from(Line::from(&query[..cursor]).width()).unwrap_or(0);
        return (query.to_owned(), cursor_column);
    }
    let before_cursor = if cursor == query.len() {
        width
    } else {
        width / 2
    };
    let mut size = 0;
    let mut start = cursor;
    for (index, grapheme) in query[..cursor].grapheme_indices(true).rev() {
        let next_size = size + Line::from(grapheme).width();
        if next_size > usize::from(before_cursor) {
            break;
        }
        start = index;
        size = next_size;
    }
    let cursor_column = u16::try_from(size).unwrap_or(0);
    let mut end = start;
    size = 0;
    for (index, grapheme) in query[start..].grapheme_indices(true) {
        size += Line::from(grapheme).width();
        if size > usize::from(width) {
            break;
        }
        end = start + index + grapheme.len();
    }
    (query[start..end].to_owned(), cursor_column)
}

fn list_item(row: &SearchResult, width: u16) -> ListItem<'static> {
    let kind = row.kind.label();
    let kind_width = u16::try_from(kind.len()).unwrap_or(0);
    let label = truncate(&row.label, width.saturating_sub(kind_width + 4));
    let label_width = Line::from(label.as_str()).width();
    let padding = usize::from(width).saturating_sub(label_width + kind.len() + 2);
    let mut spans = Vec::new();
    for (index, grapheme) in label.graphemes(true).enumerate() {
        let matched =
            u32::try_from(index).is_ok_and(|index| row.matches.binary_search(&index).is_ok());
        let style = if matched && grapheme != "…" {
            Style::default().fg(BLUE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        spans.push(Span::styled(grapheme.to_owned(), style));
    }
    spans.push(Span::raw(" ".repeat(padding)));
    spans.push(Span::styled(kind, Style::default().fg(GRAY)));
    ListItem::new(Line::from(spans))
}

fn status_color(app: &App) -> Color {
    match &app.snapshot.service {
        ServiceState::Active => match app.snapshot.vpn.as_ref().map(|vpn| &vpn.state) {
            Some(VpnState::Connected) => GREEN,
            Some(VpnState::Connecting | VpnState::Paused) => YELLOW,
            _ => RED,
        },
        ServiceState::Inactive
        | ServiceState::Activating
        | ServiceState::Deactivating
        | ServiceState::Failed
        | ServiceState::Unavailable(_) => GRAY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ItemKind;
    use crate::model::{Action, Location, Snapshot, VpnStatus};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;

    fn terminal(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
    }

    fn row_text(terminal: &Terminal<TestBackend>, row: u16) -> String {
        let buffer = terminal.backend().buffer();
        let mut continuation_cells = 0;
        (0..buffer.area.width)
            .filter_map(|x| {
                if continuation_cells > 0 {
                    continuation_cells -= 1;
                    return None;
                }
                let symbol = buffer[(x, row)].symbol();
                continuation_cells = Line::from(symbol).width().saturating_sub(1);
                Some(symbol)
            })
            .collect()
    }

    fn text(terminal: &Terminal<TestBackend>) -> String {
        (0..terminal.backend().buffer().area.height)
            .map(|row| row_text(terminal, row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn panel_cells(terminal: &Terminal<TestBackend>) -> Vec<Cell> {
        let buffer = terminal.backend().buffer();
        let count = usize::from(buffer.area.width) * usize::from(buffer.area.height - 3);
        buffer.content()[..count].to_vec()
    }

    fn row_containing(terminal: &Terminal<TestBackend>, value: &str) -> u16 {
        (0..terminal.backend().buffer().area.height)
            .find(|row| row_text(terminal, *row).contains(value))
            .unwrap_or_else(|| panic!("{value:?} absent from\n{}", text(terminal)))
    }

    fn connected() -> Snapshot {
        Snapshot {
            service: ServiceState::Active,
            vpn: Some(VpnStatus {
                state: VpnState::Connected,
                country: Some("United_States".into()),
                city: Some("New_York".into()),
                ip: Some("192.0.2.100".into()),
                technology: Some("NORDLYNX".into()),
                server: None,
                protocol: None,
                uptime: None,
                received: None,
                sent: None,
            }),
        }
    }

    fn connected_with_session() -> Snapshot {
        let mut snapshot = connected();
        let vpn = snapshot.vpn.as_mut().unwrap();
        vpn.server = Some("United States #8373".into());
        vpn.protocol = Some("UDP".into());
        vpn.uptime = Some(Duration::from_secs(250));
        vpn.received = Some("98.95 MiB".into());
        vpn.sent = Some("15.96 MiB".into());
        snapshot
    }

    fn country() -> Location {
        Location {
            country: "Japan".into(),
            city: None,
        }
    }

    #[test]
    fn connected_panel_gives_location_prominence_and_metadata_secondary_weight() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(connected());
        let terminal = terminal(&mut app, 80, 12);
        let hero_y = row_containing(&terminal, "New York");
        let buffer = terminal.backend().buffer();
        assert_eq!(row_text(&terminal, hero_y).trim_end(), "  New York");
        assert_eq!(
            row_text(&terminal, hero_y + 1).trim_end(),
            "  United States"
        );
        assert!(row_text(&terminal, hero_y + 2).trim().is_empty());
        assert_eq!(
            row_text(&terminal, hero_y + 3).trim_end(),
            "  VPN IP      192.0.2.100"
        );
        assert_eq!(
            row_text(&terminal, hero_y + 4).trim_end(),
            "  Technology  NORDLYNX"
        );
        assert_eq!(buffer[(2, hero_y)].fg, BLUE);
        assert!(buffer[(2, hero_y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(2, hero_y + 1)].fg, GRAY);
        assert!(!buffer[(2, hero_y + 1)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(2, hero_y + 3)].fg, GRAY);
        assert_eq!(buffer[(14, hero_y + 3)].fg, TEAL);
        assert!(!terminal.backend().cursor_visible());
        assert!(buffer.content().iter().all(|cell| cell.bg == Color::Reset));
        for unwanted in [
            "Search",
            "nortui",
            "Ctrl-",
            "Enter",
            "Location",
            "Reconnect",
        ] {
            assert!(!text(&terminal).contains(unwanted));
        }
    }

    #[test]
    fn connected_session_fits_widget_and_keeps_statistics_secondary() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(connected_with_session());
        let terminal = terminal(&mut app, 80, 12);
        for (row, expected) in [
            (1, "  New York"),
            (2, "  United States"),
            (3, ""),
            (4, "  VPN IP      192.0.2.100"),
            (5, "  Server      United States #8373"),
            (6, "  Technology  NORDLYNX · UDP"),
            (7, ""),
            (8, "  Uptime      04:10"),
            (9, "  Transfer    ↓ 98.95 MiB   ↑ 15.96 MiB"),
        ] {
            assert_eq!(row_text(&terminal, row).trim_end(), expected);
        }
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 1)].fg, BLUE);
        assert!(buffer[(2, 1)].modifier.contains(Modifier::BOLD));
        for (row, color) in [(4, TEAL), (5, TEXT), (6, LAVENDER), (8, TEAL), (9, GREEN)] {
            assert_eq!(buffer[(2, row)].fg, GRAY);
            assert_eq!(buffer[(14, row)].fg, color);
        }
        assert_eq!(buffer[(28, 9)].fg, RED);
        assert_eq!(buffer[(16, 9)].fg, TEXT);
        assert_eq!(buffer[(30, 9)].fg, TEXT);
        assert!(buffer.content().iter().all(|cell| cell.bg == Color::Reset));
        assert!(row_text(&terminal, 10).starts_with('─'));
        assert!(row_text(&terminal, 11).ends_with("⦁ Connected "));
        assert!(row_text(&terminal, 11)[..66].trim().is_empty());
        assert!(!terminal.backend().cursor_visible());
    }

    #[test]
    fn session_clock_changes_format_at_an_hour_and_keeps_total_hours() {
        for (seconds, expected) in [
            (0, "00:00"),
            (14, "00:14"),
            (250, "04:10"),
            (3_599, "59:59"),
            (3_600, "01:00:00"),
            (3_661, "01:01:01"),
            (86_400, "24:00:00"),
            (90_061, "25:01:01"),
        ] {
            assert_eq!(format_uptime(Duration::from_secs(seconds)), expected);
        }
    }

    #[test]
    fn missing_session_fields_reserve_rows_without_inventing_values() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(connected_with_session());
        let mut terminal = terminal(&mut app, 80, 12);
        for (uptime, received, sent, expected_uptime, expected_transfer) in [
            (Some(250), None, None, "04:10", ""),
            (None, Some("1 MiB"), Some("2 KiB"), "", "↓ 1 MiB   ↑ 2 KiB"),
            (None, Some("1 MiB"), None, "", "↓ 1 MiB"),
            (None, None, Some("2 KiB"), "", "↑ 2 KiB"),
            (Some(250), Some(" "), Some(""), "04:10", ""),
        ] {
            let mut snapshot = connected_with_session();
            let vpn = snapshot.vpn.as_mut().unwrap();
            vpn.uptime = uptime.map(Duration::from_secs);
            vpn.received = received.map(str::to_owned);
            vpn.sent = sent.map(str::to_owned);
            app.set_snapshot(snapshot);
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            assert_eq!(row_containing(&terminal, "New York"), 1);
            assert_eq!(row_containing(&terminal, "Technology"), 6);
            for (row, label, value) in [
                (8, "Uptime", expected_uptime),
                (9, "Transfer", expected_transfer),
            ] {
                let row = row_text(&terminal, row);
                if value.is_empty() {
                    assert!(row.trim().is_empty());
                } else {
                    assert_eq!(row.trim_end(), format!("  {label:<10}  {value}"));
                }
            }
            assert!(!text(&terminal).contains("00:00"));
            assert!(!text(&terminal).contains("0 B"));
        }
        for (ip, server, technology, protocol) in [
            (Some("192.0.2.100"), None, Some("NORDLYNX"), Some("UDP")),
            (None, Some("United States #8373"), None, Some("UDP")),
            (None, None, None, None),
        ] {
            let mut snapshot = connected_with_session();
            let vpn = snapshot.vpn.as_mut().unwrap();
            vpn.ip = ip.map(str::to_owned);
            vpn.server = server.map(str::to_owned);
            vpn.technology = technology.map(str::to_owned);
            vpn.protocol = protocol.map(str::to_owned);
            app.set_snapshot(snapshot);
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            assert_eq!(row_containing(&terminal, "New York"), 1);
            assert_eq!(row_containing(&terminal, "Uptime"), 8);
            assert_eq!(row_containing(&terminal, "Transfer"), 9);
            if server.is_none() {
                assert!(row_text(&terminal, 5).trim().is_empty());
            }
            if technology.is_none() && protocol.is_some() {
                assert_eq!(row_text(&terminal, 6).trim_end(), "  Technology  UDP");
            }
        }
        app.set_snapshot(connected());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!text(&terminal).contains("Uptime"));
        assert!(!text(&terminal).contains("Transfer"));
        assert_eq!(row_containing(&terminal, "New York"), 3);
    }

    #[test]
    fn successive_session_readings_change_only_values_on_the_same_terminal() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(connected_with_session());
        let mut terminal = terminal(&mut app, 80, 12);
        let initial = terminal.backend().buffer().clone();
        let mut updated = connected_with_session();
        let vpn = updated.vpn.as_mut().unwrap();
        vpn.uptime = Some(Duration::from_secs(3_600));
        vpn.received = Some("1.01 GiB".into());
        vpn.sent = Some("2 KiB".into());
        app.set_snapshot(updated);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let current = terminal.backend().buffer();
        for row in 0..12 {
            for column in 0..80 {
                if ![8, 9].contains(&row) || column < 14 {
                    assert_eq!(current[(column, row)], initial[(column, row)]);
                }
            }
        }
        assert_eq!(row_text(&terminal, 8).trim_end(), "  Uptime      01:00:00");
        assert_eq!(
            row_text(&terminal, 9).trim_end(),
            "  Transfer    ↓ 1.01 GiB   ↑ 2 KiB"
        );
        assert!(!text(&terminal).contains("15.96"));
        assert!(!text(&terminal).contains("Refreshing"));
    }

    #[test]
    fn session_values_clip_whole_graphemes_and_leave_footer_visible() {
        let mut snapshot = connected_with_session();
        let vpn = snapshot.vpn.as_mut().unwrap();
        vpn.city = Some("東京🧑‍💻東京🧑‍💻東京🧑‍💻東京".into());
        vpn.server = Some("🧑‍💻東京e\u{301}東京🧑‍💻東京".into());
        vpn.received = Some("123456789.99 MiB".into());
        vpn.sent = Some("987654321.99 MiB".into());
        let mut app = App::new(Vec::new());
        app.set_snapshot(snapshot);
        let terminal = terminal(&mut app, 24, 12);
        let body_width = 20;
        let lines = panel_lines(&app, body_width);
        assert!(
            lines
                .iter()
                .all(|line| line.width() <= usize::from(body_width))
        );
        let server_value = &lines[4].spans[1].content;
        assert_eq!(server_value, "🧑‍💻東京e\u{301}…");
        assert_eq!(Line::from(server_value.as_ref()).width(), 8);
        assert!(!text(&terminal).contains('�'));
        assert!(row_text(&terminal, 9).trim_end().ends_with('…'));
        assert!(row_text(&terminal, 10).starts_with('─'));
        assert!(row_text(&terminal, 11).ends_with("⦁ Connected "));
    }

    #[test]
    fn pending_disconnect_freezes_session_until_confirmed_and_then_clears_it() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(connected_with_session());
        let mut terminal = terminal(&mut app, 80, 12);
        let initial = panel_cells(&terminal);
        app.busy = Some(Action::Disconnect);
        let mut latest = connected_with_session();
        latest.vpn.as_mut().unwrap().uptime = Some(Duration::from_secs(251));
        app.set_snapshot(latest.clone());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), initial);
        latest.vpn.as_mut().unwrap().state = VpnState::Disconnected;
        app.set_snapshot(latest.clone());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), initial);
        app.busy = None;
        app.set_snapshot(latest);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(text(&terminal).contains("Ready to connect"));
        for stale in [
            "Uptime", "Transfer", "04:10", "MiB", "Server", "8373", "UDP",
        ] {
            assert!(!text(&terminal).contains(stale));
        }
        app.set_snapshot(connected_with_session());
        app.set_snapshot(Snapshot {
            service: ServiceState::Inactive,
            vpn: None,
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(text(&terminal).contains("VPN is off"));
        assert!(!text(&terminal).contains("Uptime"));
        assert!(!text(&terminal).contains("Transfer"));
    }

    #[test]
    fn stopped_and_disconnected_panels_have_useful_empty_states_without_dash_tables() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(Snapshot {
            service: ServiceState::Inactive,
            vpn: None,
        });
        let stopped = terminal(&mut app, 80, 12);
        for expected in [
            "VPN is off",
            "Connect when you need it.",
            "Start service, then open search to load locations.",
            "Service stopped",
        ] {
            assert!(text(&stopped).contains(expected));
        }
        app.set_locations(vec![country()]);
        assert!(
            !text(&terminal(&mut app, 80, 12))
                .contains("Start service, then open search to load locations.")
        );
        let mut snapshot = connected();
        snapshot.vpn.as_mut().unwrap().state = VpnState::Disconnected;
        app.set_snapshot(snapshot);
        let disconnected = terminal(&mut app, 80, 12);
        assert!(text(&disconnected).contains("Ready to connect"));
        assert!(
            text(&disconnected).contains("Choose a location or connect to a recommended server.")
        );
        for panel in [&stopped, &disconnected] {
            for absent in ["—", "VPN IP", "Technology", "192.0.2.100", "New York"] {
                assert!(!text(panel).contains(absent));
            }
        }
    }

    #[test]
    fn panel_hides_stale_connection_details_outside_connected_and_paused_states() {
        for (service, state) in [
            (ServiceState::Inactive, VpnState::Connected),
            (ServiceState::Activating, VpnState::Connected),
            (ServiceState::Deactivating, VpnState::Connected),
            (ServiceState::Failed, VpnState::Connected),
            (
                ServiceState::Unavailable("error".into()),
                VpnState::Connected,
            ),
            (ServiceState::Active, VpnState::Disconnected),
            (ServiceState::Active, VpnState::Connecting),
            (ServiceState::Active, VpnState::Unavailable("error".into())),
        ] {
            let mut snapshot = connected();
            snapshot.service = service;
            snapshot.vpn.as_mut().unwrap().state = state;
            let mut app = App::new(Vec::new());
            app.set_snapshot(snapshot);
            let terminal = terminal(&mut app, 80, 12);
            for stale in [
                "New York",
                "United States",
                "192.0.2.100",
                "NORDLYNX",
                "VPN IP",
                "Technology",
            ] {
                assert!(
                    !text(&terminal).contains(stale),
                    "stale {stale} shown for {:?}",
                    app.snapshot
                );
            }
            assert!(!text(&terminal).contains('—'));
        }
    }

    #[test]
    fn partial_locations_and_metadata_use_only_available_values_including_paused() {
        for (country, city, headline) in [
            (Some("Japan"), None, "Japan"),
            (None, Some("New_York"), "New York"),
            (None, None, "VPN paused"),
            (Some("  "), Some(""), "VPN paused"),
        ] {
            let mut snapshot = connected();
            let vpn = snapshot.vpn.as_mut().unwrap();
            vpn.state = VpnState::Paused;
            vpn.country = country.map(str::to_owned);
            vpn.city = city.map(str::to_owned);
            vpn.ip = None;
            let mut app = App::new(Vec::new());
            app.set_snapshot(snapshot);
            let terminal = terminal(&mut app, 80, 12);
            let hero_y = row_containing(&terminal, headline);
            assert_eq!(terminal.backend().buffer()[(2, hero_y)].fg, BLUE);
            assert!(text(&terminal).contains("Technology  NORDLYNX"));
            assert!(!text(&terminal).contains("VPN IP"));
            assert!(!text(&terminal).contains('—'));
        }
        let mut snapshot = connected();
        let vpn = snapshot.vpn.as_mut().unwrap();
        vpn.city = None;
        vpn.country = None;
        vpn.ip = Some("  ".into());
        vpn.technology = None;
        let mut app = App::new(Vec::new());
        app.set_snapshot(snapshot);
        let terminal = terminal(&mut app, 80, 12);
        assert!(text(&terminal).contains("Connection active"));
        assert!(!text(&terminal).contains("VPN IP"));
        assert!(!text(&terminal).contains("Technology"));
    }

    #[test]
    fn panel_balances_vertical_space_and_search_uses_the_same_body_inset() {
        let mut app = App::new(vec![country()]);
        app.set_snapshot(connected());
        let short = terminal(&mut app, 80, 12);
        let tall = terminal(&mut app, 80, 24);
        assert!(row_containing(&short, "New York") >= 2);
        assert_eq!(row_containing(&tall, "New York"), 4);
        app.open_search();
        let mut search = terminal(&mut app, 80, 12);
        assert!(row_text(&search, 0).trim().is_empty());
        assert_eq!(search.backend().buffer()[(2, 1)].symbol(), "›");
        assert_eq!(search.backend().buffer()[(2, 1)].fg, BLUE);
        search.backend_mut().assert_cursor_position((4, 1));
        assert!(row_text(&search, 1).contains("Search countries and cities…"));
        for (y, range) in [(2, 2..78), (10, 0..80)] {
            for x in range {
                assert_eq!(search.backend().buffer()[(x, y)].symbol(), "─");
                assert_eq!(search.backend().buffer()[(x, y)].fg, GRAY);
            }
        }
    }

    #[test]
    fn search_rows_use_blue_matches_gray_types_and_inset_selected_background() {
        let mut app = App::new(Vec::new());
        app.open_search();
        app.results = vec![
            SearchResult {
                label: "Japan".into(),
                kind: ItemKind::Country,
                action: Action::Connect(Some(country())),
                matches: vec![0],
            },
            SearchResult {
                label: "Tokyo / Japan".into(),
                kind: ItemKind::City,
                action: Action::Connect(Some(Location {
                    country: "Japan".into(),
                    city: Some("Tokyo".into()),
                })),
                matches: Vec::new(),
            },
        ];
        app.list_state.select(Some(0));
        let terminal = terminal(&mut app, 80, 12);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(4, 3)].fg, BLUE);
        assert_eq!(buffer[(5, 3)].fg, TEXT);
        assert_eq!(buffer[(4, 4)].fg, TEXT);
        assert_eq!(buffer[(71, 3)].fg, GRAY);
        assert_eq!(buffer[(74, 4)].fg, GRAY);
        for y in 0..12 {
            for x in 0..80 {
                assert_eq!(
                    buffer[(x, y)].bg,
                    if y == 3 && (2..78).contains(&x) {
                        SELECTED_BACKGROUND
                    } else {
                        Color::Reset
                    }
                );
            }
        }
    }

    #[test]
    fn all_views_share_compact_footer_with_gray_status_right_pad_and_state_color() {
        for mode in [Mode::Panel, Mode::Search, Mode::Help] {
            for (service, state, color) in [
                (ServiceState::Active, Some(VpnState::Connected), GREEN),
                (ServiceState::Active, Some(VpnState::Disconnected), RED),
                (ServiceState::Active, Some(VpnState::Connecting), YELLOW),
                (ServiceState::Active, Some(VpnState::Paused), YELLOW),
                (ServiceState::Inactive, None, GRAY),
                (ServiceState::Activating, None, GRAY),
                (ServiceState::Deactivating, None, GRAY),
                (ServiceState::Failed, None, GRAY),
                (ServiceState::Unavailable("error".into()), None, GRAY),
            ] {
                let mut app = App::new(Vec::new());
                app.mode = mode;
                app.set_snapshot(Snapshot {
                    service,
                    vpn: state.map(|state| VpnStatus {
                        state,
                        country: Some("Japan".into()),
                        city: None,
                        ip: None,
                        technology: None,
                        server: None,
                        protocol: None,
                        uptime: None,
                        received: None,
                        sent: None,
                    }),
                });
                let status_width = u16::try_from(Line::from(app.status_text()).width()).unwrap();
                let circle_x = 79 - status_width - 2;
                let terminal = terminal(&mut app, 80, 12);
                let buffer = terminal.backend().buffer();
                assert_eq!(buffer[(circle_x, 11)].symbol(), "⦁");
                assert_eq!(buffer[(circle_x, 11)].fg, color);
                for x in circle_x + 2..79 {
                    assert_eq!(buffer[(x, 11)].fg, GRAY);
                }
                assert_eq!(buffer[(79, 11)].symbol(), " ");
                assert_ne!(buffer[(78, 11)].symbol(), " ");
                let footer = row_text(&terminal, 11);
                if app.is_working() {
                    assert!(SPINNER_FRAMES.contains(&buffer[(2, 11)].symbol()));
                    assert_eq!(buffer[(2, 11)].fg, GRAY);
                    assert!(footer.contains('…'));
                } else {
                    assert!(footer[..usize::from(circle_x)].trim().is_empty());
                }
                assert!(!footer.contains("Japan"));
            }
        }
    }

    #[test]
    fn busy_loading_and_notices_share_left_footer_without_overlapping_status() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(connected());
        app.set_notice("Refreshed", false);
        app.loading = Some((3, 50));
        app.busy = Some(Action::Connect(Some(Location {
            country: "Japan".into(),
            city: Some("東京".into()),
        })));
        for width in [24, 32, 40, 80] {
            let terminal = terminal(&mut app, width, 12);
            let buffer = terminal.backend().buffer();
            let circle_x = width - 12;
            assert_eq!(buffer[(circle_x, 11)].symbol(), "⦁");
            assert_eq!(buffer[(circle_x, 11)].fg, GREEN);
            assert!(row_text(&terminal, 11).ends_with("⦁ Connected "));
            assert_eq!(buffer[(width - 1, 11)].symbol(), " ");
            assert_eq!(buffer[(circle_x - 1, 11)].symbol(), " ");
            assert_eq!(buffer[(circle_x - 2, 11)].symbol(), " ");
            let left = (2..circle_x - 2)
                .map(|x| buffer[(x, 11)].symbol())
                .collect::<String>();
            assert!(SPINNER_FRAMES.contains(&buffer[(2, 11)].symbol()));
            assert_eq!(buffer[(2, 11)].fg, GRAY);
            assert!(left[buffer[(2, 11)].symbol().len()..].starts_with(" Conn"));
            if width < 80 {
                assert!(left.trim_end().ends_with('…'));
            }
        }
        let wide = terminal(&mut app, 80, 12);
        assert!(row_text(&wide, 11).contains("Connecting to 東"));
        assert!(!row_text(&wide, 11).contains("Loading"));
        assert!(!row_text(&wide, 11).contains("Refreshed"));
        app.busy = None;
        let loading = terminal(&mut app, 80, 12);
        assert!(SPINNER_FRAMES.contains(&loading.backend().buffer()[(2, 11)].symbol()));
        assert_eq!(loading.backend().buffer()[(2, 11)].fg, GRAY);
        assert!(
            row_text(&terminal(&mut app, 80, 12), 11).contains("Loading cities · 3/50 countries")
        );
        app.loading = Some((0, 0));
        assert!(row_text(&terminal(&mut app, 80, 12), 11).contains("Loading countries…"));
        app.loading = None;
        let complete = terminal(&mut app, 80, 12);
        assert!(row_text(&complete, 11).starts_with("  Refreshed"));
        app.set_notice("接続が完了しました 接続が完了しました", false);
        let narrow = terminal(&mut app, 32, 12);
        assert!(row_text(&narrow, 11).contains('接'));
        assert!(row_text(&narrow, 11).contains('…'));
        assert!(row_text(&narrow, 11).ends_with("⦁ Connected "));
    }

    #[test]
    fn narrow_service_status_keeps_circle_and_drops_redundant_left_content() {
        let mut app = App::new(Vec::new());
        for service in [
            ServiceState::Inactive,
            ServiceState::Activating,
            ServiceState::Unavailable("error".into()),
        ] {
            app.set_snapshot(Snapshot { service, vpn: None });
            app.set_notice("A long operation finished", false);
            let terminal = terminal(&mut app, 24, 5);
            let footer = row_text(&terminal, 4);
            assert!(footer.ends_with(&format!("⦁ {} ", app.status_text())));
            let circle_x = (0..24)
                .find(|x| terminal.backend().buffer()[(*x, 4)].symbol() == "⦁")
                .unwrap();
            assert_eq!(terminal.backend().buffer()[(circle_x, 4)].fg, GRAY);
            assert_eq!(terminal.backend().buffer()[(23, 4)].symbol(), " ");
        }
    }

    #[test]
    fn error_has_conditional_red_row_above_separator_and_does_not_replace_activity() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(connected());
        let normal = terminal(&mut app, 80, 12);
        assert!(row_text(&normal, 9).trim().is_empty());
        app.busy = Some(Action::Refresh);
        app.set_notice("Cannot read the connection\nPlease retry", true);
        let error = terminal(&mut app, 80, 12);
        assert_eq!(panel_cells(&error), panel_cells(&normal));
        assert_eq!(
            row_text(&error, 9).trim_end(),
            "  Cannot read the connection Please retry"
        );
        assert_eq!(error.backend().buffer()[(2, 9)].fg, RED);
        assert!(row_text(&error, 10).starts_with('─'));
        assert!(row_text(&error, 11).contains("Refreshing…"));
        assert!(row_text(&error, 11).ends_with("⦁ Connected "));
        let narrow = terminal(&mut app, 24, 5);
        assert_eq!(narrow.backend().buffer()[(2, 2)].fg, RED);
        assert!(row_text(&narrow, 2).trim_end().ends_with('…'));
        app.notice = None;
        let cleared = terminal(&mut app, 80, 12);
        assert!(!text(&cleared).contains("Cannot read"));
        assert!(row_text(&cleared, 9).trim().is_empty());
    }

    #[test]
    fn connecting_and_disconnecting_keep_confirmed_body_until_a_final_status_read() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(connected());
        let mut terminal = terminal(&mut app, 80, 12);
        let initial = panel_cells(&terminal);
        app.busy = Some(Action::Connect(Some(Location {
            country: "Japan".into(),
            city: Some("Tokyo".into()),
        })));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), initial);
        assert!(row_text(&terminal, 11).contains("Connecting to Tokyo / Japan…"));
        assert_eq!(terminal.backend().buffer()[(4, 11)].fg, GRAY);

        let mut intermediate = connected();
        intermediate.vpn.as_mut().unwrap().state = VpnState::Connecting;
        app.set_snapshot(intermediate);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), initial);
        assert!(row_text(&terminal, 11).ends_with("⦁ Connecting… "));

        let mut tokyo = connected();
        let vpn = tokyo.vpn.as_mut().unwrap();
        vpn.city = Some("Tokyo".into());
        vpn.country = Some("Japan".into());
        vpn.ip = Some("192.0.2.200".into());
        app.set_snapshot(tokyo.clone());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), initial);
        app.busy = None;
        app.set_notice("Connected", false);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), initial);
        app.set_snapshot(tokyo.clone());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(text(&terminal).contains("Tokyo"));
        assert!(text(&terminal).contains("Japan"));
        assert!(text(&terminal).contains("192.0.2.200"));
        assert!(!text(&terminal).contains("New York"));
        let tokyo_body = panel_cells(&terminal);

        app.busy = Some(Action::Disconnect);
        tokyo.vpn.as_mut().unwrap().state = VpnState::Disconnected;
        app.set_snapshot(tokyo.clone());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), tokyo_body);
        assert!(row_text(&terminal, 11).contains("Disconnecting…"));
        app.busy = None;
        app.set_notice("Disconnected", false);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), tokyo_body);
        app.set_snapshot(tokyo);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(text(&terminal).contains("Ready to connect"));
        for old in ["Tokyo", "Japan", "192.0.2.200", "VPN IP", "Technology"] {
            assert!(!text(&terminal).contains(old));
        }
    }

    #[test]
    fn initial_panel_is_blank_and_uses_a_gray_footer_spinner_until_confirmed() {
        let mut app = App::new(Vec::new());
        let mut terminal = terminal(&mut app, 80, 12);
        for row in 0..10 {
            assert!(row_text(&terminal, row).trim().is_empty());
        }
        assert!(SPINNER_FRAMES.contains(&terminal.backend().buffer()[(2, 11)].symbol()));
        assert_eq!(terminal.backend().buffer()[(2, 11)].fg, GRAY);
        assert!(row_text(&terminal, 11).ends_with("⦁ Checking… "));
        let circle = row_text(&terminal, 11).find('⦁').unwrap();
        let activity = row_text(&terminal, 11)[..circle].trim().to_owned();
        assert!(SPINNER_FRAMES.contains(&activity.as_str()));

        app.set_snapshot(Snapshot {
            service: ServiceState::Activating,
            vpn: None,
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        for row in 0..10 {
            assert!(row_text(&terminal, row).trim().is_empty());
        }
        assert!(row_text(&terminal, 11).contains("Starting service…"));
        assert_eq!(terminal.backend().buffer()[(4, 11)].fg, GRAY);
        app.set_snapshot(Snapshot {
            service: ServiceState::Inactive,
            vpn: None,
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(text(&terminal).contains("VPN is off"));
        assert!(row_text(&terminal, 11).ends_with("⦁ Service stopped "));
        assert!(row_text(&terminal, 11).starts_with("  "));
        assert!(!SPINNER_FRAMES.contains(&terminal.backend().buffer()[(2, 11)].symbol()));
    }

    #[test]
    fn service_actions_and_failed_observations_leave_confirmed_panel_in_place() {
        let mut app = App::new(vec![country()]);
        app.set_snapshot(Snapshot {
            service: ServiceState::Inactive,
            vpn: None,
        });
        let mut terminal = terminal(&mut app, 80, 12);
        let off_body = panel_cells(&terminal);
        app.busy = Some(Action::StartService);
        app.set_snapshot(Snapshot {
            service: ServiceState::Activating,
            vpn: None,
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), off_body);
        assert!(row_text(&terminal, 11).contains("Starting service…"));
        app.busy = None;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), off_body);
        let mut ready = connected();
        ready.vpn.as_mut().unwrap().state = VpnState::Disconnected;
        app.set_snapshot(ready);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(text(&terminal).contains("Ready to connect"));
        let ready_body = panel_cells(&terminal);

        for snapshot in [
            Snapshot {
                service: ServiceState::Unavailable("Read failed".into()),
                vpn: None,
            },
            Snapshot {
                service: ServiceState::Active,
                vpn: None,
            },
            Snapshot {
                service: ServiceState::Active,
                vpn: Some(VpnStatus::unavailable("Read failed")),
            },
        ] {
            app.set_snapshot(snapshot);
            app.set_notice("Could not read status", true);
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            assert_eq!(panel_cells(&terminal), ready_body);
            assert!(row_text(&terminal, 9).contains("Could not read status"));
            assert_eq!(terminal.backend().buffer()[(2, 9)].fg, RED);
        }
        app.notice = None;
        app.busy = Some(Action::StopService);
        app.set_snapshot(Snapshot {
            service: ServiceState::Deactivating,
            vpn: None,
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), ready_body);
        assert!(row_text(&terminal, 11).contains("Stopping service…"));
        app.busy = None;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), ready_body);
        app.set_snapshot(Snapshot {
            service: ServiceState::Inactive,
            vpn: None,
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(panel_cells(&terminal), off_body);
    }

    #[test]
    fn changing_modes_on_same_terminal_clears_content_background_and_cursor() {
        let mut app = App::new(vec![country()]);
        app.set_snapshot(connected());
        let mut terminal = terminal(&mut app, 80, 12);
        assert!(text(&terminal).contains("Technology"));
        app.open_search();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(terminal.backend().cursor_visible());
        assert!(text(&terminal).contains("Search countries and cities…"));
        assert!(!text(&terminal).contains("Technology"));
        assert!(!text(&terminal).contains("192.0.2.100"));
        app.mode = Mode::Panel;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!terminal.backend().cursor_visible());
        assert!(!text(&terminal).contains("Search countries and cities…"));
        assert!(text(&terminal).contains("New York"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.bg == Color::Reset)
        );
    }

    #[test]
    fn shortcut_help_preserves_footer_styles_and_clears_when_dismissed() {
        let mut app = App::new(vec![country()]);
        app.set_snapshot(connected());
        let mut terminal = terminal(&mut app, 80, 14);
        assert!(text(&terminal).contains("New York"));
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.mode, Mode::Help);
        assert!(text(&terminal).contains("Shortcuts"));
        assert!(text(&terminal).contains("? / Esc"));
        assert!(text(&terminal).contains("Back to panel"));
        assert!(text(&terminal).contains("Scroll help"));
        assert!(!text(&terminal).contains("New York"));
        assert!(!text(&terminal).contains("VPN IP"));
        assert!(!terminal.backend().cursor_visible());
        let key_row = row_containing(&terminal, "i or /");
        assert_eq!(terminal.backend().buffer()[(2, key_row)].fg, BLUE);
        assert_eq!(terminal.backend().buffer()[(20, key_row)].fg, TEXT);
        assert!(row_text(&terminal, 13).ends_with("⦁ Connected "));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.bg == Color::Reset)
        );

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.mode, Mode::Panel);
        assert!(text(&terminal).contains("New York"));
        assert!(!text(&terminal).contains("Shortcuts"));
        assert!(!terminal.backend().cursor_visible());

        app.open_search();
        app.paste("?");
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(row_text(&terminal, 1).starts_with("  › ?"));
        assert!(!text(&terminal).contains("Shortcuts"));
        assert!(terminal.backend().cursor_visible());
    }

    #[test]
    fn help_scrolls_wrapped_content_and_clamps_after_a_wider_taller_resize() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(connected());
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        let mut small = terminal(&mut app, 24, 8);
        assert!(text(&small).contains("Shortcuts"));
        let first = text(&small);
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        small.draw(|frame| render(frame, &mut app)).unwrap();
        assert_ne!(text(&small), first);
        assert_eq!(app.help_scroll, 10);
        assert!(!small.backend().cursor_visible());
        assert!(row_text(&small, 7).ends_with("⦁ Connected "));

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        small.draw(|frame| render(frame, &mut app)).unwrap();
        let small_maximum = help_lines(20).len() - 5;
        assert_eq!(usize::from(app.help_scroll), small_maximum);
        assert!(text(&small).contains("Home / End"));
        assert!(text(&small).contains("First or last page"));

        let larger = terminal(&mut app, 80, 24);
        let larger_maximum = help_lines(76).len().saturating_sub(21);
        assert_eq!(usize::from(app.help_scroll), larger_maximum);
        assert!(text(&larger).contains("Home / End"));
        assert!(!larger.backend().cursor_visible());
        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        let full = terminal(&mut app, 80, 50);
        assert_eq!(app.help_scroll, 0);
        for key in [
            "Ctrl-A / Home",
            "Ctrl-E / End",
            "Ctrl-J/K",
            "Ctrl-N/P",
            "Ctrl-U",
            "Ctrl-R",
            "Ctrl-C",
        ] {
            assert!(text(&full).contains(key));
        }
    }

    #[test]
    fn narrow_unicode_query_viewport_follows_beginning_end_and_grapheme_cursor() {
        let mut app = App::new(vec![country()]);
        app.open_search();
        let original = "e\u{301}東京01234567890123456789🏳️‍🌈終";
        app.paste(original);
        let mut terminal = terminal(&mut app, 24, 8);
        let end_view = row_text(&terminal, 1);
        terminal.backend_mut().assert_cursor_position((21, 1));
        assert!(!end_view.contains("e\u{301}"));
        assert!(end_view.contains("7890123456789🏳️‍🌈"));
        assert!(end_view.contains('終'));

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        terminal.backend_mut().assert_cursor_position((4, 1));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(4, 1)].symbol(), "e\u{301}");
        assert_eq!(buffer[(5, 1)].symbol(), "東");
        assert_eq!(buffer[(7, 1)].symbol(), "京");
        assert!(!row_text(&terminal, 1).contains('終'));

        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        terminal.backend_mut().assert_cursor_position((5, 1));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        terminal.backend_mut().assert_cursor_position((7, 1));

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        terminal.backend_mut().assert_cursor_position((21, 1));
        assert_eq!(row_text(&terminal, 1), end_view);
        assert_eq!(app.query, original);
        assert_eq!(
            query_viewport("United States", 10, 17),
            ("United States".into(), 10)
        );
        assert_eq!(
            query_viewport("e\u{301}東京🏳️‍🌈", 0, 5),
            ("e\u{301}東京".into(), 0)
        );
        assert_eq!(
            query_viewport("e\u{301}東京🏳️‍🌈", "e\u{301}東京🏳️‍🌈".len(), 5),
            ("京🏳️‍🌈".into(), 4)
        );
    }

    #[test]
    fn unicode_search_and_narrow_or_tiny_terminals_render_safely() {
        let mut app = App::new(vec![Location {
            country: "Japan".into(),
            city: Some("東京".into()),
        }]);
        app.set_snapshot(connected());
        let narrow = terminal(&mut app, 24, 5);
        assert!(text(&narrow).contains("New York"));
        assert!(text(&narrow).contains("Connected"));
        app.open_search();
        app.paste("東京");
        let mut search = terminal(&mut app, 40, 10);
        let search_text = text(&search);
        assert!(search_text.contains('東') && search_text.contains('京'));
        search.backend_mut().assert_cursor_position((8, 1));
        assert!(terminal(&mut app, 24, 5).backend().cursor_visible());
        assert!(text(&terminal(&mut app, 10, 2)).contains("Resize"));
        assert_eq!(truncate("東京🏳️‍🌈hello", 5), "東京…");
        assert_eq!(truncate("e\u{301}clair", 3), "e\u{301}c…");
    }
}
