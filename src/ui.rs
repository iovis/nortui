use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::{App, Mode, SearchResult};
use crate::model::{Action, ServiceState, VpnState, VpnStatus};

const TEXT: Color = Color::Rgb(198, 208, 245);
const GRAY: Color = Color::Rgb(131, 139, 167);
const BLUE: Color = Color::Rgb(140, 170, 238);
const GREEN: Color = Color::Rgb(166, 209, 137);
const RED: Color = Color::Rgb(231, 130, 132);
const YELLOW: Color = Color::Rgb(229, 200, 144);
const SELECTED_BACKGROUND: Color = Color::Rgb(35, 38, 52);
const INSET: u16 = 2;

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
        Mode::Panel => render_panel(frame, app, body),
        Mode::Search => render_search(frame, app, body),
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

fn render_panel(frame: &mut Frame, app: &App, area: Rect) {
    let lines = panel_lines(app, area.width);
    let line_count = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let upper_space = (area.height.saturating_sub(line_count) / 2).min(3);
    let panel = Rect::new(
        area.x,
        area.y + upper_space,
        area.width,
        area.height.saturating_sub(upper_space),
    );
    frame.render_widget(Paragraph::new(lines), panel);
}

fn panel_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    if let Some(action) = app
        .busy
        .as_ref()
        .filter(|action| **action != Action::Refresh)
    {
        let description = match action {
            Action::Connect(_) | Action::Reconnect => "Finding a VPN server.",
            Action::Disconnect => "Returning to your regular connection.",
            Action::StartService => "Getting ready to connect.",
            Action::StopService => "Closing the VPN connection.",
            Action::Refresh => unreachable!(),
        };
        return message_lines(&action.label(), description, width);
    }
    match &app.snapshot.service {
        ServiceState::Inactive => {
            let mut lines = message_lines("VPN is off", "Connect when you need it.", width);
            if app.locations.is_empty() {
                lines.push(Line::default());
                lines.extend(wrapped_lines(
                    "Start service to load locations",
                    width,
                    GRAY,
                ));
            }
            lines
        }
        ServiceState::Activating => {
            message_lines("Starting service…", "Getting ready to connect.", width)
        }
        ServiceState::Deactivating => {
            message_lines("Stopping service…", "Closing the VPN connection.", width)
        }
        ServiceState::Failed => {
            message_lines("Service needs attention", "Try starting it again.", width)
        }
        ServiceState::Unavailable(message) => {
            if message == "Checking service…" {
                message_lines(
                    "Checking connection…",
                    "Reading the current service state.",
                    width,
                )
            } else {
                message_lines("Service unavailable", "Refresh to check again.", width)
            }
        }
        ServiceState::Active => {
            let Some(vpn) = &app.snapshot.vpn else {
                return message_lines(
                    "Checking connection…",
                    "Reading the current VPN status.",
                    width,
                );
            };
            match vpn.state {
                VpnState::Connected | VpnState::Paused => connection_lines(vpn, width),
                VpnState::Disconnected => message_lines(
                    "Ready to connect",
                    "Choose a location or connect to a recommended server.",
                    width,
                ),
                VpnState::Connecting => {
                    message_lines("Connecting…", "Finding a VPN server.", width)
                }
                VpnState::Unavailable(_) => {
                    message_lines("Connection unavailable", "Refresh to check again.", width)
                }
            }
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
    let metadata: Vec<_> = [
        ("VPN IP", vpn.ip.as_deref()),
        ("Technology", vpn.technology.as_deref()),
    ]
    .into_iter()
    .filter_map(|(label, value)| {
        let value = value?.trim();
        (!value.is_empty()).then(|| metadata_line(label, value, width))
    })
    .collect();
    if !metadata.is_empty() {
        lines.push(Line::default());
        lines.extend(metadata);
    }
    lines
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

fn metadata_line(label: &str, value: &str, width: u16) -> Line<'static> {
    let label = format!("{label:<10}  ");
    let value_width =
        width.saturating_sub(u16::try_from(Line::from(label.as_str()).width()).unwrap_or(u16::MAX));
    Line::from(vec![
        Span::styled(label, Style::default().fg(GRAY)),
        Span::styled(truncate(value, value_width), Style::default().fg(TEXT)),
    ])
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
    let query = visible_query(&app.query, search.width.saturating_sub(3));
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
            Span::styled(query.clone(), Style::default().fg(TEXT))
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
    frame.set_cursor_position((
        search.x + 2 + u16::try_from(Line::from(query).width()).unwrap_or(0),
        search.y,
    ));
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
                    "Start service to load locations",
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
        frame.render_widget(
            Paragraph::new(truncate(&activity, left_width)).style(Style::default().fg(color)),
            Rect::new(area.x + INSET, area.y, left_width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("● ", Style::default().fg(status_color(app))),
            Span::styled(status, Style::default().fg(GRAY)),
        ])),
        status_area,
    );
}

fn footer_activity(app: &App) -> (String, Color) {
    if let Some(action) = &app.busy {
        return (action.label(), YELLOW);
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
    if let Some(notice) = app.notice.as_ref().filter(|notice| !notice.is_error) {
        return (notice.message.clone(), GRAY);
    }
    if app.snapshot.service == ServiceState::Active {
        return ("Service running".into(), GRAY);
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

fn visible_query(query: &str, width: u16) -> String {
    let mut size = 0;
    let mut start = query.len();
    for (index, grapheme) in query.grapheme_indices(true).rev() {
        let next_size = size + Line::from(grapheme).width();
        if next_size > usize::from(width) {
            break;
        }
        start = index;
        size = next_size;
    }
    query[start..].to_owned()
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
    use crate::model::{Location, Snapshot, VpnStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn terminal(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
    }

    fn row_text(terminal: &Terminal<TestBackend>, row: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, row)].symbol())
            .collect()
    }

    fn text(terminal: &Terminal<TestBackend>) -> String {
        (0..terminal.backend().buffer().area.height)
            .map(|row| row_text(terminal, row))
            .collect::<Vec<_>>()
            .join("\n")
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
            }),
        }
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
        assert_eq!(buffer[(14, hero_y + 3)].fg, TEXT);
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
            "Start service to load locations",
            "Service stopped",
        ] {
            assert!(text(&stopped).contains(expected));
        }
        app.set_locations(vec![country()]);
        assert!(!text(&terminal(&mut app, 80, 12)).contains("Start service to load locations"));
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
    fn both_modes_share_compact_footer_with_gray_status_right_pad_and_state_color() {
        for mode in [Mode::Panel, Mode::Search] {
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
                    }),
                });
                let status_width = u16::try_from(Line::from(app.status_text()).width()).unwrap();
                let circle_x = 79 - status_width - 2;
                let terminal = terminal(&mut app, 80, 12);
                let buffer = terminal.backend().buffer();
                assert_eq!(buffer[(circle_x, 11)].symbol(), "●");
                assert_eq!(buffer[(circle_x, 11)].fg, color);
                for x in circle_x + 2..79 {
                    assert_eq!(buffer[(x, 11)].fg, GRAY);
                }
                assert_eq!(buffer[(79, 11)].symbol(), " ");
                assert_ne!(buffer[(78, 11)].symbol(), " ");
                let footer = row_text(&terminal, 11);
                if app.snapshot.service == ServiceState::Active {
                    assert!(footer.starts_with("  Service running"));
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
            assert_eq!(buffer[(circle_x, 11)].symbol(), "●");
            assert_eq!(buffer[(circle_x, 11)].fg, GREEN);
            assert!(row_text(&terminal, 11).ends_with("● Connected "));
            assert_eq!(buffer[(width - 1, 11)].symbol(), " ");
            assert_eq!(buffer[(circle_x - 1, 11)].symbol(), " ");
            assert_eq!(buffer[(circle_x - 2, 11)].symbol(), " ");
            let left = (2..circle_x - 2)
                .map(|x| buffer[(x, 11)].symbol())
                .collect::<String>();
            assert!(left.starts_with("Connect"));
            if width < 80 {
                assert!(left.trim_end().ends_with('…'));
            }
        }
        let wide = terminal(&mut app, 80, 12);
        assert!(row_text(&wide, 11).contains("Connecting to 東"));
        assert!(!row_text(&wide, 11).contains("Loading"));
        assert!(!row_text(&wide, 11).contains("Refreshed"));
        app.busy = None;
        assert!(
            row_text(&terminal(&mut app, 80, 12), 11).contains("Loading cities · 3/50 countries")
        );
        app.loading = Some((0, 0));
        assert!(row_text(&terminal(&mut app, 80, 12), 11).contains("Loading countries…"));
        app.loading = None;
        assert!(row_text(&terminal(&mut app, 80, 12), 11).contains("Refreshed"));
        app.set_notice("接続が完了しました 接続が完了しました", false);
        let narrow = terminal(&mut app, 32, 12);
        assert!(row_text(&narrow, 11).contains('接'));
        assert!(row_text(&narrow, 11).contains('…'));
        assert!(row_text(&narrow, 11).ends_with("● Connected "));
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
            assert!(footer.ends_with(&format!("● {} ", app.status_text())));
            let circle_x = (0..24)
                .find(|x| terminal.backend().buffer()[(*x, 4)].symbol() == "●")
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
        assert_eq!(
            row_text(&error, 9).trim_end(),
            "  Cannot read the connection Please retry"
        );
        assert_eq!(error.backend().buffer()[(2, 9)].fg, RED);
        assert!(row_text(&error, 10).starts_with('─'));
        assert!(row_text(&error, 11).contains("Refreshing…"));
        assert!(row_text(&error, 11).ends_with("● Connected "));
        let narrow = terminal(&mut app, 24, 5);
        assert_eq!(narrow.backend().buffer()[(2, 2)].fg, RED);
        assert!(row_text(&narrow, 2).trim_end().ends_with('…'));
        app.notice = None;
        let cleared = terminal(&mut app, 80, 12);
        assert!(!text(&cleared).contains("Cannot read"));
        assert!(row_text(&cleared, 9).trim().is_empty());
    }

    #[test]
    fn pending_location_is_identified_as_connecting_without_old_connection_metadata() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(connected());
        app.busy = Some(Action::Connect(Some(Location {
            country: "Japan".into(),
            city: Some("Tokyo".into()),
        })));
        let terminal = terminal(&mut app, 80, 12);
        assert!(text(&terminal).contains("Connecting to Tokyo / Japan…"));
        assert!(text(&terminal).contains("Finding a VPN server."));
        for stale in ["New York", "United States", "192.0.2.100", "NORDLYNX"] {
            assert!(!text(&terminal).contains(stale));
        }
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
