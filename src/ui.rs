use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::{App, Mode, SearchResult};
use crate::model::{ServiceState, VpnState};

const TEXT: Color = Color::Rgb(198, 208, 245);
const GRAY: Color = Color::Rgb(131, 139, 167);
const BLUE: Color = Color::Rgb(140, 170, 238);
const GREEN: Color = Color::Rgb(166, 209, 137);
const RED: Color = Color::Rgb(231, 130, 132);
const YELLOW: Color = Color::Rgb(229, 200, 144);
const SELECTED_BACKGROUND: Color = Color::Rgb(35, 38, 52);

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.height < 5 || area.width < 24 {
        frame.render_widget(
            Paragraph::new("Resize to at least 24 × 5").style(Style::default().fg(GRAY)),
            area,
        );
        return;
    }
    let [content, separator, notice, footer] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    match app.mode {
        Mode::Panel => render_panel(frame, app, content),
        Mode::Search => render_search(frame, app, content),
    }
    frame.render_widget(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(GRAY)),
        separator,
    );
    let (message, color) = notice_text(app);
    frame.render_widget(
        Paragraph::new(message).style(Style::default().fg(color)),
        notice,
    );
    render_status(frame, app, footer);
}

fn render_panel(frame: &mut Frame, app: &App, area: Rect) {
    let service = match app.snapshot.service {
        ServiceState::Active => "Running",
        ServiceState::Inactive => "Stopped",
        ServiceState::Activating => "Starting…",
        ServiceState::Deactivating => "Stopping…",
        ServiceState::Failed => "Failed",
        ServiceState::Unavailable(_) => "Unavailable",
    };
    let vpn = app.snapshot.vpn.as_ref().filter(|vpn| {
        app.snapshot.service == ServiceState::Active
            && matches!(vpn.state, VpnState::Connected | VpnState::Paused)
    });
    let location = vpn.map_or_else(String::new, |vpn| {
        [&vpn.city, &vpn.country]
            .into_iter()
            .flatten()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.replace('_', " "))
            .collect::<Vec<_>>()
            .join(", ")
    });
    let ip = vpn.and_then(|vpn| vpn.ip.as_deref()).unwrap_or_default();
    let technology = vpn
        .and_then(|vpn| vpn.technology.as_deref())
        .unwrap_or_default();
    let lines: Vec<_> = [
        ("Service", service),
        ("Location", location.as_str()),
        ("VPN IP", ip),
        ("Technology", technology),
    ]
    .into_iter()
    .map(|(label, value)| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{label:<10}  "), Style::default().fg(GRAY)),
            Span::styled(
                if value.trim().is_empty() {
                    "—"
                } else {
                    value
                },
                Style::default().fg(TEXT),
            ),
        ])
    })
    .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_search(frame: &mut Frame, app: &mut App, area: Rect) {
    let [search, results] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
    let search_text = if app.query.is_empty() {
        Line::from(vec![
            Span::styled("› ", Style::default().fg(BLUE)),
            Span::styled("Search countries and cities…", Style::default().fg(GRAY)),
        ])
    } else {
        Line::from(vec![
            Span::styled("› ", Style::default().fg(BLUE)),
            Span::raw(visible_query(&app.query, search.width.saturating_sub(3))),
        ])
    };
    frame.render_widget(
        Paragraph::new(search_text)
            .style(Style::default().fg(TEXT))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(GRAY)),
            ),
        search,
    );
    let cursor_width =
        Line::from(visible_query(&app.query, search.width.saturating_sub(3))).width();
    frame.set_cursor_position((
        search.x + 2 + u16::try_from(cursor_width).unwrap_or(0),
        search.y,
    ));
    if app.results.is_empty() {
        frame.render_widget(
            Paragraph::new(if app.query.trim().is_empty() {
                "  No cached locations"
            } else {
                "  No matches"
            })
            .style(Style::default().fg(GRAY)),
            results,
        );
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

fn render_status(frame: &mut Frame, app: &App, footer: Rect) {
    let status_area = Rect {
        width: footer.width.saturating_sub(1),
        ..footer
    };
    let status = app.status_text(true);
    let details = Line::from(status.as_str()).width() + 2 <= usize::from(status_area.width);
    let status = if details {
        status
    } else {
        app.status_text(false)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("● ", Style::default().fg(status_color(app))),
            Span::styled(status, Style::default().fg(GRAY)),
        ]))
        .alignment(Alignment::Right),
        status_area,
    );
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
    let label_width = Line::from(row.label.as_str()).width();
    let kind = row.kind.label();
    let padding = usize::from(width).saturating_sub(label_width + kind.len() + 4);
    let mut spans = Vec::new();
    for (index, grapheme) in row.label.graphemes(true).enumerate() {
        let matched =
            u32::try_from(index).is_ok_and(|index| row.matches.binary_search(&index).is_ok());
        let style = if matched {
            Style::default().fg(BLUE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        spans.push(Span::styled(grapheme.to_owned(), style));
    }
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::styled(kind, Style::default().fg(GRAY)));
    }
    ListItem::new(Line::from(spans))
}

fn notice_text(app: &App) -> (String, Color) {
    if let Some(action) = &app.busy {
        return (action.label(), YELLOW);
    }
    if let Some(notice) = &app.notice {
        return (
            notice.message.clone(),
            if notice.is_error { RED } else { GRAY },
        );
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
    if app.locations.is_empty() && app.snapshot.service == ServiceState::Inactive {
        return ("Start service to load locations".into(), GRAY);
    }
    (String::new(), GRAY)
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn terminal(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
    }

    fn text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn row_text(terminal: &Terminal<TestBackend>, row: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, row)].symbol())
            .collect()
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

    #[test]
    fn panel_shows_information_without_search_cursor_or_key_legends() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(connected());
        let terminal = terminal(&mut app, 80, 12);
        let buffer = terminal.backend().buffer();
        assert_eq!(app.mode, Mode::Panel);
        for (row, expected) in [
            "  Service     Running",
            "  Location    New York, United States",
            "  VPN IP      192.0.2.100",
            "  Technology  NORDLYNX",
        ]
        .into_iter()
        .enumerate()
        {
            assert!(row_text(&terminal, u16::try_from(row).unwrap()).starts_with(expected));
            assert_eq!(buffer[(2, u16::try_from(row).unwrap())].fg, GRAY);
            assert_eq!(buffer[(14, u16::try_from(row).unwrap())].fg, TEXT);
        }
        assert!(!terminal.backend().cursor_visible());
        let text = text(&terminal);
        for unwanted in ["Search", "nortui", "Ctrl-", "Enter", "Reconnect"] {
            assert!(!text.contains(unwanted));
        }
        assert!(buffer.content().iter().all(|cell| cell.bg == Color::Reset));
    }

    #[test]
    fn panel_hides_stale_connection_details_when_service_or_vpn_is_not_connected() {
        let states = [
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
        ];
        for (service, state) in states {
            let mut snapshot = connected();
            snapshot.service = service;
            snapshot.vpn.as_mut().unwrap().state = state;
            let mut app = App::new(Vec::new());
            app.set_snapshot(snapshot);
            let terminal = terminal(&mut app, 80, 12);
            for row in 1..4 {
                assert!(row_text(&terminal, row).contains('—'));
            }
            let text = text(&terminal);
            for stale in ["New York", "United States", "192.0.2.100", "NORDLYNX"] {
                assert!(
                    !text.contains(stale),
                    "stale {stale} shown for {:?}",
                    app.snapshot
                );
            }
        }
    }

    #[test]
    fn panel_uses_available_location_fields_and_retains_details_when_paused() {
        for (country, city, location) in [
            (Some("Japan"), None, "Japan"),
            (None, Some("New_York"), "New York"),
            (None, None, "—"),
        ] {
            let mut snapshot = connected();
            let vpn = snapshot.vpn.as_mut().unwrap();
            vpn.state = VpnState::Paused;
            vpn.country = country.map(str::to_owned);
            vpn.city = city.map(str::to_owned);
            let mut app = App::new(Vec::new());
            app.set_snapshot(snapshot);
            let terminal = terminal(&mut app, 80, 12);
            assert_eq!(
                row_text(&terminal, 1).trim_end(),
                format!("  Location    {location}")
            );
            assert!(row_text(&terminal, 2).contains("192.0.2.100"));
            assert!(row_text(&terminal, 3).contains("NORDLYNX"));
        }
    }

    #[test]
    fn search_has_gray_separators_and_location_placeholder_without_legends() {
        let mut app = App::new(vec![Location {
            country: "Japan".into(),
            city: None,
        }]);
        app.open_search();
        let mut terminal = terminal(&mut app, 80, 12);
        terminal.backend_mut().assert_cursor_position((2, 0));
        assert!(terminal.backend().cursor_visible());
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "›");
        assert_eq!(buffer[(0, 0)].fg, BLUE);
        assert!(row_text(&terminal, 0).contains("Search countries and cities…"));
        for y in [1, 9] {
            for x in 0..80 {
                assert_eq!(buffer[(x, y)].symbol(), "─");
                assert_eq!(buffer[(x, y)].fg, GRAY);
            }
        }
        let text = text(&terminal);
        for unwanted in ["nortui", "Ctrl-", "Enter", "Esc", "commands"] {
            assert!(!text.contains(unwanted));
        }
        assert!(row_text(&terminal, 10).trim().is_empty());
    }

    #[test]
    fn search_rows_use_blue_matches_gray_types_and_only_selected_background() {
        let mut app = App::new(Vec::new());
        app.open_search();
        app.results = vec![
            SearchResult {
                label: "Japan".into(),
                kind: ItemKind::Country,
                action: Action::Connect(Some(Location {
                    country: "Japan".into(),
                    city: None,
                })),
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
        assert_eq!(buffer[(2, 2)].fg, BLUE);
        assert_eq!(buffer[(3, 2)].fg, TEXT);
        assert_eq!(buffer[(2, 3)].fg, TEXT);
        assert_eq!(buffer[(71, 2)].fg, GRAY);
        assert_eq!(buffer[(75, 3)].fg, GRAY);
        for y in 0..12 {
            for x in 0..80 {
                assert_eq!(
                    buffer[(x, y)].bg,
                    if y == 2 {
                        SELECTED_BACKGROUND
                    } else {
                        Color::Reset
                    }
                );
            }
        }
    }

    #[test]
    fn footer_is_gray_right_aligned_with_one_cell_padding_and_state_color() {
        for mode in [Mode::Panel, Mode::Search] {
            for (service, vpn_state, color) in [
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
                    vpn: vpn_state.map(|state| VpnStatus {
                        state,
                        country: None,
                        city: None,
                        ip: None,
                        technology: None,
                    }),
                });
                let text_width = u16::try_from(Line::from(app.status_text(true)).width()).unwrap();
                let circle_x = 79 - text_width - 2;
                let terminal = terminal(&mut app, 80, 12);
                let buffer = terminal.backend().buffer();
                assert_eq!(buffer[(circle_x, 11)].symbol(), "●");
                assert_eq!(buffer[(circle_x, 11)].fg, color);
                for x in circle_x + 2..79 {
                    assert_eq!(buffer[(x, 11)].fg, GRAY);
                }
                assert_eq!(buffer[(79, 11)].symbol(), " ");
                assert_ne!(buffer[(78, 11)].symbol(), " ");
            }
        }
    }

    #[test]
    fn changing_modes_clears_former_content_and_toggles_cursor() {
        let mut app = App::new(vec![Location {
            country: "Japan".into(),
            city: None,
        }]);
        app.set_snapshot(connected());
        let mut terminal = terminal(&mut app, 80, 12);
        assert!(text(&terminal).contains("Technology"));
        app.open_search();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(terminal.backend().cursor_visible());
        assert!(text(&terminal).contains("Search countries and cities…"));
        assert!(!text(&terminal).contains("Technology"));
        assert!(!row_text(&terminal, 2).contains("192.0.2.100"));
        app.mode = Mode::Panel;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!terminal.backend().cursor_visible());
        assert!(!text(&terminal).contains("Search countries and cities…"));
        assert!(row_text(&terminal, 1).contains("New York, United States"));
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
    fn first_launch_guidance_and_empty_search_distinguish_missing_cache_from_no_matches() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(Snapshot {
            service: ServiceState::Inactive,
            vpn: None,
        });
        let panel = terminal(&mut app, 80, 12);
        assert!(text(&panel).contains("Service stopped"));
        assert!(text(&panel).contains("Start service to load locations"));
        app.open_search();
        let search = terminal(&mut app, 80, 12);
        assert!(text(&search).contains("No cached locations"));
        assert!(text(&search).contains("Start service to load locations"));
        app.paste("zzzz");
        assert!(text(&terminal(&mut app, 80, 12)).contains("No matches"));
    }

    #[test]
    fn narrow_screens_unicode_search_and_tiny_terminal_render_without_panicking() {
        let mut app = App::new(vec![Location {
            country: "Japan".into(),
            city: Some("東京".into()),
        }]);
        app.set_snapshot(connected());
        let panel = terminal(&mut app, 40, 10);
        assert!(row_text(&panel, 1).contains("New York, United States"));
        assert!(row_text(&panel, 2).contains("192.0.2.100"));
        assert!(text(&panel).contains("Connected"));
        let short = terminal(&mut app, 24, 5);
        assert!(row_text(&short, 0).contains("Running"));
        assert!(text(&short).contains("Connected"));
        app.open_search();
        app.paste("東京");
        let search_text = text(&terminal(&mut app, 40, 10));
        assert!(search_text.contains('東') && search_text.contains('京'));
        assert!(terminal(&mut app, 24, 5).backend().cursor_visible());
        assert!(text(&terminal(&mut app, 10, 2)).contains("Resize"));
    }
}
