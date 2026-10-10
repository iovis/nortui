use std::{
    ffi::{OsStr, OsString},
    future::Future,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::Notify,
    task::JoinHandle,
    time::timeout,
};

use crate::model::{Action, ServiceState, Snapshot, VpnState, VpnStatus};

const SERVICE: &str = "nordvpnd.service";
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone, Debug)]
pub struct Backend {
    nordvpn: PathBuf,
    systemctl: PathBuf,
    sudo: PathBuf,
    reapers: Reapers,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Authentication {
    Completed,
    Canceled,
}

#[derive(Clone, Debug, Default)]
struct Reapers {
    state: Arc<Mutex<ProcessState>>,
    changed: Arc<Notify>,
}

#[derive(Debug, Default)]
struct ProcessState {
    closing: bool,
    active: usize,
    jobs: Vec<JoinHandle<()>>,
}

struct ManagedChild {
    child: Option<Child>,
    reapers: Reapers,
}

impl ManagedChild {
    fn new(reapers: &Reapers) -> Result<Self> {
        let mut state = reapers.state.lock().expect("Reaper lock poisoned");
        if state.closing {
            bail!("The command backend is shutting down");
        }
        state.active += 1;
        Ok(Self {
            child: None,
            reapers: reapers.clone(),
        })
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let job = self.child.take().and_then(|mut child| {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return None;
            }
            // Abort can drop capture at any await point. Kill synchronously, then
            // explicitly reap instead of relying on Tokio's deferred orphan queue.
            let _ = child.start_kill();
            tokio::runtime::Handle::try_current().ok().map(|runtime| {
                runtime.spawn(async move {
                    let _ = child.wait().await;
                })
            })
        });
        let mut state = self.reapers.state.lock().expect("Reaper lock poisoned");
        state.jobs.retain(|job| !job.is_finished());
        state.jobs.extend(job);
        state.active -= 1;
        self.reapers.changed.notify_one();
    }
}

#[derive(Debug)]
struct Captured {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Captured {
    fn message(&self) -> String {
        match (self.stdout.trim(), self.stderr.trim()) {
            ("", stderr) => stderr.to_owned(),
            (stdout, "") => stdout.to_owned(),
            (stdout, stderr) => format!("{stdout}\n{stderr}"),
        }
    }

    fn checked(self, operation: &str) -> Result<Self> {
        if self.status.success() {
            Ok(self)
        } else {
            let message = self.message();
            if message.is_empty() {
                bail!("{operation} failed ({})", self.status);
            }
            bail!("{operation} failed: {message}");
        }
    }
}

impl Default for Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend {
    pub fn new() -> Self {
        Self::with_programs("nordvpn".into(), "systemctl".into(), "sudo".into())
    }

    pub fn with_programs(nordvpn: PathBuf, systemctl: PathBuf, sudo: PathBuf) -> Self {
        Self {
            nordvpn,
            systemctl,
            sudo,
            reapers: Reapers::default(),
        }
    }

    /// Await after canceling command jobs, before shutting down the runtime.
    /// The shared backend rejects new captured commands after this call begins.
    pub async fn reap_canceled(&self) {
        loop {
            let notified = self.reapers.changed.notified();
            let (jobs, active) = {
                let mut state = self.reapers.state.lock().expect("Reaper lock poisoned");
                state.closing = true;
                (std::mem::take(&mut state.jobs), state.active)
            };
            for job in jobs {
                let _ = job.await;
            }
            if active == 0 {
                break;
            }
            notified.await;
        }
    }

    pub async fn service_state(&self) -> Result<ServiceState> {
        let result = capture(
            &self.systemctl,
            &["show", "--property=ActiveState", "--value", SERVICE],
            READ_TIMEOUT,
            &self.reapers,
        )
        .await?
        .checked("Checking the NordVPN service")?;
        Ok(parse_service_state(&result.stdout))
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        let service = self.service_state().await?;
        let vpn = if service == ServiceState::Active {
            Some(
                match capture(&self.nordvpn, &["status"], READ_TIMEOUT, &self.reapers)
                    .await
                    .and_then(|result| result.checked("Reading VPN status"))
                {
                    Ok(result) => parse_status(&result.stdout),
                    Err(error) => VpnStatus::unavailable(format!("{error:#}")),
                },
            )
        } else {
            None
        };
        Ok(Snapshot { service, vpn })
    }

    pub async fn countries(&self) -> Result<Vec<String>> {
        self.require_running_service().await?;
        let result = capture(&self.nordvpn, &["countries"], READ_TIMEOUT, &self.reapers)
            .await?
            .checked("Loading countries")?;
        parse_locations(&result.stdout)
    }

    pub async fn cities(&self, country: &str) -> Result<Vec<String>> {
        self.require_running_service().await?;
        let result = capture(
            &self.nordvpn,
            &["cities", country],
            READ_TIMEOUT,
            &self.reapers,
        )
        .await?;
        // This specific response means the country supports country-level connections only.
        // Other list failures must keep the previously cached cities intact.
        if result
            .message()
            .contains("Servers by city are not available for this country.")
        {
            return Ok(Vec::new());
        }
        let result = result.checked("Loading cities")?;
        parse_locations(&result.stdout)
    }

    pub async fn requires_auth(&self, action: &Action) -> Result<bool> {
        match action {
            Action::StartService | Action::StopService => Ok(true),
            Action::Connect(_) | Action::Reconnect => {
                let state = self.service_state().await?;
                match state {
                    ServiceState::Active => Ok(false),
                    ServiceState::Unavailable(message) => {
                        bail!("Cannot determine whether service startup is needed: {message}")
                    }
                    _ => Ok(true),
                }
            }
            Action::Disconnect | Action::Refresh => Ok(false),
        }
    }

    pub async fn has_credentials(&self) -> Result<bool> {
        Ok(
            capture(&self.sudo, &["-n", "-v"], READ_TIMEOUT, &self.reapers)
                .await?
                .status
                .success(),
        )
    }

    /// Run only after the caller has restored the terminal to ordinary input mode.
    pub async fn authenticate(&self, canceled: impl Future<Output = ()>) -> Result<Authentication> {
        let mut managed = ManagedChild::new(&self.reapers)?;
        managed.child = Some(
            Command::new(&self.sudo)
                .arg("-v")
                .env("LC_ALL", "C")
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .context("Could not run sudo authentication")?,
        );
        let child = managed
            .child
            .as_mut()
            .expect("Authentication child is present");
        let status = tokio::select! {
            biased;
            () = canceled => {
                let killed = child.kill().await;
                let reaped = child.wait().await;
                if reaped.is_err() {
                    killed.context("Could not kill canceled sudo authentication")?;
                    reaped.context("Could not reap canceled sudo authentication")?;
                }
                return Ok(Authentication::Canceled);
            }
            status = child.wait() => status.context("Could not wait for sudo authentication")?,
        };
        if !status.success() {
            bail!("Sudo authentication was canceled or failed ({status})");
        }
        Ok(Authentication::Completed)
    }

    pub async fn execute(&self, action: &Action) -> Result<String> {
        match action {
            Action::StartService => self.control_service("start").await,
            Action::StopService => self.control_service("stop").await,
            Action::Connect(location) => {
                self.ensure_running_service().await?;
                let mut arguments = vec!["connect"];
                if let Some(location) = location {
                    arguments.push(&location.country);
                    if let Some(city) = &location.city {
                        arguments.push(city);
                    }
                }
                self.run_vpn(&arguments, CONNECT_TIMEOUT, "Connection command completed")
                    .await
            }
            Action::Reconnect => {
                self.ensure_running_service().await?;
                self.run_vpn(
                    &["connect"],
                    CONNECT_TIMEOUT,
                    "Connection command completed",
                )
                .await
            }
            Action::Disconnect => {
                // A stale UI selection must not wake the socket-activated daemon after
                // another application has stopped the service.
                self.require_running_service().await?;
                self.run_vpn(&["disconnect"], CONNECT_TIMEOUT, "Disconnected")
                    .await
            }
            Action::Refresh => {
                self.snapshot().await?;
                Ok("Status refreshed".into())
            }
        }
    }

    async fn require_running_service(&self) -> Result<()> {
        match self.service_state().await? {
            ServiceState::Active => Ok(()),
            ServiceState::Unavailable(message) => bail!("Service state unavailable: {message}"),
            _ => bail!("The NordVPN service is not running"),
        }
    }

    async fn ensure_running_service(&self) -> Result<()> {
        match self.service_state().await? {
            ServiceState::Active => Ok(()),
            ServiceState::Unavailable(message) => {
                bail!("Cannot start a service with unknown state: {message}")
            }
            _ => {
                self.control_service("start").await?;
                self.require_running_service().await
            }
        }
    }

    async fn control_service(&self, verb: &str) -> Result<String> {
        let arguments: [&OsStr; 4] = [
            OsStr::new("-n"),
            self.systemctl.as_os_str(),
            OsStr::new(verb),
            OsStr::new(SERVICE),
        ];
        let result = capture(&self.sudo, &arguments, SERVICE_TIMEOUT, &self.reapers)
            .await?
            .checked("Controlling the NordVPN service")?;
        let message = result.message();
        if message.is_empty() {
            Ok(if verb == "start" {
                "Service started".into()
            } else {
                "Service stopped".into()
            })
        } else {
            Ok(message)
        }
    }

    async fn run_vpn(&self, arguments: &[&str], limit: Duration, fallback: &str) -> Result<String> {
        let result = capture(&self.nordvpn, arguments, limit, &self.reapers)
            .await?
            .checked("NordVPN command")?;
        let message = result.message();
        Ok(if message.is_empty() {
            fallback.to_owned()
        } else {
            message
        })
    }
}

async fn capture<S: AsRef<OsStr>>(
    executable: &Path,
    arguments: &[S],
    limit: Duration,
    reapers: &Reapers,
) -> Result<Captured> {
    let mut managed = ManagedChild::new(reapers)?;
    let arguments: Vec<OsString> = arguments
        .iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect();
    let name = executable
        .file_name()
        .unwrap_or(executable.as_os_str())
        .to_string_lossy();
    let child = Command::new(executable)
        .args(&arguments)
        .env("LC_ALL", "C")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Could not run {name}"))?;
    managed.child = Some(child);
    let child = managed.child.as_mut().expect("Command child is present");
    let mut stdout = child.stdout.take().context("Could not capture stdout")?;
    let mut stderr = child.stderr.take().context("Could not capture stderr")?;
    // Drain both pipes while waiting, so large stderr/stdout cannot deadlock a command.
    // The child stays owned here so timeout can explicitly kill and reap it.
    let completion = async {
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let (status, output_read, errors_read) = tokio::join!(
            child.wait(),
            stdout.read_to_end(&mut output),
            stderr.read_to_end(&mut errors),
        );
        output_read.context("Could not read command stdout")?;
        errors_read.context("Could not read command stderr")?;
        Ok::<_, anyhow::Error>(Captured {
            status: status.context("Could not wait for command")?,
            stdout: strip_terminal_sequences(&String::from_utf8_lossy(&output)),
            stderr: strip_terminal_sequences(&String::from_utf8_lossy(&errors)),
        })
    };
    if let Ok(result) = timeout(limit, completion).await {
        result
    } else {
        // kill() already waits in Tokio, but wait() also handles the race where the
        // child exited just as the deadline elapsed and makes reaping explicit.
        let kill_result = child.kill().await;
        let wait_result = child.wait().await;
        if wait_result.is_err() {
            kill_result.context("Could not kill timed-out command")?;
            wait_result.context("Could not reap timed-out command")?;
        }
        bail!("{name} timed out after {} seconds", limit.as_secs_f64());
    }
}

pub fn parse_service_state(output: &str) -> ServiceState {
    match strip_terminal_sequences(output).trim() {
        "active" | "ActiveState=active" => ServiceState::Active,
        "inactive" | "ActiveState=inactive" => ServiceState::Inactive,
        "activating" | "ActiveState=activating" => ServiceState::Activating,
        "deactivating" | "ActiveState=deactivating" => ServiceState::Deactivating,
        "failed" | "ActiveState=failed" => ServiceState::Failed,
        output => ServiceState::Unavailable(if output.is_empty() {
            "systemctl returned no service state".into()
        } else {
            format!("Unrecognized service state: {output}")
        }),
    }
}

pub fn parse_status(output: &str) -> VpnStatus {
    let output = strip_terminal_sequences(output);
    let fields: Vec<_> = output
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect();
    let field = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
            .filter(|value| !value.is_empty())
    };
    let state = match field("Status") {
        Some(value) if value.eq_ignore_ascii_case("Connected") => VpnState::Connected,
        Some(value) if value.eq_ignore_ascii_case("Connecting") => VpnState::Connecting,
        Some(value) if value.eq_ignore_ascii_case("Disconnected") => VpnState::Disconnected,
        Some(value) if value.eq_ignore_ascii_case("Paused") => VpnState::Paused,
        Some(value) => {
            return VpnStatus::unavailable(format!("Unrecognized VPN state: {value}"));
        }
        None => return VpnStatus::unavailable("NordVPN returned no recognizable Status field"),
    };
    VpnStatus {
        state,
        country: field("Country").map(str::to_owned),
        city: field("City").map(str::to_owned),
        ip: field("IP").map(str::to_owned),
        server: field("Server")
            .or_else(|| field("Current server"))
            .or_else(|| field("Hostname"))
            .map(str::to_owned),
        technology: field("Current technology")
            .or_else(|| field("Technology"))
            .map(str::to_owned),
        protocol: field("Current protocol")
            .or_else(|| field("Protocol"))
            .map(str::to_owned),
        uptime: field("Uptime").and_then(parse_uptime),
        received: field("Transfer").and_then(|value| parse_transferred(value, "received")),
        sent: field("Transfer").and_then(|value| parse_transferred(value, "sent")),
    }
}

fn parse_transferred(input: &str, direction: &str) -> Option<String> {
    let mut amounts = input.split(',').filter_map(|clause| {
        let mut words = clause.split_whitespace();
        let amount = words.next()?;
        let unit = words.next()?;
        let label = words.next()?;
        if words.next().is_some() || !label.eq_ignore_ascii_case(direction) {
            return None;
        }
        let mut parts = amount.split('.');
        let integer = parts.next()?;
        let fraction = parts.next();
        let digits =
            |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
        if !digits(integer)
            || fraction.is_some_and(|part| !digits(part))
            || parts.next().is_some()
            || !matches!(
                unit.to_ascii_lowercase().as_str(),
                "b" | "byte"
                    | "bytes"
                    | "kb"
                    | "mb"
                    | "gb"
                    | "tb"
                    | "pb"
                    | "eb"
                    | "kib"
                    | "mib"
                    | "gib"
                    | "tib"
                    | "pib"
                    | "eib"
            )
        {
            return None;
        }
        Some(format!("{amount} {unit}"))
    });
    let amount = amounts.next()?;
    // A duplicated direction is ambiguous instead of a trustworthy cumulative total.
    if amounts.next().is_some() {
        return None;
    }
    Some(amount)
}

fn parse_uptime(input: &str) -> Option<Duration> {
    let mut words = input.split_whitespace();
    let mut seconds = 0_u64;
    let mut seen = false;
    while let Some(value) = words.next() {
        if !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let value: u64 = value.parse().ok()?;
        let unit = words.next()?;
        let multiplier = match unit.to_ascii_lowercase().as_str() {
            "second" | "seconds" => 1,
            "minute" | "minutes" => 60,
            "hour" | "hours" => 3_600,
            "day" | "days" => 86_400,
            "week" | "weeks" => 604_800,
            _ => return None,
        };
        seconds = seconds.checked_add(value.checked_mul(multiplier)?)?;
        seen = true;
    }
    seen.then(|| Duration::from_secs(seconds))
}

pub fn parse_locations(output: &str) -> Result<Vec<String>> {
    let output = strip_terminal_sequences(output);
    let mut names = Vec::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        // The current non-TTY CLI prints raw underscore-delimited tokens, either one
        // per line or in columns. Reject prose rather than caching an error as places.
        if matches!(line.to_ascii_lowercase().as_str(), "countries:" | "cities:") {
            continue;
        }
        if line.contains("://") || line.ends_with('.') || line.contains(':') {
            bail!("Unexpected location-list output: {line}");
        }
        for token in line.split_whitespace() {
            if !valid_location_token(token) {
                bail!("Unexpected location token: {token}");
            }
            names.push(token.to_owned());
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn valid_location_token(token: &str) -> bool {
    token.starts_with(char::is_alphabetic)
        && token.chars().all(|character| {
            character.is_alphanumeric()
                || matches!(character, '_' | '-' | '.' | '\'' | '’' | '(' | ')')
        })
}

/// Remove ANSI escape sequences and control characters before rendering command output.
pub fn strip_terminal_sequences(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' => match characters.next() {
                Some('[') => {
                    for character in characters.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&character) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(character) = characters.next() {
                        if character == '\u{7}' {
                            break;
                        }
                        if character == '\u{1b}' && characters.peek() == Some(&'\\') {
                            characters.next();
                            break;
                        }
                    }
                }
                Some(character) if ('\u{20}'..='\u{2f}').contains(&character) => {
                    for character in characters.by_ref() {
                        if ('\u{30}'..='\u{7e}').contains(&character) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {
                if characters.peek() != Some(&'\n') {
                    output.push('\n');
                }
            }
            '\n' | '\t' => output.push(character),
            character if !character.is_control() => output.push(character),
            _ => {}
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use tempfile::TempDir;

    use super::*;
    use crate::model::Location;

    struct Fixture {
        directory: TempDir,
        backend: Backend,
    }

    impl Fixture {
        fn new(state: &str) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            fs::write(root.join("state"), state).unwrap();
            fs::write(root.join("status"), "Status: Disconnected\n").unwrap();
            fs::write(root.join("countries"), "United_States\nUnited_Kingdom\n").unwrap();
            fs::write(root.join("cities"), "New_York\nSalt_Lake_City\n").unwrap();
            fs::write(root.join("log"), "").unwrap();
            let systemctl = program(
                root,
                "systemctl",
                r#"
printf 'systemctl' >> "$root/log"
for argument in "$@"; do printf '|%s' "$argument" >> "$root/log"; done
printf '\n' >> "$root/log"
case "$1" in
  show) cat "$root/state" ;;
  start)
    if [ -f "$root/fail-start" ]; then printf 'startup failed\n' >&2; exit 1; fi
    printf active > "$root/state" ;;
  stop) printf inactive > "$root/state" ;;
  *) exit 2 ;;
esac
"#,
            );
            let sudo = program(
                root,
                "sudo",
                r#"
printf 'sudo' >> "$root/log"
for argument in "$@"; do printf '|%s' "$argument" >> "$root/log"; done
printf '\n' >> "$root/log"
if [ "$1" = -n ] && [ "$2" = -v ]; then
  if [ -f "$root/no-credentials" ]; then exit 1; fi
  exit 0
fi
if [ "$1" = -v ]; then
  if [ -f "$root/auth-fails" ]; then exit 1; fi
  exit 0
fi
if [ "$1" = -n ]; then shift; exec "$@"; fi
exit 2
"#,
            );
            let nordvpn = program(
                root,
                "nordvpn",
                r#"
printf 'nordvpn' >> "$root/log"
for argument in "$@"; do printf '|%s' "$argument" >> "$root/log"; done
printf '\n' >> "$root/log"
case "$1" in
  status)
    cat "$root/status"
    if [ -f "$root/fail-status" ]; then printf 'login required\n' >&2; exit 1; fi ;;
  countries) cat "$root/countries" ;;
  cities)
    if [ -f "$root/no-cities" ]; then
      printf 'Servers by city are not available for this country.\n' >&2
      exit 1
    fi
    if [ -f "$root/fail-cities" ]; then printf 'Service unavailable\n' >&2; exit 1; fi
    cat "$root/cities" ;;
  connect) printf '\033[32mConnected\033[0m\n' ;;
  disconnect) printf 'Disconnected\n' ;;
  *) exit 2 ;;
esac
"#,
            );
            Self {
                directory,
                backend: Backend::with_programs(nordvpn, systemctl, sudo),
            }
        }

        fn mark(&self, name: &str) {
            fs::write(self.directory.path().join(name), "").unwrap();
        }

        fn log(&self) -> String {
            fs::read_to_string(self.directory.path().join("log")).unwrap()
        }
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
    }

    fn program(root: &Path, name: &str, body: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(
            &path,
            format!("#!/bin/sh\nroot={}\n{body}", shell_quote(root)),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn parses_known_status_states_and_connection_fields() {
        let connected = parse_status(
            "\x1b[32mStatus: Connected\x1b[0m\r\nCountry: United States\nCity: New York\nIP: 203.0.113.10\nCurrent technology: NORDLYNX\n",
        );
        assert_eq!(connected.state, VpnState::Connected);
        assert_eq!(connected.country.as_deref(), Some("United States"));
        assert_eq!(connected.city.as_deref(), Some("New York"));
        assert_eq!(connected.ip.as_deref(), Some("203.0.113.10"));
        assert_eq!(connected.technology.as_deref(), Some("NORDLYNX"));
        for (label, state) in [
            ("Connecting", VpnState::Connecting),
            ("Disconnected", VpnState::Disconnected),
            ("Paused", VpnState::Paused),
        ] {
            assert_eq!(parse_status(&format!("Status: {label}\n")).state, state);
        }
    }

    #[test]
    fn parses_connected_session_details_from_complete_status() {
        let status = parse_status(
            "Status: Connected\nServer: United States #8373\nHostname: us8373.nordvpn.com\nIP: 203.0.113.10\nCountry: United States\nCity: New York\nCurrent technology: NORDLYNX\nCurrent protocol: UDP\nPost-quantum VPN: Disabled\nTransfer: 98.95 MiB received, 15.96 MiB sent\nUptime: 4 minutes 10 seconds\n",
        );
        assert_eq!(status.state, VpnState::Connected);
        assert_eq!(status.server.as_deref(), Some("United States #8373"));
        assert_eq!(status.protocol.as_deref(), Some("UDP"));
        assert_eq!(status.uptime, Some(Duration::from_secs(250)));
        assert_eq!(status.received.as_deref(), Some("98.95 MiB"));
        assert_eq!(status.sent.as_deref(), Some("15.96 MiB"));
    }

    #[test]
    fn older_aliases_and_empty_primary_fields_use_available_session_details() {
        let status = parse_status(
            "Status: Connected\nServer: \nCurrent server: Germany #42\nHostname: de42.nordvpn.com\nCurrent technology: \nTechnology: OpenVPN\nCurrent protocol: \nProtocol: TCP\n",
        );
        assert_eq!(status.server.as_deref(), Some("Germany #42"));
        assert_eq!(status.technology.as_deref(), Some("OpenVPN"));
        assert_eq!(status.protocol.as_deref(), Some("TCP"));
        assert_eq!(status.uptime, None);
        assert_eq!(status.received, None);
        assert_eq!(status.sent, None);

        let hostname = parse_status("Status: Connected\nHostname: us8373.nordvpn.com\n");
        assert_eq!(hostname.server.as_deref(), Some("us8373.nordvpn.com"));
        let preferred = parse_status(
            "Status: Connected\nServer: United States #8373\nCurrent server: Germany #42\nCurrent protocol: UDP\nProtocol: TCP\n",
        );
        assert_eq!(preferred.server.as_deref(), Some("United States #8373"));
        assert_eq!(preferred.protocol.as_deref(), Some("UDP"));
    }

    #[test]
    fn malformed_or_missing_optional_telemetry_keeps_recognizable_vpn_state() {
        for telemetry in [
            "",
            "Uptime: unknown\nTransfer: unavailable\n",
            "Uptime: 5\nTransfer: -1 MiB received, NaN MiB sent\n",
            "Uptime: 1.5 minutes\nTransfer: 42 MiB/s received, 12. MiB sent\n",
            "Uptime: +5 seconds\nTransfer: 1..2 KiB received, .5 B sent\n",
            "Uptime: 1 second garbage\nTransfer: 1 MiB received per second, 3 parsecs sent\n",
        ] {
            let status = parse_status(&format!("Status: Connected\n{telemetry}"));
            assert_eq!(status.state, VpnState::Connected, "{telemetry}");
            assert_eq!(status.uptime, None, "{telemetry}");
            assert_eq!(status.received, None, "{telemetry}");
            assert_eq!(status.sent, None, "{telemetry}");
        }
        let unavailable = VpnStatus::unavailable("offline");
        assert_eq!(unavailable.server, None);
        assert_eq!(unavailable.protocol, None);
        assert_eq!(unavailable.uptime, None);
        assert_eq!(unavailable.received, None);
        assert_eq!(unavailable.sent, None);
    }

    #[test]
    fn transfer_totals_are_tagged_independently_and_preserve_display_units() {
        let reversed = parse_status(
            "Status: Connected\nTransfer: 0 B sent, 0.00 KiB received\nUptime: 0 seconds\n",
        );
        assert_eq!(reversed.received.as_deref(), Some("0.00 KiB"));
        assert_eq!(reversed.sent.as_deref(), Some("0 B"));
        assert_eq!(reversed.uptime, Some(Duration::ZERO));

        let partial = parse_status("Status: Connected\nTransfer: 3.50 GB RECEIVED, bad MiB sent\n");
        assert_eq!(partial.received.as_deref(), Some("3.50 GB"));
        assert_eq!(partial.sent, None);
        let duplicated = parse_status(
            "Status: Connected\nTransfer: 1 MiB received, 2 MiB received, 4 bytes sent\n",
        );
        assert_eq!(duplicated.received, None);
        assert_eq!(duplicated.sent.as_deref(), Some("4 bytes"));
        let sent_only = parse_status("Status: Connected\nTransfer: 1 byte sent\n");
        assert_eq!(sent_only.received, None);
        assert_eq!(sent_only.sent.as_deref(), Some("1 byte"));
    }

    #[test]
    fn uptime_accepts_human_units_and_checks_duration_boundaries() {
        for (text, seconds) in [
            ("1 second", 1),
            ("60 seconds", 60),
            ("59 minutes 59 seconds", 3_599),
            ("1 hour", 3_600),
            ("1 day 1 hour 1 minute 1 second", 90_061),
            ("2 weeks 3 days 4 hours 5 minutes 6 seconds", 1_483_506),
            ("  1 WEEK  1 DAY  ", 691_200),
            ("18446744073709551615 seconds", u64::MAX),
        ] {
            assert_eq!(
                parse_uptime(text),
                Some(Duration::from_secs(seconds)),
                "{text}"
            );
        }
        for text in [
            "",
            "seconds",
            "1 fortnight",
            "1 second 2",
            "1.1 seconds",
            "-1 seconds",
            "18446744073709551616 seconds",
            "18446744073709551615 minutes",
            "18446744073709551615 seconds 1 second",
        ] {
            assert_eq!(parse_uptime(text), None, "{text}");
        }
    }

    #[test]
    fn unfamiliar_or_missing_status_is_unavailable() {
        for output in [
            "",
            "Disconnected",
            "Status: Something new",
            "Please log in.",
        ] {
            assert!(matches!(
                parse_status(output).state,
                VpnState::Unavailable(_)
            ));
        }
    }

    #[test]
    fn parses_locations_as_raw_tokens_in_single_or_multiple_columns() {
        let expected = vec!["New_York", "Salt_Lake_City", "São_Paulo"];
        assert_eq!(
            parse_locations("New_York\nSão_Paulo\nSalt_Lake_City\nNew_York\n").unwrap(),
            expected
        );
        assert_eq!(
            parse_locations("New_York    Salt_Lake_City\n\x1b[34mSão_Paulo\x1b[0m\n").unwrap(),
            expected
        );
        assert_eq!(parse_locations("\n\t\n").unwrap(), Vec::<String>::new());
        assert!(parse_locations("Please log in.").is_err());
        assert!(parse_locations("--help").is_err());
    }

    #[test]
    fn sanitizes_csi_osc_strings_and_control_characters() {
        assert_eq!(
            strip_terminal_sequences(
                "\x1b[31mRed\x1b[0m\x1b]0;title\x07\x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\\r\nnext\x00\x08\x7f\t✓",
            ),
            "Redlink\nnext\t✓"
        );
        assert_eq!(strip_terminal_sequences("\x1bPignored\x1b\\ok\x1b(B"), "ok");
    }

    #[test]
    fn parses_service_states_without_inventing_inactive_for_unknown_output() {
        assert_eq!(parse_service_state("active\n"), ServiceState::Active);
        assert_eq!(
            parse_service_state("ActiveState=failed\n"),
            ServiceState::Failed
        );
        assert_eq!(parse_service_state("activating"), ServiceState::Activating);
        assert_eq!(
            parse_service_state("deactivating"),
            ServiceState::Deactivating
        );
        for output in ["", "unknown", "active\ninactive"] {
            assert!(matches!(
                parse_service_state(output),
                ServiceState::Unavailable(_)
            ));
        }
    }

    #[tokio::test]
    async fn stopped_service_polling_and_location_queries_never_invoke_nordvpn() {
        let fixture = Fixture::new("inactive");
        let snapshot = fixture.backend.snapshot().await.unwrap();
        assert_eq!(snapshot.service, ServiceState::Inactive);
        assert!(snapshot.vpn.is_none());
        assert!(fixture.backend.countries().await.is_err());
        assert!(fixture.backend.cities("United_States").await.is_err());
        assert!(fixture.backend.execute(&Action::Disconnect).await.is_err());
        assert!(!fixture.log().contains("nordvpn|"));
    }

    #[tokio::test]
    async fn reconnect_is_one_connect_command_without_disconnect_or_service_control() {
        let fixture = Fixture::new("active");
        assert_eq!(
            fixture.backend.execute(&Action::Reconnect).await.unwrap(),
            "Connected"
        );
        let log = fixture.log();
        let calls: Vec<_> = log
            .lines()
            .filter(|line| line.starts_with("nordvpn|"))
            .collect();
        assert_eq!(calls, ["nordvpn|connect"]);
        assert!(!log.contains("sudo|"));
    }

    #[tokio::test]
    async fn city_connect_starts_inactive_service_then_sends_separate_country_and_city() {
        let fixture = Fixture::new("inactive");
        let action = Action::Connect(Some(Location {
            country: "United_States".into(),
            city: Some("New_York".into()),
        }));
        assert!(fixture.backend.requires_auth(&action).await.unwrap());
        fixture.backend.execute(&action).await.unwrap();
        let log = fixture.log();
        let start = log.find("systemctl|start|nordvpnd.service").unwrap();
        let connect = log.find("nordvpn|connect|United_States|New_York").unwrap();
        assert!(start < connect);
        assert!(log.contains("sudo|-n|"));
        assert_eq!(
            fixture.backend.service_state().await.unwrap(),
            ServiceState::Active
        );
    }

    #[tokio::test]
    async fn recommended_and_country_connections_keep_their_exact_arguments() {
        let fixture = Fixture::new("active");
        fixture
            .backend
            .execute(&Action::Connect(None))
            .await
            .unwrap();
        fixture
            .backend
            .execute(&Action::Connect(Some(Location {
                country: "United_Kingdom".into(),
                city: None,
            })))
            .await
            .unwrap();
        fixture.backend.execute(&Action::Disconnect).await.unwrap();
        let log = fixture.log();
        let calls: Vec<_> = log
            .lines()
            .filter(|line| line.starts_with("nordvpn|"))
            .collect();
        assert_eq!(
            calls,
            [
                "nordvpn|connect",
                "nordvpn|connect|United_Kingdom",
                "nordvpn|disconnect"
            ]
        );
        assert!(!log.contains("sudo|"));
    }

    #[tokio::test]
    async fn failed_service_start_and_unknown_state_prevent_connection() {
        let fixture = Fixture::new("inactive");
        fixture.mark("fail-start");
        assert!(
            fixture
                .backend
                .execute(&Action::Connect(None))
                .await
                .is_err()
        );
        assert!(!fixture.log().contains("nordvpn|"));
        fs::write(fixture.directory.path().join("state"), "unknown").unwrap();
        assert!(
            fixture
                .backend
                .requires_auth(&Action::Reconnect)
                .await
                .is_err()
        );
        assert!(fixture.backend.execute(&Action::Reconnect).await.is_err());
        assert!(!fixture.log().contains("nordvpn|"));
    }

    #[tokio::test]
    async fn failed_and_unfamiliar_status_remain_active_but_vpn_unavailable() {
        let fixture = Fixture::new("active");
        fixture.mark("fail-status");
        let snapshot = fixture.backend.snapshot().await.unwrap();
        assert_eq!(snapshot.service, ServiceState::Active);
        assert!(matches!(
            snapshot.vpn.unwrap().state,
            VpnState::Unavailable(_)
        ));
        fs::remove_file(fixture.directory.path().join("fail-status")).unwrap();
        fs::write(
            fixture.directory.path().join("status"),
            "Unexpected status output",
        )
        .unwrap();
        assert!(matches!(
            fixture.backend.snapshot().await.unwrap().vpn.unwrap().state,
            VpnState::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn countries_without_city_support_are_empty_but_other_failures_are_errors() {
        let fixture = Fixture::new("active");
        assert_eq!(
            fixture.backend.countries().await.unwrap(),
            ["United_Kingdom", "United_States"]
        );
        assert_eq!(
            fixture.backend.cities("United_States").await.unwrap(),
            ["New_York", "Salt_Lake_City"]
        );
        fixture.mark("no-cities");
        assert_eq!(
            fixture.backend.cities("United_States").await.unwrap(),
            Vec::<String>::new()
        );
        fs::remove_file(fixture.directory.path().join("no-cities")).unwrap();
        fixture.mark("fail-cities");
        assert!(fixture.backend.cities("United_States").await.is_err());
    }

    #[tokio::test]
    async fn service_actions_use_sudo_without_enabling_or_disabling_the_unit() {
        let fixture = Fixture::new("inactive");
        assert!(
            fixture
                .backend
                .requires_auth(&Action::StartService)
                .await
                .unwrap()
        );
        assert!(
            fixture
                .backend
                .requires_auth(&Action::StopService)
                .await
                .unwrap()
        );
        fixture
            .backend
            .execute(&Action::StartService)
            .await
            .unwrap();
        fixture.backend.execute(&Action::StopService).await.unwrap();
        assert_eq!(
            fixture.backend.service_state().await.unwrap(),
            ServiceState::Inactive
        );
        let log = fixture.log();
        assert!(log.contains("systemctl|start|nordvpnd.service"));
        assert!(log.contains("systemctl|stop|nordvpnd.service"));
        assert!(!log.contains("enable"));
        assert!(!log.contains("disable"));
        assert!(!log.contains("nordvpn|"));
    }

    #[tokio::test]
    async fn credentials_check_never_prompts_and_authentication_reports_failure() {
        let fixture = Fixture::new("inactive");
        assert!(fixture.backend.has_credentials().await.unwrap());
        fixture.mark("no-credentials");
        assert!(!fixture.backend.has_credentials().await.unwrap());
        assert!(fixture.log().contains("sudo|-n|-v"));
        assert_eq!(
            fixture
                .backend
                .authenticate(std::future::pending())
                .await
                .unwrap(),
            Authentication::Completed
        );
        fixture.mark("auth-fails");
        assert!(
            fixture
                .backend
                .authenticate(std::future::pending())
                .await
                .is_err()
        );
        assert!(fixture.log().contains("sudo|-v"));
    }

    #[tokio::test]
    async fn canceling_authentication_kills_and_reaps_without_running_an_action() {
        let fixture = Fixture::new("inactive");
        program(
            fixture.directory.path(),
            "sudo",
            "printf 'sudo|-v\\n' >> \"$root/log\"\nprintf '%s' \"$$\" > \"$root/pid\"\nexec sleep 30\n",
        );
        let pid_file = fixture.directory.path().join("pid");
        let canceled = async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let result = timeout(
            Duration::from_secs(2),
            fixture.backend.authenticate(canceled),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, Authentication::Canceled);
        let pid = fs::read_to_string(pid_file).unwrap();
        assert!(!Path::new("/proc").join(pid).exists());
        assert_eq!(fixture.log(), "sudo|-v\n");
        assert_eq!(
            fs::read_to_string(fixture.directory.path().join("state")).unwrap(),
            "inactive"
        );
    }

    #[tokio::test]
    async fn an_existing_exit_request_takes_precedence_over_authentication() {
        let fixture = Fixture::new("inactive");
        assert_eq!(
            fixture
                .backend
                .authenticate(std::future::ready(()))
                .await
                .unwrap(),
            Authentication::Canceled
        );
    }

    #[tokio::test]
    async fn shutdown_reaps_authentication_if_its_future_is_aborted() {
        let fixture = Fixture::new("inactive");
        program(
            fixture.directory.path(),
            "sudo",
            "printf '%s' \"$$\" > \"$root/pid\"\nexec sleep 30\n",
        );
        let backend = fixture.backend.clone();
        let job = tokio::spawn(async move { backend.authenticate(std::future::pending()).await });
        let pid_file = fixture.directory.path().join("pid");
        timeout(Duration::from_secs(2), async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let pid = fs::read_to_string(pid_file).unwrap();
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        timeout(Duration::from_secs(2), fixture.backend.reap_canceled())
            .await
            .unwrap();
        assert!(!Path::new("/proc").join(pid).exists());
        let error = fixture
            .backend
            .authenticate(std::future::pending())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("shutting down"));
    }

    #[tokio::test]
    async fn timeout_kills_and_reaps_the_owned_process() {
        let directory = tempfile::tempdir().unwrap();
        let executable = program(
            directory.path(),
            "slow",
            "printf '%s' \"$$\" > \"$root/pid\"\nexec sleep 30\n",
        );
        let result = capture(
            &executable,
            &[] as &[&str],
            Duration::from_millis(100),
            &Reapers::default(),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("timed out"));
        let pid = fs::read_to_string(directory.path().join("pid")).unwrap();
        assert!(!Path::new("/proc").join(pid).exists());
    }

    #[tokio::test]
    async fn canceling_capture_kills_its_child_without_extra_vpn_commands() {
        let directory = tempfile::tempdir().unwrap();
        let executable = program(
            directory.path(),
            "slow",
            "printf '%s' \"$$\" > \"$root/pid\"\nexec sleep 30\n",
        );
        let job = tokio::spawn(async move {
            capture(
                &executable,
                &[] as &[&str],
                Duration::from_secs(10),
                &Reapers::default(),
            )
            .await
        });
        let pid_file = directory.path().join("pid");
        timeout(Duration::from_secs(2), async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let pid = fs::read_to_string(pid_file).unwrap();
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        timeout(Duration::from_secs(2), async {
            while Path::new("/proc").join(&pid).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn shutdown_reaps_aborted_nested_captures_and_rejects_late_commands() {
        let directory = tempfile::tempdir().unwrap();
        let executable = program(
            directory.path(),
            "slow",
            "printf '%s' \"$$\" > \"$root/pid-$1\"\nexec sleep 30\n",
        );
        let backend = Backend::new();
        let mut children = tokio::task::JoinSet::new();
        for identifier in ["1", "2"] {
            let executable = executable.clone();
            let reapers = backend.reapers.clone();
            children.spawn(async move {
                capture(
                    &executable,
                    &[identifier],
                    Duration::from_secs(10),
                    &reapers,
                )
                .await
            });
        }
        timeout(Duration::from_secs(2), async {
            while !directory.path().join("pid-1").exists()
                || !directory.path().join("pid-2").exists()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let pids = ["pid-1", "pid-2"]
            .map(|filename| fs::read_to_string(directory.path().join(filename)).unwrap());
        drop(children);
        timeout(Duration::from_secs(2), backend.reap_canceled())
            .await
            .unwrap();
        for pid in pids {
            assert!(!Path::new("/proc").join(pid).exists());
        }
        let error = backend.service_state().await.unwrap_err();
        assert!(error.to_string().contains("shutting down"));
    }
}
