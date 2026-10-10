# nortui

A small terminal interface for the NordVPN Linux CLI. Check your connection,
manage the service, and find a country or city from a keyboard-driven panel.

## Features

- Connection status, server details, VPN IP, uptime, and transfer totals.
- Connect, disconnect, reconnect, and start or stop the NordVPN service.
- Fuzzy country and city search with a persistent location cache.
- Background status updates and location loading.

## Requirements

- Linux with systemd.
- The NordVPN CLI installed and authenticated.
- Access to the NordVPN daemon and sudo permissions to manage its service.
- A Rust toolchain to build from source.

Use the NordVPN CLI to log in with `nordvpn login` and configure your account
before launching nortui.

## Installation

From a checkout of this repository:

```sh
cargo install --path . --locked
nortui
```

To run without installing:

```sh
cargo run --release
```

## Usage

Run `nortui` in an interactive terminal. The main panel shows the current
connection and available session details, with status and activity in the
footer.

### Main panel

| Key         | Action                       |
| ----------- | ---------------------------- |
| i or /      | Search countries and cities  |
| c           | Connect or reconnect         |
| d           | Disconnect                   |
| s           | Start or stop the service    |
| r           | Refresh status and locations |
| ?           | Show shortcut help           |
| q or Escape | Quit                         |

### Location search

Type to fuzzy find a country or city, then press Enter to connect. City results
include their country. Panel shortcut letters become ordinary text in search.

| Key                                     | Action                                |
| --------------------------------------- | ------------------------------------- |
| ↑ / ↓, Ctrl-K / Ctrl-J, Ctrl-P / Ctrl-N | Select a result                       |
| Page Up / Page Down                     | Move ten results                      |
| Enter                                   | Connect and return to the panel       |
| ← / →                                   | Move the query cursor                 |
| Ctrl-A / Home, Ctrl-E / End             | Move to the start or end of the query |
| Ctrl-U                                  | Clear the query                       |
| Ctrl-R                                  | Refresh status and locations          |
| Escape                                  | Return to the panel                   |

Ctrl-C quits from any view. Ctrl-R refreshes from any view; locations reload
immediately in search or the next time search opens. Press `?` in the panel for
in-app shortcut help.

### Service controls

Connecting starts `nordvpnd.service` if needed. Connecting again runs
`nordvpn connect` to choose a recommended server. Disconnecting leaves the
service running; use `s` to stop it separately. Service controls preserve the
service's enabled or disabled configuration at boot.

Service changes use sudo and may prompt for a password in the terminal. Quitting
nortui closes the interface and leaves the current VPN connection and service
state unchanged. Manage account settings and VPN options through the NordVPN
CLI.

### Location cache

Opening search refreshes locations once per session while the service is
running. Cached entries appear immediately and are available when the service is
stopped. Locations load in the background, so search remains usable during a
refresh.

The cache is stored at `$XDG_CACHE_HOME/nortui/locations.json`, or
`~/.cache/nortui/locations.json` when `XDG_CACHE_HOME` is unset.

## Development

```sh
cargo check
cargo fmt --check
cargo test -- --test-threads=1
cargo clippy -- -W clippy::pedantic -A clippy::missing-errors-doc -A clippy::missing-panics-doc -A clippy::must-use-candidate
cargo build
python3 tests/terminal_smoke.py
```

Tests do not require a VPN account, sudo privileges, or a running NordVPN
service.
