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
        Ok(wayfinder_internal_cli::CliCommand::Route(mut options)) => {
            if options.prompt == "-" && options.input.is_none() {
                options.input = Some(read_stdin());
            }
            match wayfinder_internal_cli::execute(wayfinder_internal_cli::CliCommand::Route(
                options,
            )) {
                Ok(output) => {
                    print!("{}", output.stdout);
                    eprint!("{}", output.stderr);
                }
                Err(err) => {
                    eprintln!("wayfinder-router: {err}");
                    std::process::exit(err.exit_code());
                }
            }
        }
        Ok(command) => match wayfinder_internal_cli::execute(command) {
            Ok(output) => {
                print!("{}", output.stdout);
                eprint!("{}", output.stderr);
            }
            Err(err) => {
                eprintln!("wayfinder-router: {err}");
                std::process::exit(err.exit_code());
            }
        },
        Err(err) => {
            eprintln!("wayfinder-router: {err}");
            std::process::exit(err.exit_code());
        }
    }
}

fn read_stdin() -> String {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);
    input
}

fn read_piped_stdin() -> Option<String> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let input = read_stdin();
    if input.trim().is_empty() {
        None
    } else {
        Some(input)
    }
}
