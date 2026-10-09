program_name := file_name(justfile_directory())
debug_bin := "./target/debug" / program_name
profile_bin := "./target/profiling" / program_name

default: run

@list:
    just --list

alias i := install
install:
    cargo install --path . --locked

alias r := run
run *args:
    cargo run -- {{ args }}

alias b := build_debug
alias build := build_debug
build_debug:
    cargo build

alias rr := run_release
run_release *args:
    cargo run --release -- {{ args }}

alias release := build_release
build_release:
    cargo build --release

alias t := run_test
alias test := run_test
run_test:
    cargo nextest run

alias w := watch
alias dev := watch
watch:
    watchexec -c clear -e rs,toml -- just run

alias d := debug
debug *args: build_debug
    rust-lldb -Q -o "b {{ program_name }}::main" -o "run" -- {{ debug_bin }} {{ args }}

console:
    evcxr

alias v := valgrind
valgrind *args: build_debug
    valgrind --tool=memcheck --leak-check=full --track-origins=yes {{ debug_bin }} {{ args }}

alias c := callgrind
callgrind *args: build_profile
    valgrind --tool=callgrind --callgrind-out-file=callgrind.out {{ profile_bin }} {{ args }}
    callgrind_annotate callgrind.out

alias p := profile
profile *args: build_profile
    samply record {{ profile_bin }} {{ args }}

build_profile:
    cargo build --profile profiling

clean:
    cargo clean
