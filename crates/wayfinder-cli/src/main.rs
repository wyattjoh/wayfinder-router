use std::io::{self, IsTerminal, Read};

use wayfinder_internal_tui::{run_chat, run_interactive_chat, should_launch_interactive};

fn main() {
    // Capture the terminal state before reading stdin: a piped stdin populates the
    // chat input and keeps the transcript path, so the interactive decision must see
    // whether the user is at a real terminal with nothing to route yet.
    let stdin_is_terminal = io::stdin().is_terminal();
    let stdin = read_piped_stdin();
    match wayfinder_internal_cli::parse_with_input(std::env::args().skip(1), stdin) {
        Ok(wayfinder_internal_cli::CliCommand::Serve(options)) => {
            if let Err(err) = wayfinder_internal_gateway::serve_blocking(options) {
                eprintln!("wayfinder-router: {err}");
                std::process::exit(1);
            }
        }
        Ok(wayfinder_internal_cli::CliCommand::Chat(options)) => {
            if should_launch_interactive(stdin_is_terminal, options.input.is_some()) {
                if let Err(err) = run_interactive_chat(&options) {
                    eprintln!("wayfinder-router: {err}");
                    std::process::exit(1);
                }
                return;
            }
            match run_chat(&options) {
                Ok(message) => println!("{message}"),
                Err(err) => {
                    eprintln!("wayfinder-router: {err}");
                    std::process::exit(1);
                }
            }
        }
        Ok(wayfinder_internal_cli::CliCommand::Help(text)) => println!("{text}"),
        Err(err) => {
            eprintln!("wayfinder-router: {err}");
            std::process::exit(2);
        }
    }
}

fn read_piped_stdin() -> Option<String> {
    let mut stdin = io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut input = String::new();
    stdin.read_to_string(&mut input).ok()?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
