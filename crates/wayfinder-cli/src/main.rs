use std::io::{self, IsTerminal, Read};

fn main() {
    let stdin = read_piped_stdin();
    match wayfinder_internal_cli::parse_with_input(std::env::args().skip(1), stdin) {
        Ok(wayfinder_internal_cli::CliCommand::Serve(options)) => {
            if let Err(err) = wayfinder_internal_gateway::serve_blocking(options) {
                eprintln!("wayfinder-router: {err}");
                std::process::exit(1);
            }
        }
        Ok(wayfinder_internal_cli::CliCommand::Chat(options)) => {
            match wayfinder_internal_tui::run_chat(&options) {
                Ok(message) => println!("{message}"),
                Err(err) => {
                    eprintln!("wayfinder-router: {err}");
                    std::process::exit(1);
                }
            }
        }
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
