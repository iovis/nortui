use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::{App, ItemKind, SearchResult};
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
    let [search, results, notice, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let search_text = if app.query.is_empty() {
        Line::from(vec![
            Span::styled("› ", Style::default().fg(BLUE)),
            Span::styled(
                "Search commands, countries, cities…",
                Style::default().fg(GRAY),
            ),
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
            Paragraph::new("  No matches").style(Style::default().fg(GRAY)),
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
    let (message, color) = notice_text(app);
    frame.render_widget(
        Paragraph::new(message).style(Style::default().fg(color)),
        notice,
    );
    render_status(frame, app, footer);
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
        } else if row.kind == ItemKind::Command {
            Style::default().fg(BLUE)
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
    use crate::model::{Location, Snapshot, VpnStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn terminal(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        terminal(app, width, height)
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn titleless_search_has_gray_separator_and_no_key_legends() {
        let mut app = App::new(vec![Location {
            country: "Japan".into(),
            city: None,
        }]);
        let mut terminal = terminal(&mut app, 80, 12);
        terminal.backend_mut().assert_cursor_position((2, 0));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "›");
        assert_eq!(buffer[(0, 0)].fg, BLUE);
        for x in 0..80 {
            assert_eq!(buffer[(x, 1)].symbol(), "─");
            assert_eq!(buffer[(x, 1)].fg, GRAY);
        }
        let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(!text.contains("nortui"));
        assert!(!text.contains("Ctrl-"));
        assert!(!text.contains("Enter Run"));
        assert!(!text.contains("Esc Close"));
        assert!(
            buffer.content()[800..880]
                .iter()
                .all(|cell| cell.symbol() == " ")
        );
    }

    #[test]
    fn rows_use_blue_commands_gray_types_and_only_selected_background() {
        let mut app = App::new(Vec::new());
        app.results = vec![
            SearchResult {
                label: "Start service".into(),
                kind: ItemKind::Command,
                action: crate::model::Action::StartService,
                matches: vec![0],
            },
            SearchResult {
                label: "Refresh".into(),
                kind: ItemKind::Command,
                action: crate::model::Action::Refresh,
                matches: Vec::new(),
            },
            SearchResult {
                label: "Japan".into(),
                kind: ItemKind::Country,
                action: crate::model::Action::Connect(Some(Location {
                    country: "Japan".into(),
                    city: None,
                })),
                matches: vec![0],
            },
        ];
        app.list_state.select(Some(0));
        let terminal = terminal(&mut app, 80, 12);
        let buffer = terminal.backend().buffer();
        for x in 2..15 {
            assert_eq!(buffer[(x, 2)].fg, BLUE);
        }
        for x in 2..9 {
            assert_eq!(buffer[(x, 3)].fg, BLUE);
        }
        assert_eq!(buffer[(2, 4)].fg, BLUE);
        assert_eq!(buffer[(3, 4)].fg, TEXT);
        for y in 2..5 {
            assert_eq!(buffer[(71, y)].fg, GRAY);
        }
        for y in 0..12 {
            for x in 0..80 {
                assert_eq!(
                    buffer[(x, y)].bg,
                    if y == 2 {
                        SELECTED_BACKGROUND
                    } else {
                        Color::Reset
                    },
                    "unexpected background at ({x}, {y})",
                );
            }
        }
    }

    #[test]
    fn footer_is_gray_right_aligned_with_one_cell_padding_and_state_color() {
        for (service, vpn_state, color) in [
            (ServiceState::Active, Some(VpnState::Connected), GREEN),
            (ServiceState::Active, Some(VpnState::Disconnected), RED),
            (ServiceState::Active, Some(VpnState::Connecting), YELLOW),
            (ServiceState::Active, Some(VpnState::Paused), YELLOW),
            (ServiceState::Inactive, None, GRAY),
            (ServiceState::Activating, None, GRAY),
            (ServiceState::Deactivating, None, GRAY),
            (ServiceState::Failed, None, GRAY),
        ] {
            let mut app = App::new(Vec::new());
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

    #[test]
    fn stopped_footer_and_initial_location_guidance_are_visible() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(Snapshot {
            service: ServiceState::Inactive,
            vpn: None,
        });
        let text = screen(&mut app, 80, 24);
        assert!(text.contains("Service stopped"));
        assert!(text.contains("Start service to load locations"));
        assert!(text.contains("Start service"));
    }

    #[test]
    fn narrow_connected_screen_retains_location_and_hides_extra_details() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(Snapshot {
            service: ServiceState::Active,
            vpn: Some(VpnStatus {
                state: VpnState::Connected,
                country: Some("Japan".into()),
                city: Some("Tokyo".into()),
                ip: Some("192.0.2.100".into()),
                technology: Some("NORDLYNX".into()),
            }),
        });
        let text = screen(&mut app, 40, 10);
        assert!(text.contains("Connected · Japan / Tokyo"));
        assert!(!text.contains("192.0.2.100"));
        assert!(text.contains("Reconnect"));
    }

    #[test]
    fn empty_results_unicode_and_small_terminal_render_without_panicking() {
        let mut app = App::new(vec![Location {
            country: "Japan".into(),
            city: Some("東京".into()),
        }]);
        app.paste("東京");
        let text = screen(&mut app, 80, 12);
        assert!(text.contains('東') && text.contains('京'));
        app.paste("zzzz");
        assert!(screen(&mut app, 40, 10).contains("No matches"));
        assert!(screen(&mut app, 10, 2).contains("Resize"));
    }
}
