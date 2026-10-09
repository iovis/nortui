use std::fmt::Write;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as InputEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::DefaultTerminal;
use ratatui::widgets::ListState;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::JoinHandle;
use unicode_segmentation::UnicodeSegmentation;

use crate::backend::Backend;
use crate::cache::{self, Catalog};
use crate::model::{Action, Location, ServiceState, Snapshot, VpnState};
use crate::ui;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ItemKind {
    Command,
    Country,
    City,
}

impl ItemKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Command => "Command",
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
    pub query: String,
    pub snapshot: Snapshot,
    pub locations: Vec<Location>,
    pub results: Vec<SearchResult>,
    pub selected: usize,
    pub list_state: ListState,
    pub notice: Option<Notice>,
    pub busy: Option<Action>,
    pub loading: Option<(usize, usize)>,
    matcher: Matcher,
}

impl App {
    pub fn new(locations: Vec<Location>) -> Self {
        let mut app = Self {
            query: String::new(),
            snapshot: Snapshot::default(),
            locations,
            results: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
            notice: None,
            busy: None,
            loading: None,
            matcher: Matcher::new(Config::DEFAULT),
        };
        app.rebuild(false);
        app
    }

    pub fn set_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshot = snapshot;
        self.rebuild(true);
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
            !notice.is_error && notice.created.elapsed() > Duration::from_secs(8)
        }) {
            self.notice = None;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Input {
        if key.kind == KeyEventKind::Release {
            return Input::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => Input::Exit,
                KeyCode::Char('n') => {
                    self.move_selection(1);
                    Input::None
                }
                KeyCode::Char('p') => {
                    self.move_selection(-1);
                    Input::None
                }
                KeyCode::Char('u') => {
                    self.query.clear();
                    self.rebuild(false);
                    Input::None
                }
                KeyCode::Char('r') => Input::Execute(Action::Refresh),
                _ => Input::None,
            };
        }
        match key.code {
            KeyCode::Esc if self.query.is_empty() => Input::Exit,
            KeyCode::Esc => {
                self.query.clear();
                self.rebuild(false);
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
            KeyCode::Enter => self
                .results
                .get(self.selected)
                .map_or(Input::None, |row| Input::Execute(row.action.clone())),
            KeyCode::Backspace => {
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                }
                self.rebuild(false);
                Input::None
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.push(character);
                self.rebuild(false);
                Input::None
            }
            _ => Input::None,
        }
    }

    pub fn paste(&mut self, value: &str) {
        self.query
            .extend(value.chars().filter(|character| !character.is_control()));
        self.rebuild(false);
    }

    pub fn status_text(&self, details: bool) -> String {
        match &self.snapshot.service {
            ServiceState::Inactive => "Service stopped".into(),
            ServiceState::Activating => "Service starting…".into(),
            ServiceState::Deactivating => "Service stopping…".into(),
            ServiceState::Failed => "Service failed".into(),
            ServiceState::Unavailable(_) => "Service unavailable".into(),
            ServiceState::Active => self.vpn_text(details),
        }
    }

    fn vpn_text(&self, details: bool) -> String {
        let Some(vpn) = &self.snapshot.vpn else {
            return "Service running · Checking VPN…".into();
        };
        let mut text = match vpn.state {
            VpnState::Connected => "Connected".to_owned(),
            VpnState::Connecting => "Service running · Connecting…".to_owned(),
            VpnState::Disconnected => "Service running · Disconnected".to_owned(),
            VpnState::Paused => "Service running · Paused".to_owned(),
            VpnState::Unavailable(_) => "Service running · VPN unavailable".to_owned(),
        };
        if matches!(vpn.state, VpnState::Connected) {
            if let Some(country) = &vpn.country {
                let _ = write!(text, " · {}", country.replace('_', " "));
                if let Some(city) = &vpn.city {
                    let _ = write!(text, " / {}", city.replace('_', " "));
                }
            }
            if details {
                for value in [&vpn.ip, &vpn.technology].into_iter().flatten() {
                    let _ = write!(text, " · {value}");
                }
            }
        }
        text
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
        let mut rows = Vec::new();
        let connected = self
            .snapshot
            .vpn
            .as_ref()
            .is_some_and(|vpn| matches!(vpn.state, VpnState::Connected));
        let connect = if connected {
            Action::Reconnect
        } else {
            Action::Connect(None)
        };
        rows.push(command_row(
            if connected {
                "Reconnect"
            } else {
                "Connect recommended"
            },
            connect,
        ));
        if self.snapshot.service == ServiceState::Active {
            if self.snapshot.vpn.as_ref().is_some_and(|vpn| {
                !matches!(vpn.state, VpnState::Disconnected | VpnState::Unavailable(_))
            }) {
                rows.push(command_row("Disconnect", Action::Disconnect));
            }
            rows.push(command_row("Stop service", Action::StopService));
        } else if matches!(
            self.snapshot.service,
            ServiceState::Inactive | ServiceState::Failed
        ) {
            rows.push(command_row("Start service", Action::StartService));
        }
        rows.push(command_row("Refresh", Action::Refresh));
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
        rows.extend(locations);
        rows
    }

    fn rebuild(&mut self, preserve: bool) {
        let selected_action = preserve
            .then(|| {
                self.results
                    .get(self.selected)
                    .map(|row| row.action.clone())
            })
            .flatten();
        let query = self.query.replace('_', " ");
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
        if !self.query.is_empty() {
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

fn command_row(label: &str, action: Action) -> SearchResult {
    SearchResult {
        label: label.into(),
        kind: ItemKind::Command,
        action,
        matches: Vec::new(),
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
                    "{error}; locations will reload when the service starts"
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
            self.app
                .set_notice("Refreshing status and locations…", false);
            self.spawn_status();
            return;
        }
        self.app.busy = Some(action.clone());
        self.app.set_notice(action.label(), false);
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
        if snapshot.service == ServiceState::Active
            && self.app.snapshot.service != ServiceState::Active
        {
            self.discovery_requested = true;
        }
        if snapshot.service != ServiceState::Active {
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

struct TerminalSession {
    terminal: DefaultTerminal,
}

impl TerminalSession {
    fn new() -> Result<Self> {
        let terminal = ratatui::try_init().context("Cannot initialize terminal")?;
        let session = Self { terminal };
        execute!(io::stdout(), EnableBracketedPaste)?;
        Ok(session)
    }

    fn suspend<T>(&mut self, operation: impl FnOnce() -> T) -> Result<T> {
        execute!(io::stdout(), DisableBracketedPaste)?;
        ratatui::restore();
        let result = operation();
        self.terminal =
            ratatui::try_init().context("Cannot restore terminal after authentication")?;
        execute!(io::stdout(), EnableBracketedPaste)?;
        // Fullscreen reinitialization already resets Ratatui's buffers. Clear
        // directly: Terminal::clear queries cursor position unnecessarily here.
        execute!(io::stdout(), Clear(ClearType::All))?;
        Ok(result)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        ratatui::restore();
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
    result
}

fn run_loop(controller: &mut Controller) -> Result<()> {
    let mut terminal = TerminalSession::new()?;
    controller.spawn_status();
    loop {
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
    }

    #[test]
    fn ordinary_q_is_search_and_escape_clears_before_exiting() {
        let mut app = App::new(Vec::new());
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Input::None);
        assert_eq!(app.query, "q");
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Input::None);
        assert!(app.query.is_empty());
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Input::Exit);
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
        app.paste("united states");
        assert_eq!(app.results[0].kind, ItemKind::Country);
        app.query.clear();
        app.paste("United_States");
        assert_eq!(app.results[0].kind, ItemKind::Country);
    }

    #[test]
    fn progressive_locations_preserve_selected_action() {
        let mut app = App::new(Vec::new());
        app.move_selection(1);
        assert_eq!(app.results[app.selected].action, Action::Refresh);
        app.set_locations(vec![Location {
            country: "Japan".into(),
            city: None,
        }]);
        assert_eq!(app.results[app.selected].action, Action::Refresh);
        app.paste("no-such-location");
        assert!(app.results.is_empty());
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Input::None);
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
}
