//! Desktop entry point, built only with the `gui` feature. Double-clicking the
//! app bundle starts this binary, which always opens a window.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let path = match lastwrite::database_path(custom_db()) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("lastwrite: {message}");
            return ExitCode::FAILURE;
        }
    };
    match lastwrite::gui::run(path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("lastwrite: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Accepts the same `--db PATH` option as the CLI, which is useful for demos
/// and for keeping test runs out of the real history file.
fn custom_db() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--db" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}
