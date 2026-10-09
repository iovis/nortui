# nortui

A small NordVPN terminal widget: an information panel with action shortcuts, a fuzzy country/city picker, and a persistent connection footer.

## Install

Requires Linux, systemd, the NordVPN CLI, and sudo. Log in with `nordvpn login` beforehand. Service controls use your existing sudo permissions.

```sh
cargo install --path . --locked
nortui
```

Or run from the checkout with `cargo run`.

## Use

The widget opens on a panel showing the service state, location, VPN IP, and technology. Use these shortcuts from the panel:

| Key | Action |
| --- | --- |
| i or / | Open location search |
| c | Connect or reconnect |
| d | Disconnect |
| s | Start or stop the service |
| r | Refresh status and locations |
| q or Escape | Close |

In location search, countries appear first and cities second when the query is empty. Type to fuzzy find either; city results include their country. Ordinary letters always become search input, including panel shortcut letters.

| Key | Search action |
| --- | --- |
| ↑ / ↓, Ctrl-K / Ctrl-J, or Ctrl-P / Ctrl-N | Select a result |
| Page Up / Page Down | Move ten results |
| Enter | Connect to the selected location and return to the panel |
| Ctrl-U | Clear search |
| Escape | Cancel search and return to the panel |

Ctrl-C closes and Ctrl-R refreshes in either mode. The panel has no persistent key legend.

**Connect** starts `nordvpnd.service` when necessary, then runs `nordvpn connect`. **Reconnect** also runs exactly `nordvpn connect`, allowing NordVPN to choose the recommended server again. **Disconnect** leaves the service running; **Stop service** stops it. The widget preserves the service's enabled/disabled configuration, and closing it preserves your connection and service state.

When service control needs authentication, nortui temporarily restores the terminal for `sudo -v`, then returns to the widget. Selecting a location returns to the panel before connecting or authenticating. It never reads or stores your password.

Status refreshes every two seconds. Countries and cities load progressively while the service runs and are cached at `$XDG_CACHE_HOME/nortui/locations.json` (normally `~/.cache/nortui/locations.json`). Cached locations remain searchable while the service is stopped. On a first launch with no cache, press **s** to start the service and load locations, or **c** to start and connect immediately. Refreshing does not start a stopped service. The service toggle waits for an observed state; if state is unavailable or changing, it reports that instead of guessing.

The right-aligned gray footer reports connection state, with country/city and IP/technology added in search when space permits. Its circle is green when connected, red when disconnected, yellow when connecting or paused, and gray when the service is stopped. The panel hides connection details when disconnected or stopped. The interface uses Catppuccin Frappé with a transparent terminal background and a #232634 selection highlight. Commands run in the background; notices appear above the footer. A failed command preserves the interface and reports its error. NordVPN queries require access to its daemon, typically through membership in the `nordvpn` group. Unknown status output is shown as unavailable.

## Hyprland widget

Launch in Kitty:

```sh
kitty --class=widget.nortui -e nortui
```

For a Hyprland Lua configuration using `hl`, add a window rule:

```lua
hl.window_rule({
  name = "nortui",
  match = { class = "widget\\.nortui" },
  center = true,
  dim_around = true,
  float = true,
  pin = true,
  size = { 880, 500 },
})
```

Use this command in your preferred keybinding to toggle the widget:

```sh
hyprclose class:widget.nortui || kitty --class=widget.nortui -e nortui
```

## Development

```sh
cargo check
cargo fmt
cargo test
cargo clippy -- -W clippy::pedantic -A clippy::missing-errors-doc -A clippy::missing-panics-doc -A clippy::must-use-candidate
cargo build
python3 tests/terminal_smoke.py
```

Rust tests use isolated fake executables for VPN and service mutations. The Python smoke tests exercise the full binary in a pseudo-terminal with fake NordVPN/systemctl/sudo programs; they do not need a VPN account, sudo privileges, or a running service.

The command parser targets NordVPN Linux 5.4.0. Account management, VPN settings, server groups, and favorites are outside the initial interface.
