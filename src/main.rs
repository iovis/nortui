use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    match arguments.as_slice() {
        [] => match nortui::app::run(nortui::backend::Backend::new()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                let _ = writeln!(io::stderr(), "nortui: {error:#}");
                ExitCode::FAILURE
            }
        },
        [argument] if argument == "--help" || argument == "-h" => {
            println!(
                "nortui — a NordVPN information panel and location picker\n\nUsage: nortui [--help | --version]\n\nPanel: i or /: search   c: connect/reconnect   d: disconnect\ns: start/stop service   r: refresh   q or Esc: close\n\nSearch: type countries/cities   ↑/↓, Ctrl-K/J or Ctrl-P/N: select\nEnter: connect and return to panel   Ctrl-U: clear   Esc: panel\nCtrl-C: close   Ctrl-R: refresh (in either mode)\n\nConnect starts nordvpnd.service on demand. Service changes may\nask for your sudo password in the terminal. Closing leaves the VPN alone.\n\nWidget: kitty --class=widget.nortui -e nortui"
            );
            ExitCode::SUCCESS
        }
        [argument] if argument == "--version" || argument == "-V" => {
            println!("nortui {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            let _ = writeln!(io::stderr(), "nortui: unknown arguments; use --help");
            ExitCode::FAILURE
        }
    }
}
