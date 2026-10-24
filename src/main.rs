mod backup;
mod cli;
mod commands;
mod config;
mod history;
mod monitor;
mod scan;
mod targets;
mod ui;
mod util;

use std::process::ExitCode;

use clap::Parser;

use crate::ui as console;

fn main() -> ExitCode {
    // Rust ignores SIGPIPE by default, which turns `tend man | head` into a panic.
    // Standard CLI behavior is to die quietly when the reader closes the pipe.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let raw: Vec<String> = std::env::args().collect();

    // `version` as a word, plus the short flag the previous build accepted.
    if matches!(
        raw.get(1).map(String::as_str),
        Some("version" | "--version" | "-v")
    ) {
        println!("tend {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let cli = cli::Cli::parse_from(raw);
    match cli::run(cli) {
        Ok(code) => ExitCode::from(code as u8),
        Err(error) => {
            console::say_fail(&error.to_string());
            ExitCode::FAILURE
        }
    }
}
