use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::cursor::MoveTo;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as InputEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::ListState;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::JoinHandle;
use unicode_segmentation::UnicodeSegmentation;

use crate::backend::Backend;
use crate::cache::{self, Catalog};
use crate::model::{Action, Location, ServiceState, Snapshot, VpnState};
use crate::ui;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const REFRESH_NOTICE: &str = "Refreshing status…";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Panel,
    Search,
    Help,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ItemKind {
    Country,
    City,
}

impl ItemKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Country => "Country",
            Self::City => "City",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchResult {
    pub label: String,
    pub kind: ItemKind,
    pub action: Action,
    pub matches: Vec<u32>,
}

pub struct Notice {
    pub message: String,
    pub is_error: bool,
    created: Instant,
}

pub struct App {
    pub mode: Mode,
    pub query: String,
    pub help_scroll: u16,
    pub snapshot: Snapshot,
    pub locations: Vec<Location>,
    pub results: Vec<SearchResult>,
    pub selected: usize,
    pub list_state: ListState,
    pub notice: Option<Notice>,
    pub busy: Option<Action>,
    pub loading: Option<(usize, usize)>,
    panel_snapshot: Option<Snapshot>,
    initializing: bool,
    action_preparing: bool,
    query_cursor: usize,
    matcher: Matcher,
}

impl App {
    pub fn new(locations: Vec<Location>) -> Self {
        let mut app = Self {
            mode: Mode::Panel,
            query: String::new(),
            help_scroll: 0,
            snapshot: Snapshot::default(),
            locations,
            results: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
            notice: None,
            busy: None,
            loading: None,
            panel_snapshot: None,
            initializing: true,
            action_preparing: false,
            query_cursor: 0,
            matcher: Matcher::new(Config::DEFAULT),
        };
        app.rebuild(false);
        app
    }

    pub fn set_snapshot(&mut self, snapshot: Snapshot) {
        let settled = match snapshot.service {
            ServiceState::Inactive | ServiceState::Failed => true,
            ServiceState::Active => snapshot.vpn.as_ref().is_some_and(|vpn| {
                matches!(
                    vpn.state,
                    VpnState::Connected | VpnState::Disconnected | VpnState::Paused
                )
            }),
            _ => false,
        };
        if self.busy.is_none() && settled {
            self.panel_snapshot = Some(snapshot.clone());
        }
        self.initializing = false;
        self.snapshot = snapshot;
        self.rebuild(true);
    }

    pub fn panel_snapshot(&self) -> Option<&Snapshot> {
        self.panel_snapshot.as_ref()
    }

    pub fn is_initializing(&self) -> bool {
        self.initializing
    }

    pub fn running_action(&self) -> Option<&Action> {
        self.busy.as_ref().filter(|_| !self.action_preparing)
    }

    pub fn set_locations(&mut self, locations: Vec<Location>) {
        self.locations = locations;
        self.rebuild(true);
    }

    pub fn set_notice(&mut self, message: impl Into<String>, is_error: bool) {
        self.notice = Some(Notice {
            message: message.into().replace(['\n', '\r'], " "),
            is_error,
            created: Instant::now(),
        });
    }

    pub fn expire_notice(&mut self) {
        if self.notice.as_ref().is_some_and(|notice| {
            !notice.is_error
                && notice.message != REFRESH_NOTICE
                && notice.created.elapsed() > Duration::from_secs(3)
        }) {
            self.notice = None;
        }
    }

    pub fn is_working(&self) -> bool {
        self.is_initializing()
            || self.running_action().is_some()
            || self.loading.is_some()
            || matches!(
                self.snapshot.service,
                ServiceState::Activating | ServiceState::Deactivating
            )
            || (self.snapshot.service == ServiceState::Active
                && self
                    .snapshot
                    .vpn
                    .as_ref()
                    .is_some_and(|vpn| vpn.state == VpnState::Connecting))
            || self
                .notice
                .as_ref()
                .is_some_and(|notice| !notice.is_error && notice.message == REFRESH_NOTICE)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Input {
        if key.kind == KeyEventKind::Release
            || key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return Input::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => Input::Exit,
                KeyCode::Char('r') if key.kind == KeyEventKind::Press => {
                    Input::Execute(Action::Refresh)
                }
                KeyCode::Char('n' | 'j') if self.mode == Mode::Search => {
                    self.move_selection(1);
                    Input::None
                }
                KeyCode::Char('p' | 'k') if self.mode == Mode::Search => {
                    self.move_selection(-1);
                    Input::None
                }
                KeyCode::Char('j') if self.mode == Mode::Help => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                    Input::None
                }
                KeyCode::Char('k') if self.mode == Mode::Help => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                    Input::None
                }
                KeyCode::Char('a') if self.mode == Mode::Search => {
                    self.query_cursor = 0;
                    Input::None
                }
                KeyCode::Char('e') if self.mode == Mode::Search => {
                    self.query_cursor = self.query.len();
                    Input::None
                }
                KeyCode::Char('u') if self.mode == Mode::Search => {
                    self.query.clear();
                    self.query_cursor = 0;
                    self.rebuild(false);
                    Input::None
                }
                _ => Input::None,
            };
        }
        match self.mode {
            Mode::Panel if key.kind == KeyEventKind::Press => self.handle_panel_key(key.code),
            Mode::Panel => Input::None,
            Mode::Search => self.handle_search_key(key),
            Mode::Help => self.handle_help_key(key),
        }
    }

    pub fn open_search(&mut self) {
        self.mode = Mode::Search;
        self.query.clear();
        self.query_cursor = 0;
        self.help_scroll = 0;
        self.rebuild(false);
    }

    fn show_panel(&mut self) {
        self.mode = Mode::Panel;
        self.query.clear();
        self.query_cursor = 0;
        self.help_scroll = 0;
        self.rebuild(false);
    }

    fn handle_help_key(&mut self, key: KeyEvent) -> Input {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') if key.kind == KeyEventKind::Press => {
                self.show_panel();
            }
            KeyCode::Char('q') if key.kind == KeyEventKind::Press => return Input::Exit,
            KeyCode::Down | KeyCode::Char('j') => {
                self.help_scroll = self.help_scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
            }
            KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(10),
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
            KeyCode::Home => self.help_scroll = 0,
            KeyCode::End => self.help_scroll = u16::MAX,
            _ => {}
        }
        Input::None
    }

    fn handle_panel_key(&mut self, code: KeyCode) -> Input {
        match code {
            KeyCode::Char('i' | '/') => {
                self.open_search();
                Input::None
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                self.help_scroll = 0;
                self.rebuild(false);
                Input::None
            }
            KeyCode::Char('c') => Input::Execute(
                if self.snapshot.service == ServiceState::Active
                    && self
                        .snapshot
                        .vpn
                        .as_ref()
                        .is_some_and(|vpn| vpn.state == VpnState::Connected)
                {
                    Action::Reconnect
                } else {
                    Action::Connect(None)
                },
            ),
            KeyCode::Char('d') => Input::Execute(Action::Disconnect),
            KeyCode::Char('s') => match self.snapshot.service {
                ServiceState::Active => Input::Execute(Action::StopService),
                ServiceState::Inactive | ServiceState::Failed => {
                    Input::Execute(Action::StartService)
                }
                ServiceState::Activating | ServiceState::Deactivating => {
                    self.set_notice("Service change is still in progress", false);
                    Input::None
                }
                ServiceState::Unavailable(_) => {
                    self.set_notice("Service state unavailable; refresh to check again", true);
                    Input::None
                }
            },
            KeyCode::Char('r') => Input::Execute(Action::Refresh),
            KeyCode::Char('q') | KeyCode::Esc => Input::Exit,
            _ => Input::None,
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Input {
        match key.code {
            KeyCode::Esc => {
                self.show_panel();
                Input::None
            }
            KeyCode::Down => {
                self.move_selection(1);
                Input::None
            }
            KeyCode::Up => {
                self.move_selection(-1);
                Input::None
            }
            KeyCode::PageDown => {
                self.move_selection(10);
                Input::None
            }
            KeyCode::PageUp => {
                self.move_selection(-10);
                Input::None
            }
            KeyCode::Left => {
                self.query_cursor = self.previous_query_boundary();
                Input::None
            }
            KeyCode::Right => {
                self.query_cursor = self.next_query_boundary();
                Input::None
            }
            KeyCode::Home => {
                self.query_cursor = 0;
                Input::None
            }
            KeyCode::End => {
                self.query_cursor = self.query.len();
                Input::None
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press && self.busy.is_none() => {
                let action = self
                    .results
                    .get(self.selected)
                    .map(|row| row.action.clone());
                if let Some(action) = action {
                    self.show_panel();
                    Input::Execute(action)
                } else {
                    Input::None
                }
            }
            KeyCode::Backspace => {
                let cursor = self.query_cursor();
                let previous = self.previous_query_boundary();
                if previous != cursor {
                    self.query.replace_range(previous..cursor, "");
                    self.query_cursor = previous;
                    self.rebuild(false);
                }
                Input::None
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert_query(character.encode_utf8(&mut [0; 4]));
                Input::None
            }
            _ => Input::None,
        }
    }

    pub fn paste(&mut self, value: &str) {
        if self.mode != Mode::Search {
            return;
        }
        let value: String = value
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        self.insert_query(&value);
    }

    pub fn query_cursor(&self) -> usize {
        self.query
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain(std::iter::once(self.query.len()))
            .take_while(|index| *index <= self.query_cursor)
            .last()
            .unwrap_or(0)
    }

    fn previous_query_boundary(&self) -> usize {
        let cursor = self.query_cursor();
        self.query
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .take_while(|index| *index < cursor)
            .last()
            .unwrap_or(0)
    }

    fn next_query_boundary(&self) -> usize {
        let cursor = self.query_cursor();
        self.query
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .find(|index| *index > cursor)
            .unwrap_or(self.query.len())
    }

    fn insert_query(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        let cursor = self.query_cursor();
        self.query.insert_str(cursor, value);
        let inserted_end = cursor + value.len();
        // Inserting a combining mark or joiner can merge with the following
        // grapheme. Keep the cursor after the whole newly formed cluster.
        self.query_cursor = self
            .query
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain(std::iter::once(self.query.len()))
            .find(|index| *index >= inserted_end)
            .unwrap_or(self.query.len());
        self.rebuild(false);
    }

    pub fn status_text(&self) -> String {
        if self.is_initializing() {
            return "Checking…".into();
        }
        match &self.snapshot.service {
            ServiceState::Inactive => "Service stopped".into(),
            ServiceState::Activating => "Service starting…".into(),
            ServiceState::Deactivating => "Service stopping…".into(),
            ServiceState::Failed => "Service failed".into(),
            ServiceState::Unavailable(_) => "Service unavailable".into(),
            ServiceState::Active => match self.snapshot.vpn.as_ref().map(|vpn| &vpn.state) {
                Some(VpnState::Connected) => "Connected",
                Some(VpnState::Connecting) => "Connecting…",
                Some(VpnState::Disconnected) => "Disconnected",
                Some(VpnState::Paused) => "Paused",
                Some(VpnState::Unavailable(_)) => "VPN unavailable",
                None => "Checking VPN…",
            }
            .into(),
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.results.len() - 1);
        self.list_state.select(Some(self.selected));
    }

    fn candidates(&self) -> Vec<SearchResult> {
        if self.mode != Mode::Search {
            return Vec::new();
        }
        let mut locations: Vec<_> = self
            .locations
            .iter()
            .map(|location| SearchResult {
                label: location.label(),
                kind: if location.city.is_some() {
                    ItemKind::City
                } else {
                    ItemKind::Country
                },
                action: Action::Connect(Some(location.clone())),
                matches: Vec::new(),
            })
            .collect();
        locations.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.label.cmp(&right.label))
        });
        locations
    }

    fn rebuild(&mut self, preserve: bool) {
        let selected_action = preserve
            .then(|| {
                self.results
                    .get(self.selected)
                    .map(|row| row.action.clone())
            })
            .flatten();
        let query = self.query.trim().replace('_', " ");
        let pattern = Pattern::parse(&query, CaseMatching::Ignore, Normalization::Smart);
        let mut buffer = Vec::new();
        let mut scored: Vec<_> = self
            .candidates()
            .into_iter()
            .filter_map(|mut row| {
                let score = pattern.indices(
                    Utf32Str::new(&row.label, &mut buffer),
                    &mut self.matcher,
                    &mut row.matches,
                )?;
                row.matches.sort_unstable();
                row.matches.dedup();
                Some((score, row))
            })
            .collect();
        if !query.is_empty() {
            let exact_query = query
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            scored.sort_by(|(left_score, left), (right_score, right)| {
                (right.label.to_lowercase() == exact_query)
                    .cmp(&(left.label.to_lowercase() == exact_query))
                    .then_with(|| {
                        right_score
                            .cmp(left_score)
                            .then_with(|| left.label.cmp(&right.label))
                    })
            });
        }
        self.results = scored.into_iter().map(|(_, row)| row).collect();
        self.selected = selected_action
            .and_then(|action| self.results.iter().position(|row| row.action == action))
            .unwrap_or(0);
        self.list_state
            .select((!self.results.is_empty()).then_some(self.selected));
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    None,
    Exit,
    Execute(Action),
}

enum Event {
    Status(std::result::Result<Snapshot, String>),
    Prepared {
        action: Action,
        authenticate: bool,
    },
    Finished(std::result::Result<String, String>),
    Countries(Vec<String>),
    Cities {
        country: String,
        result: std::result::Result<Vec<String>, String>,
        finished: usize,
        total: usize,
    },
    DiscoveryDone(Option<String>),
    Exit,
}

struct Message {
    generation: u64,
    event: Event,
}

struct Controller {
    app: App,
    backend: Backend,
    catalog: Catalog,
    cache_path: Option<PathBuf>,
    handle: Handle,
    sender: Sender<Message>,
    receiver: Receiver<Message>,
    generation: u64,
    status_job: Option<JoinHandle<()>>,
    action_job: Option<JoinHandle<()>>,
    discovery_job: Option<JoinHandle<()>>,
    discovery_requested: bool,
    last_poll: Instant,
    status_error: Option<String>,
}

impl Controller {
    fn new(backend: Backend, handle: Handle) -> Self {
        let cache_path = cache::default_path();
        let loaded = cache_path.as_deref().map(Catalog::load).transpose();
        let (catalog, error) = match loaded {
            Ok(catalog) => (catalog.unwrap_or_default(), None),
            Err(error) => (
                Catalog::default(),
                Some(format!(
                    "{error}; locations will reload when you open search"
                )),
            ),
        };
        let mut app = App::new(catalog.locations());
        if let Some(error) = error {
            app.set_notice(error, true);
        }
        let (sender, receiver) = mpsc::channel();
        Self {
            app,
            backend,
            catalog,
            cache_path,
            handle,
            sender,
            receiver,
            generation: 0,
            status_job: None,
            action_job: None,
            discovery_job: None,
            discovery_requested: true,
            last_poll: Instant::now(),
            status_error: None,
        }
    }

    fn spawn_status(&mut self) {
        abort(&mut self.status_job);
        let backend = self.backend.clone();
        let sender = self.sender.clone();
        let generation = self.generation;
        self.last_poll = Instant::now();
        self.status_job = Some(self.handle.spawn(async move {
            let result = backend
                .snapshot()
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(Message {
                generation,
                event: Event::Status(result),
            });
        }));
    }

    fn tick(&mut self) {
        self.app.expire_notice();
        if self.last_poll.elapsed() >= POLL_INTERVAL
            && self.status_job.as_ref().is_none_or(JoinHandle::is_finished)
        {
            self.spawn_status();
        }
        if self.discovery_requested
            && self.app.mode == Mode::Search
            && self.app.busy.is_none()
            && self.app.snapshot.service == ServiceState::Active
            && self.discovery_job.is_none()
        {
            self.discovery_requested = false;
            self.app.loading = Some((0, 0));
            self.discovery_job = Some(self.handle.spawn(discover(
                self.backend.clone(),
                self.sender.clone(),
                self.generation,
            )));
        }
    }

    fn execute(&mut self, action: Action) {
        if self.app.busy.is_some() {
            return;
        }
        self.generation += 1;
        abort(&mut self.status_job);
        self.discovery_requested |= self.discovery_job.is_some() || action == Action::Refresh;
        abort(&mut self.discovery_job);
        self.app.loading = None;
        if action == Action::Refresh {
            self.app.set_notice(REFRESH_NOTICE, false);
            self.spawn_status();
            return;
        }
        self.app.busy = Some(action.clone());
        self.app.action_preparing = true;
        let backend = self.backend.clone();
        let sender = self.sender.clone();
        let generation = self.generation;
        self.action_job = Some(self.handle.spawn(async move {
            let result = async {
                let requires_auth = backend.requires_auth(&action).await?;
                let authenticate = requires_auth && !backend.has_credentials().await?;
                Ok::<_, anyhow::Error>(authenticate)
            }
            .await;
            let event = match result {
                Ok(authenticate) => Event::Prepared {
                    action,
                    authenticate,
                },
                Err(error) => Event::Finished(Err(format!("{error:#}"))),
            };
            let _ = sender.send(Message { generation, event });
        }));
    }

    fn start_action(&mut self, action: Action) {
        self.app.action_preparing = false;
        self.app.set_notice(action.label(), false);
        let backend = self.backend.clone();
        let sender = self.sender.clone();
        let generation = self.generation;
        self.action_job = Some(self.handle.spawn(async move {
            let result = backend
                .execute(&action)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(Message {
                generation,
                event: Event::Finished(result),
            });
        }));
    }

    fn finish_action(&mut self, result: std::result::Result<String, String>) {
        self.generation += 1;
        self.app.busy = None;
        self.app.action_preparing = false;
        self.action_job = None;
        match result {
            Ok(message) => self.app.set_notice(message, false),
            Err(message) => self.app.set_notice(message, true),
        }
        self.spawn_status();
    }

    fn receive(&mut self, terminal: &mut TerminalSession) -> Result<bool> {
        while let Ok(message) = self.receiver.try_recv() {
            if matches!(message.event, Event::Exit) {
                return Ok(false);
            }
            if message.generation != self.generation {
                continue;
            }
            self.apply_event(message.event, terminal)?;
        }
        Ok(true)
    }

    fn apply_event(&mut self, event: Event, terminal: &mut TerminalSession) -> Result<()> {
        match event {
            Event::Status(result) => self.apply_status(result),
            Event::Prepared {
                action,
                authenticate,
            } => {
                if authenticate {
                    // No Crossterm input reader runs while sudo owns the terminal.
                    let result = terminal.suspend(|| self.backend.authenticate())?;
                    if let Err(error) = result {
                        self.finish_action(Err(format!("{error:#}")));
                        return Ok(());
                    }
                }
                self.start_action(action);
            }
            Event::Finished(result) => self.finish_action(result),
            Event::Countries(countries) => {
                self.app.loading = Some((0, countries.len()));
                self.catalog.set_countries(&countries);
                self.update_catalog();
            }
            Event::Cities {
                country,
                result,
                finished,
                total,
            } => {
                self.app.loading = Some((finished, total));
                if let Ok(cities) = result {
                    self.catalog.set_cities(country, cities);
                    self.update_catalog();
                }
            }
            Event::DiscoveryDone(error) => {
                self.discovery_job = None;
                self.app.loading = None;
                if let Some(error) = error {
                    self.app.set_notice(error, true);
                }
            }
            Event::Exit => {}
        }
        Ok(())
    }

    fn apply_status(&mut self, result: std::result::Result<Snapshot, String>) {
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => Snapshot {
                service: ServiceState::Unavailable(error),
                vpn: None,
            },
        };
        if snapshot.service != ServiceState::Active {
            self.discovery_requested |= self.discovery_job.is_some();
            abort(&mut self.discovery_job);
            self.app.loading = None;
        }
        let error = match &snapshot.service {
            ServiceState::Unavailable(error) => Some(error),
            _ => snapshot.vpn.as_ref().and_then(|vpn| match &vpn.state {
                VpnState::Unavailable(error) => Some(error),
                _ => None,
            }),
        };
        let next_error = error.cloned();
        if let Some(error) = error {
            self.app.set_notice(error.clone(), true);
        } else if self.status_error.as_ref().is_some_and(|previous| {
            self.app
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message == previous.replace(['\n', '\r'], " "))
        }) {
            self.app.notice = None;
        }
        if next_error.is_none()
            && self
                .app
                .notice
                .as_ref()
                .is_some_and(|notice| !notice.is_error && notice.message == REFRESH_NOTICE)
        {
            self.app.set_notice("Status refreshed", false);
        }
        self.status_error = next_error;
        self.app.set_snapshot(snapshot);
    }

    fn update_catalog(&mut self) {
        self.app.set_locations(self.catalog.locations());
        if let Some(path) = &self.cache_path
            && let Err(error) = self.catalog.save(path)
        {
            self.app.set_notice(format!("{error:#}"), true);
        }
    }

    async fn shutdown(&mut self) {
        let jobs: Vec<_> = [
            &mut self.status_job,
            &mut self.action_job,
            &mut self.discovery_job,
        ]
        .into_iter()
        .filter_map(Option::take)
        .collect();
        for job in &jobs {
            job.abort();
        }
        for job in jobs {
            let _ = job.await;
        }
        self.backend.reap_canceled().await;
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        abort(&mut self.status_job);
        abort(&mut self.action_job);
        abort(&mut self.discovery_job);
    }
}

fn abort(job: &mut Option<JoinHandle<()>>) {
    if let Some(job) = job.take() {
        job.abort();
    }
}

async fn discover(backend: Backend, sender: Sender<Message>, generation: u64) {
    let mut errors = Vec::new();
    if let Err(error) = discover_inner(backend, &sender, generation, &mut errors).await {
        errors.push(format!("{error:#}"));
    }
    let error = (!errors.is_empty()).then(|| {
        format!(
            "Location refresh: {} ({} failed; cached entries retained)",
            errors[0],
            errors.len()
        )
    });
    let _ = sender.send(Message {
        generation,
        event: Event::DiscoveryDone(error),
    });
}

async fn discover_inner(
    backend: Backend,
    sender: &Sender<Message>,
    generation: u64,
    errors: &mut Vec<String>,
) -> Result<()> {
    if backend.service_state().await? != ServiceState::Active {
        return Ok(());
    }
    let countries = backend.countries().await?;
    let total = countries.len();
    let _ = sender.send(Message {
        generation,
        event: Event::Countries(countries.clone()),
    });
    let finished = AtomicUsize::new(0);
    for batch in countries.chunks(4) {
        // These futures belong to the discovery job. Aborting and joining that job
        // drops all four children before shutdown awaits the backend's reapers.
        let query =
            |country| query_country(&backend, sender, generation, country, &finished, total);
        let results = tokio::join!(
            query(batch.first()),
            query(batch.get(1)),
            query(batch.get(2)),
            query(batch.get(3))
        );
        errors.extend(
            [results.0, results.1, results.2, results.3]
                .into_iter()
                .flatten(),
        );
    }
    Ok(())
}

async fn query_country(
    backend: &Backend,
    sender: &Sender<Message>,
    generation: u64,
    country: Option<&String>,
    finished: &AtomicUsize,
    total: usize,
) -> Option<String> {
    let country = country?;
    let result = backend
        .cities(country)
        .await
        .map_err(|error| format!("{error:#}"));
    let error = result
        .as_ref()
        .err()
        .map(|error| format!("{country}: {error}"));
    let finished = finished.fetch_add(1, Ordering::Relaxed) + 1;
    let _ = sender.send(Message {
        generation,
        event: Event::Cities {
            country: country.clone(),
            result,
            finished,
            total,
        },
    });
    error
}

// Ratatui's Terminal::drop prints a failed cursor-restoration error to stderr.
// Ignore writes only after stdout's terminal has closed, so that hidden-cursor
// cleanup cannot recursively panic before the controller reaps its children.
struct TerminalOutput(io::Stdout);

impl io::Write for TerminalOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match io::Write::write(&mut self.0, buffer) {
            Err(_) if !self.0.is_terminal() => Ok(buffer.len()),
            result => result,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match io::Write::flush(&mut self.0) {
            Err(_) if !self.0.is_terminal() => Ok(()),
            result => result,
        }
    }
}

type WidgetTerminal = Terminal<CrosstermBackend<TerminalOutput>>;

struct TerminalSession {
    terminal: WidgetTerminal,
}

impl TerminalSession {
    fn new() -> Result<Self> {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |information| {
            let _ = ratatui::try_restore();
            previous_hook(information);
        }));
        Ok(Self {
            terminal: Self::initialize().context("Cannot initialize terminal")?,
        })
    }

    fn initialize() -> Result<WidgetTerminal> {
        let result = (|| {
            enable_raw_mode()?;
            execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
            Ok(Terminal::new(CrosstermBackend::new(TerminalOutput(
                io::stdout(),
            )))?)
        })();
        if result.is_err() {
            let _ = ratatui::try_restore();
        }
        result
    }

    fn suspend<T>(&mut self, operation: impl FnOnce() -> T) -> Result<T> {
        execute!(io::stdout(), DisableBracketedPaste)?;
        self.terminal.show_cursor()?;
        // Keep sudo in the alternate screen so its prompt cannot reappear in
        // the saved shell screen when the widget closes.
        disable_raw_mode().context("Cannot restore terminal input for authentication")?;
        execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
        let result = operation();
        enable_raw_mode().context("Cannot restore terminal input after authentication")?;
        execute!(
            io::stdout(),
            EnableBracketedPaste,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        // Recreate Ratatui's buffers for a full redraw without switching screens
        // or querying cursor position through Terminal::clear.
        self.terminal = Terminal::new(CrosstermBackend::new(TerminalOutput(io::stdout())))
            .context("Cannot restore terminal after authentication")?;
        Ok(result)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        // restore() prints on failure, which can panic when stderr is the same
        // deleted terminal. Cleanup must reach the controller's child reapers.
        let _ = ratatui::try_restore();
    }
}

pub fn run(backend: Backend) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("nortui needs an interactive terminal; try kitty --class=widget.nortui -e nortui");
    }
    let runtime = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("Cannot start background workers")?;
    let result = run_with_runtime(backend, &runtime);
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

fn run_with_runtime(backend: Backend, runtime: &Runtime) -> Result<()> {
    let mut controller = Controller::new(backend, runtime.handle().clone());
    let signal_job = install_signals(runtime.handle(), controller.sender.clone())?;
    let result = run_loop(&mut controller);
    signal_job.abort();
    runtime.block_on(async {
        controller.shutdown().await;
        let _ = signal_job.await;
    });
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        // Closing the window can race a draw/read and produce an I/O error.
        Ok(())
    } else {
        result
    }
}

fn run_loop(controller: &mut Controller) -> Result<()> {
    let mut terminal = TerminalSession::new()?;
    controller.spawn_status();
    loop {
        // A closed widget no longer has a usable terminal. The bounded input
        // reader returns on EOF, allowing this check and final child cleanup.
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            break;
        }
        if !controller.receive(&mut terminal)? {
            break;
        }
        controller.tick();
        terminal
            .terminal
            .draw(|frame| ui::render(frame, &mut controller.app))?;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let input = match event::read()? {
            InputEvent::Key(key) => controller.app.handle_key(key),
            InputEvent::Paste(text) => {
                controller.app.paste(&text);
                Input::None
            }
            _ => Input::None,
        };
        match input {
            Input::Exit => break,
            Input::Execute(action) => controller.execute(action),
            Input::None => {}
        }
    }
    Ok(())
}

fn install_signals(handle: &Handle, sender: Sender<Message>) -> Result<JoinHandle<()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let _guard = handle.enter();
    let mut termination = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(handle.spawn(async move {
        tokio::select! {
            _ = termination.recv() => {},
            _ = hangup.recv() => {},
            _ = interrupt.recv() => {},
        }
        let _ = sender.send(Message {
            generation: 0,
            event: Event::Exit,
        });
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn multiword_fuzzy_search_finds_city_and_keeps_raw_tokens() {
        let mut app = App::new(vec![Location {
            country: "United_States".into(),
            city: Some("New_York".into()),
        }]);
        app.open_search();
        app.paste("new york");
        assert_eq!(app.results.len(), 1);
        assert!(!app.results[0].matches.is_empty());
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Input::Execute(Action::Connect(Some(Location {
                country: "United_States".into(),
                city: Some("New_York".into())
            })))
        );
        assert_eq!(app.mode, Mode::Panel);
        assert!(app.query.is_empty());
        assert!(app.results.is_empty());
    }

    #[test]
    fn search_letters_never_trigger_panel_shortcuts_and_escape_returns_to_panel() {
        let mut app = App::new(Vec::new());
        app.open_search();
        for letter in "cdsrqi/".chars() {
            assert_eq!(app.handle_key(key(KeyCode::Char(letter))), Input::None);
        }
        assert_eq!(app.query, "cdsrqi/");
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Input::None);
        assert_eq!(app.mode, Mode::Panel);
        assert!(app.query.is_empty());
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Input::Exit);
        app.open_search();
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Input::None);
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Input::Exit);
    }

    #[test]
    fn panel_has_no_results_and_both_search_keys_open_locations_without_inserting_text() {
        let mut app = App::new(vec![
            Location {
                country: "Japan".into(),
                city: None,
            },
            Location {
                country: "Japan".into(),
                city: Some("Tokyo".into()),
            },
        ]);
        assert_eq!(app.mode, Mode::Panel);
        assert!(app.results.is_empty());
        app.paste("s");
        assert!(app.query.is_empty());
        for trigger in ['i', '/'] {
            assert_eq!(app.handle_key(key(KeyCode::Char(trigger))), Input::None);
            assert_eq!(app.mode, Mode::Search);
            assert!(app.query.is_empty());
            assert_eq!(app.results.len(), 2);
            assert_eq!(app.results[0].kind, ItemKind::Country);
            assert_eq!(app.results[1].kind, ItemKind::City);
            app.paste("japan");
            app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
            assert!(app.query.is_empty());
            assert_eq!(app.results.len(), 2);
            app.handle_key(key(KeyCode::Esc));
            assert!(app.results.is_empty());
        }
    }

    #[test]
    fn control_j_k_move_selection_without_changing_search() {
        let mut app = App::new(vec![
            Location {
                country: "Japan".into(),
                city: None,
            },
            Location {
                country: "Japan".into(),
                city: Some("Tokyo".into()),
            },
        ]);
        app.open_search();
        assert_eq!(app.selected, 0);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Input::None
        );
        assert_eq!(app.results[app.selected].kind, ItemKind::City);
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.selected, 0);
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.selected, 0);
        assert!(app.query.is_empty());
    }

    #[test]
    fn panel_shortcuts_choose_connect_disconnect_refresh_and_observed_service_toggle() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(Snapshot {
            service: ServiceState::Inactive,
            vpn: None,
        });
        assert_eq!(
            app.handle_key(key(KeyCode::Char('c'))),
            Input::Execute(Action::Connect(None))
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('d'))),
            Input::Execute(Action::Disconnect)
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'))),
            Input::Execute(Action::Refresh)
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('s'))),
            Input::Execute(Action::StartService)
        );
        app.set_snapshot(Snapshot {
            service: ServiceState::Active,
            vpn: Some(crate::model::VpnStatus {
                state: VpnState::Connected,
                country: None,
                city: None,
                ip: None,
                technology: None,
            }),
        });
        assert_eq!(
            app.handle_key(key(KeyCode::Char('c'))),
            Input::Execute(Action::Reconnect)
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('s'))),
            Input::Execute(Action::StopService)
        );
        app.snapshot.service = ServiceState::Inactive;
        assert_eq!(
            app.handle_key(key(KeyCode::Char('c'))),
            Input::Execute(Action::Connect(None))
        );
        for service in [
            ServiceState::Activating,
            ServiceState::Deactivating,
            ServiceState::Unavailable("cannot query systemd".into()),
        ] {
            app.snapshot.service = service;
            assert_eq!(app.handle_key(key(KeyCode::Char('s'))), Input::None);
            assert!(app.notice.is_some());
        }
        app.snapshot.service = ServiceState::Failed;
        assert_eq!(
            app.handle_key(key(KeyCode::Char('s'))),
            Input::Execute(Action::StartService)
        );
    }

    #[test]
    fn repeated_or_modified_panel_keys_do_not_run_actions() {
        let mut app = App::new(Vec::new());
        for character in "cdsri/q".chars() {
            let mut repeated = key(KeyCode::Char(character));
            repeated.kind = KeyEventKind::Repeat;
            assert_eq!(app.handle_key(repeated), Input::None);
            repeated.kind = KeyEventKind::Release;
            assert_eq!(app.handle_key(repeated), Input::None);
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::ALT)),
                Input::None
            );
        }
        assert_eq!(app.mode, Mode::Panel);
        assert!(app.query.is_empty());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Input::Exit
        );
        app.open_search();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Input::Exit
        );
    }

    #[test]
    fn empty_or_busy_picker_selection_does_not_leave_search_or_start_action() {
        let mut app = App::new(Vec::new());
        app.open_search();
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Input::None);
        assert_eq!(app.mode, Mode::Search);
        app.set_locations(vec![Location {
            country: "Japan".into(),
            city: None,
        }]);
        app.busy = Some(Action::Connect(None));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Input::None);
        assert_eq!(app.mode, Mode::Search);
    }

    #[test]
    fn search_backspace_removes_a_whole_grapheme_and_sanitizes_paste() {
        let mut app = App::new(Vec::new());
        app.open_search();
        app.paste("e\u{301}\n\u{1b}");
        assert_eq!(app.query, "e\u{301}");
        app.handle_key(key(KeyCode::Backspace));
        assert!(app.query.is_empty());
    }

    #[test]
    fn shortcut_help_is_modal_and_question_mark_stays_search_text() {
        let mut app = App::new(vec![Location {
            country: "Japan".into(),
            city: Some("Tokyo".into()),
        }]);
        assert_eq!(app.handle_key(key(KeyCode::Char('?'))), Input::None);
        assert_eq!(app.mode, Mode::Help);
        assert!(app.results.is_empty());
        for character in "cdsri/".chars() {
            assert_eq!(app.handle_key(key(KeyCode::Char(character))), Input::None);
            assert_eq!(app.mode, Mode::Help);
        }
        app.paste("ignored");
        assert!(app.query.is_empty());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Input::Execute(Action::Refresh)
        );
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Input::Exit);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Input::Exit
        );
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Panel);
        app.handle_key(key(KeyCode::Char('?')));
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Panel);
        app.open_search();
        assert_eq!(app.handle_key(key(KeyCode::Char('?'))), Input::None);
        assert_eq!(app.mode, Mode::Search);
        assert_eq!(app.query, "?");
    }

    #[test]
    fn query_start_end_movement_preserves_results_and_edits_at_cursor() {
        let mut app = App::new(vec![Location {
            country: "Japan".into(),
            city: Some("Tokyo".into()),
        }]);
        app.open_search();
        app.paste("Tokyo");
        let action = app.results[0].action.clone();
        for (character, position) in [('a', 0), ('e', "Tokyo".len())] {
            app.handle_key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::CONTROL,
            ));
            assert_eq!(app.query, "Tokyo");
            assert_eq!(app.query_cursor(), position);
            assert_eq!(app.results[app.selected].action, action);
        }
        app.handle_key(key(KeyCode::Home));
        app.paste("e\u{301}");
        assert_eq!(app.query, "e\u{301}Tokyo");
        assert_eq!(app.query_cursor(), "e\u{301}".len());
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.query, "Tokyo");
        assert_eq!(app.query_cursor(), 0);
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.query, "okyo");
        assert_eq!(app.query_cursor(), 0);
        app.handle_key(key(KeyCode::Char('T')));
        assert_eq!(app.query, "Tokyo");
        app.handle_key(key(KeyCode::End));
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.query, "Toko");
        assert_eq!(app.query_cursor(), "Tok".len());
        app.paste("y\n\u{1b}");
        assert_eq!(app.query, "Tokyo");
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(app.query.is_empty());
        assert_eq!(app.query_cursor(), 0);
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Backspace));
        assert!(app.query.is_empty());
    }

    #[test]
    fn cursor_stays_on_grapheme_boundary_when_insertion_joins_following_text() {
        let mut app = App::new(Vec::new());
        app.open_search();
        app.paste("👩💻");
        app.handle_key(key(KeyCode::Home));
        app.handle_key(key(KeyCode::Right));
        app.paste("\u{200d}");
        assert_eq!(app.query, "👩‍💻");
        assert_eq!(app.query_cursor(), app.query.len());
        app.handle_key(key(KeyCode::Backspace));
        assert!(app.query.is_empty());

        app.paste("東京");
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.query, "京");
        assert_eq!(app.query_cursor(), 0);
        app.query.clear();
        app.paste("東京");
        app.query = "e\u{301}".into();
        assert_eq!(app.query_cursor(), app.query.len());
        app.handle_key(key(KeyCode::Backspace));
        assert!(app.query.is_empty());
    }

    #[test]
    fn exact_country_query_ranks_country_above_its_cities() {
        let mut app = App::new(vec![
            Location {
                country: "United_States".into(),
                city: Some("Los_Angeles".into()),
            },
            Location {
                country: "United_States".into(),
                city: None,
            },
        ]);
        app.open_search();
        app.paste("united states");
        assert_eq!(app.results[0].kind, ItemKind::Country);
        app.query.clear();
        app.paste("United_States");
        assert_eq!(app.results[0].kind, ItemKind::Country);
    }

    #[test]
    fn progressive_locations_preserve_selected_action() {
        let mut app = App::new(vec![
            Location {
                country: "Japan".into(),
                city: None,
            },
            Location {
                country: "United_States".into(),
                city: None,
            },
        ]);
        app.open_search();
        app.move_selection(1);
        let selected = app.results[app.selected].action.clone();
        app.set_locations(vec![
            Location {
                country: "Australia".into(),
                city: None,
            },
            Location {
                country: "Japan".into(),
                city: None,
            },
            Location {
                country: "United_States".into(),
                city: None,
            },
        ]);
        assert_eq!(app.results[app.selected].action, selected);
        app.paste("no-such-location");
        assert!(app.results.is_empty());
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Input::None);
    }

    #[test]
    fn panel_retains_confirmed_state_through_work_and_incomplete_observations() {
        let mut app = App::new(Vec::new());
        assert!(app.is_initializing());
        assert!(app.is_working());
        assert!(app.panel_snapshot().is_none());
        let ready = Snapshot {
            service: ServiceState::Active,
            vpn: Some(crate::model::VpnStatus {
                state: VpnState::Disconnected,
                country: None,
                city: None,
                ip: None,
                technology: None,
            }),
        };
        app.set_snapshot(ready.clone());
        assert!(!app.is_initializing());
        assert!(!app.is_working());
        app.busy = Some(Action::Connect(None));
        let mut connected = ready.clone();
        let vpn = connected.vpn.as_mut().unwrap();
        vpn.state = VpnState::Connected;
        vpn.city = Some("Tokyo".into());
        app.set_snapshot(connected.clone());
        assert_eq!(app.snapshot, connected);
        assert_eq!(app.panel_snapshot(), Some(&ready));
        app.busy = None;
        assert_eq!(app.panel_snapshot(), Some(&ready));
        app.set_snapshot(connected.clone());
        assert_eq!(app.panel_snapshot(), Some(&connected));

        let mut connecting = connected.clone();
        connecting.vpn.as_mut().unwrap().state = VpnState::Connecting;
        for incomplete in [
            connecting,
            Snapshot {
                service: ServiceState::Activating,
                vpn: None,
            },
            Snapshot {
                service: ServiceState::Deactivating,
                vpn: None,
            },
            Snapshot {
                service: ServiceState::Active,
                vpn: None,
            },
            Snapshot {
                service: ServiceState::Active,
                vpn: Some(crate::model::VpnStatus::unavailable(
                    "Temporary read failure",
                )),
            },
            Snapshot {
                service: ServiceState::Unavailable("Temporary read failure".into()),
                vpn: None,
            },
        ] {
            app.set_snapshot(incomplete.clone());
            assert_eq!(app.snapshot, incomplete);
            assert_eq!(app.panel_snapshot(), Some(&connected));
        }
        app.set_snapshot(ready.clone());
        assert_eq!(app.panel_snapshot(), Some(&ready));
        let off = Snapshot {
            service: ServiceState::Inactive,
            vpn: None,
        };
        app.set_snapshot(off.clone());
        assert_eq!(app.panel_snapshot(), Some(&off));
    }

    #[test]
    fn action_preparation_blocks_duplicates_without_visible_action_progress() {
        let runtime = Runtime::new().unwrap();
        let mut controller = Controller::new(Backend::new(), runtime.handle().clone());
        controller.app.set_snapshot(Snapshot {
            service: ServiceState::Inactive,
            vpn: None,
        });
        controller.app.set_notice("Previous status", false);
        controller.app.busy = Some(Action::StartService);
        controller.app.action_preparing = true;
        assert!(controller.app.running_action().is_none());
        assert!(!controller.app.is_working());
        controller.execute(Action::StopService);
        assert_eq!(controller.app.busy, Some(Action::StartService));
        assert!(controller.action_job.is_none());
        assert_eq!(
            controller.app.notice.as_ref().unwrap().message,
            "Previous status"
        );
        controller.app.action_preparing = false;
        assert_eq!(controller.app.running_action(), Some(&Action::StartService));
        assert!(controller.app.is_working());
    }

    #[test]
    fn duplicate_actions_do_not_start_a_second_job() {
        let runtime = Runtime::new().unwrap();
        let mut controller = Controller::new(Backend::new(), runtime.handle().clone());
        controller.app.busy = Some(Action::StartService);
        controller.execute(Action::StopService);
        assert_eq!(controller.app.busy, Some(Action::StartService));
        assert!(controller.action_job.is_none());
    }

    #[test]
    fn footer_status_is_compact_and_identical_in_both_modes() {
        let mut app = App::new(Vec::new());
        app.set_snapshot(Snapshot {
            service: ServiceState::Active,
            vpn: Some(crate::model::VpnStatus {
                state: VpnState::Connected,
                country: Some("United_States".into()),
                city: Some("New_York".into()),
                ip: Some("192.0.2.10".into()),
                technology: Some("NORDLYNX".into()),
            }),
        });
        assert_eq!(app.status_text(), "Connected");
        app.open_search();
        assert_eq!(app.status_text(), "Connected");
        app.snapshot.service = ServiceState::Inactive;
        assert_eq!(app.status_text(), "Service stopped");
    }

    #[test]
    fn completed_refresh_does_not_leave_the_footer_showing_refresh_in_progress() {
        let runtime = Runtime::new().unwrap();
        let mut controller = Controller::new(Backend::new(), runtime.handle().clone());
        controller.app.set_notice(REFRESH_NOTICE, false);
        assert!(controller.app.is_working());
        controller.apply_status(Ok(Snapshot {
            service: ServiceState::Inactive,
            vpn: None,
        }));
        assert_eq!(
            controller.app.notice.as_ref().unwrap().message,
            "Status refreshed"
        );
        assert!(!controller.app.is_working());
        controller.app.set_notice(REFRESH_NOTICE, false);
        controller.apply_status(Err("Cannot query systemd".into()));
        let notice = controller.app.notice.as_ref().unwrap();
        assert!(notice.is_error);
        assert_eq!(notice.message, "Cannot query systemd");
        assert!(!controller.app.is_working());
    }
}
