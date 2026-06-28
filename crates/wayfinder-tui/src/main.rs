use std::io::{self, IsTerminal, Read};

use wayfinder_internal_tui::{run_chat, run_interactive_chat, should_launch_interactive};

fn main() {
    let mut options = wayfinder_internal_tui::ChatOptions::default();
    let mut args = std::env::args().skip(1);
    if matches!(args.next().as_deref(), Some("chat")) {
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--theme" => options.theme = args.next().unwrap_or(options.theme),
                "--threshold" => {
                    options.threshold = args.next().and_then(|value| value.parse().ok())
                }
                "--why" => options.show_why = true,
                "--dry-run" => options.dry_run = true,
                "--no-stream" => options.stream = false,
                "--base-url" => options.base_url = args.next(),
                "--thread-dir" => options.thread_dir = args.next().map(Into::into),
                "--" => {
                    options.input = Some(args.collect::<Vec<_>>().join(" "));
                    break;
                }
                text if !text.starts_with('-') => {
                    let mut parts = vec![text.to_string()];
                    parts.extend(args);
                    options.input = Some(parts.join(" "));
                    break;
                }
                _ => {}
            }
        }
    }
    // Capture the terminal state before reading stdin: a piped stdin populates
    // `input` and keeps the transcript path, so the decision must see whether the
    // user is at an interactive terminal with nothing to route yet.
    let stdin_is_terminal = io::stdin().is_terminal();
    if options.input.is_none() {
        options.input = read_piped_stdin();
    }
    if should_launch_interactive(stdin_is_terminal, options.input.is_some()) {
        if let Err(err) = run_interactive_chat(&options) {
            eprintln!("wayfinder-router-tui: {err}");
            std::process::exit(2);
        }
        return;
    }
    match run_chat(&options) {
        Ok(output) => println!("{output}"),
        Err(err) => {
            eprintln!("wayfinder-router-tui: {err}");
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
