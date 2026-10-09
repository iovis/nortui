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

The widget opens on a connection view with a prominent city and secondary country, followed by VPN IP and technology. When stopped or disconnected, it shows a short contextual message. The panel keeps the last confirmed information during commands and intermediate status checks, then updates when a settled state is known. On launch, its body stays blank until the first confirmed reading. Progress appears only in the gray footer, and action progress begins after authentication preparation. Service state and activity share the footer with connection status. Use these shortcuts from the panel:

| Key | Action |
| --- | --- |
| i or / | Open location search |
| c | Connect or reconnect |
| d | Disconnect |
| s | Start or stop the service |
| r | Refresh status; reload locations on next search |
| ? | Show shortcut help |
| q or Escape | Close |

In location search, countries appear first and cities second when the query is empty. Type to fuzzy find either; city results include their country. Ordinary letters always become search input, including panel shortcut letters.

| Key | Search action |
| --- | --- |
| ↑ / ↓, Ctrl-K / Ctrl-J, or Ctrl-P / Ctrl-N | Select a result |
| Page Up / Page Down | Move ten results |
| Ctrl-A / Home, Ctrl-E / End | Move to the start or end of the query |
| ← / → | Move the query cursor by one character cluster |
| Enter | Connect to the selected location and return to the panel |
| Ctrl-R | Refresh status and reload locations |
| Ctrl-U | Clear search |
| Escape | Cancel search and return to the panel |

Typing and paste insert at the query cursor. Backspace removes the character cluster before it, keeping combined accents and emoji intact. Long queries scroll horizontally to keep the cursor visible. `?` is ordinary query text in search.

From the panel, **?** opens shortcut help while keeping the connection footer visible. **?** or Escape returns to the panel, and ordinary action letters are ignored in help. Use arrows, j/k, or Ctrl-J/K to scroll; Page Up/Down scroll ten rows and Home/End jump to the first/last page. The panel has no persistent key legend.

Ctrl-C closes from any view. Ctrl-R refreshes status from any view and reloads locations immediately in search, or on the next search when used from the panel or help.

**Connect** starts `nordvpnd.service` when necessary, then runs `nordvpn connect`. **Reconnect** also runs exactly `nordvpn connect`, allowing NordVPN to choose the recommended server again. **Disconnect** leaves the service running; **Stop service** stops it. The widget preserves the service's enabled/disabled configuration, and closing it preserves your connection and service state.

When service control needs authentication, nortui temporarily restores normal terminal input for `sudo -v` inside the widget's alternate screen, then redraws the widget. Closing restores your original terminal contents without bringing back the sudo prompt. Selecting a location returns to the panel before connecting or authenticating. It never reads or stores your password.

Status refreshes every two seconds. Opening location search for the first time in each app session refreshes countries and cities in the background while the service runs. Cached names appear immediately, including when the service is stopped. Locations are cached at `$XDG_CACHE_HOME/nortui/locations.json` (normally `~/.cache/nortui/locations.json`). A completed location list is reused when search is reopened or the service is restarted, until you explicitly refresh it. Opening the panel, starting the service, and refreshing from the panel do not load locations until you open search.

Locations arrive progressively, with a braille-dot spinner beside loading progress in the footer. A requested load can finish after you return to the panel; an interrupted load waits for search before restarting. Pending actions use the same spinner, which disappears when the work finishes. On a first launch with no cache, press **s** to start the service, then **i** or **/** to open search and load locations. **c** starts the service and connects immediately to a recommended server. Refreshing does not start a stopped service. The service toggle waits for an observed state; if state is unavailable or changing, it reports that instead of guessing.

The footer uses the same compact layout in the panel, location picker, and help: service information or current activity on the left, gray connection status with one padding cell on the right. Its circle is green when connected, red when disconnected, yellow when connecting or paused, and gray when the service is stopped. The panel hides stale connection details and omits unavailable metadata. The interface uses Catppuccin Frappé with a transparent terminal background and a #232634 selection highlight, with matching margins in every view. Commands run in the background; progress and success notices appear in the footer. Errors get a separate red row above the separator when needed. A failed command preserves the interface and reports its error. NordVPN queries require access to its daemon, typically through membership in the `nordvpn` group. Unknown status output is shown as unavailable.

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
  size = { 880, 320 },
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
