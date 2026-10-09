use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    match arguments.as_slice() {
        [] => match nortui::app::run(nortui::backend::Backend::new()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("nortui: {error:#}");
                ExitCode::FAILURE
            }
        },
        [argument] if argument == "--help" || argument == "-h" => {
            println!(
                "nortui — a searchable NordVPN terminal widget\n\nUsage: nortui [--help | --version]\n\nType to search commands, countries, and cities.\n↑/↓ or Ctrl-N/P: select   Enter: run   Ctrl-R: refresh\nCtrl-U: clear   Esc: clear search, then close   Ctrl-C: close\n\nConnect starts nordvpnd.service on demand. Service changes may\nask for your sudo password in the terminal. Closing leaves the VPN alone.\n\nWidget: kitty --class=widget.nortui -e nortui"
            );
            ExitCode::SUCCESS
        }
        [argument] if argument == "--version" || argument == "-V" => {
            println!("nortui {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("nortui: unknown arguments; use --help");
            ExitCode::FAILURE
        }
    }
}
