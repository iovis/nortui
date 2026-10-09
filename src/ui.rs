use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::{App, SearchResult};
use crate::model::{ServiceState, VpnState};

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.height < 5 || area.width < 24 {
        frame.render_widget(Paragraph::new("Resize to at least 24 × 5"), area);
        return;
    }
    let [search, results, notice, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let search_text = if app.query.is_empty() {
        Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Cyan)),
            Span::styled(
                "Search commands, countries, cities…",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Cyan)),
            Span::raw(visible_query(&app.query, search.width.saturating_sub(3))),
        ])
    };
    frame.render_widget(
        Paragraph::new(search_text).block(Block::default().borders(Borders::BOTTOM).title(
            Line::from(vec![Span::styled(
                    " nortui ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )]),
        )),
        search,
    );
    let cursor_width =
        Line::from(visible_query(&app.query, search.width.saturating_sub(3))).width();
    frame.set_cursor_position((
        search.x + 2 + u16::try_from(cursor_width).unwrap_or(0),
        search.y + 1,
    ));
    if app.results.is_empty() {
        frame.render_widget(
            Paragraph::new("  No matches").style(Style::default().fg(Color::DarkGray)),
            results,
        );
    } else {
        let rows: Vec<_> = app
            .results
            .iter()
            .map(|row| list_item(row, results.width))
            .collect();
        let list = List::new(rows).highlight_symbol("› ").highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_stateful_widget(list, results, &mut app.list_state);
    }
    let (message, color) = notice_text(app);
    frame.render_widget(
        Paragraph::new(message).style(Style::default().fg(color)),
        notice,
    );
    let hints = if area.width >= 76 {
        "↑↓ Select  Enter Run  Esc Close"
    } else {
        "Enter Run  Esc Close"
    };
    let hint_width = u16::try_from(hints.len()).unwrap_or(u16::MAX);
    let available = area.width.saturating_sub(hint_width + 3);
    let status = app.status_text(true);
    let details = Line::from(status.clone()).width() <= usize::from(available);
    let status = if details {
        status
    } else {
        app.status_text(false)
    };
    let status_width = u16::try_from(Line::from(status.as_str()).width()).unwrap_or(u16::MAX);
    let show_hints = status_width <= available;
    let [status_area, hints_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(if show_hints { hint_width } else { 0 }),
    ])
    .areas(footer);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("● ", Style::default().fg(status_color(app))),
            Span::raw(status),
        ])),
        status_area,
    );
    if show_hints {
        frame.render_widget(
            Paragraph::new(hints).style(Style::default().fg(Color::DarkGray)),
            hints_area,
        );
    }
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
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        spans.push(Span::styled(grapheme.to_owned(), style));
    }
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::styled(kind, Style::default().fg(Color::Gray)));
    }
    ListItem::new(Line::from(spans))
}

fn notice_text(app: &App) -> (String, Color) {
    if let Some(action) = &app.busy {
        return (action.label(), Color::Yellow);
    }
    if let Some(notice) = &app.notice {
        return (
            notice.message.clone(),
            if notice.is_error {
                Color::Red
            } else {
                Color::Gray
            },
        );
    }
    if let Some((finished, total)) = app.loading {
        return (
            if total == 0 {
                "Loading countries…".into()
            } else {
                format!("Loading cities · {finished}/{total} countries")
            },
            Color::Gray,
        );
    }
    if app.locations.is_empty() && app.snapshot.service == ServiceState::Inactive {
        return ("Start service to load locations".into(), Color::Gray);
    }
    (
        format!(
            "{} results · Ctrl-R Refresh · Ctrl-U Clear",
            app.results.len()
        ),
        Color::DarkGray,
    )
}

fn status_color(app: &App) -> Color {
    match &app.snapshot.service {
        ServiceState::Active => match app.snapshot.vpn.as_ref().map(|vpn| &vpn.state) {
            Some(VpnState::Connected) => Color::Green,
            Some(VpnState::Connecting | VpnState::Paused) => Color::Yellow,
            Some(VpnState::Unavailable(_)) => Color::Red,
            _ => Color::Gray,
        },
        ServiceState::Activating | ServiceState::Deactivating => Color::Yellow,
        ServiceState::Failed | ServiceState::Unavailable(_) => Color::Red,
        ServiceState::Inactive => Color::Gray,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Location, Snapshot, VpnStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
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
