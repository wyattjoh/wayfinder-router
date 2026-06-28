use std::error::Error;
use std::fmt;

use wayfinder_internal_gateway::{serve_summary, ServeOptions};
use wayfinder_internal_tui::{run_chat, ChatOptions, HELP};

#[derive(Debug, PartialEq, Eq)]
pub struct CliError(String);

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for CliError {}

pub enum CliCommand {
    Serve(ServeOptions),
    Chat(ChatOptions),
    Help(String),
}

/// Usage block for `chat --help`, joined with the slash-command [`HELP`] summary.
const CHAT_USAGE: &str = "\
usage: wayfinder-router chat [OPTIONS] [PROMPT]

Open the interactive chat on a terminal. When a prompt is passed as arguments or
stdin is piped, print the routing transcript instead so scripting and CI work.

options:
  --theme <name>       color theme: auto, dark, or light
  --threshold <0..1>   local/cloud routing cut
  --why                expand the routing decision for each prompt
  --dry-run            skip the gateway call
  --no-stream          disable token-by-token streaming
  --base-url <url>     gateway base url for the thin client
  --thread-dir <dir>   directory for saved conversation threads
  --help, -h           show this help";

fn chat_help() -> String {
    format!("{CHAT_USAGE}\n\n{HELP}")
}

pub fn run<I>(args: I) -> Result<String, CliError>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    run_with_input(args, None)
}

pub fn run_with_input<I>(args: I, stdin: Option<String>) -> Result<String, CliError>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    match parse_with_input(args, stdin)? {
        CliCommand::Serve(options) => Ok(serve_summary(&options)),
        CliCommand::Chat(options) => {
            run_chat(&options).map_err(|err| CliError::new(err.to_string()))
        }
        CliCommand::Help(text) => Ok(text),
    }
}

pub fn parse<I>(args: I) -> Result<CliCommand, CliError>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    parse_with_input(args, None)
}

pub fn parse_with_input<I>(args: I, stdin: Option<String>) -> Result<CliCommand, CliError>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    match args.next().as_deref() {
        Some("serve") => Ok(CliCommand::Serve(parse_serve(args)?)),
        Some("chat") => match parse_chat(args)? {
            None => Ok(CliCommand::Help(chat_help())),
            Some(mut options) => {
                if options.input.is_none() {
                    options.input = stdin.and_then(non_empty);
                }
                Ok(CliCommand::Chat(options))
            }
        },
        Some(command) => Err(CliError::new(format!(
            "unknown command '{command}' (expected 'serve' or 'chat')"
        ))),
        None => Err(CliError::new("expected command: serve or chat")),
    }
}

fn parse_serve<I>(args: I) -> Result<ServeOptions, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = ServeOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => options.host = next_value(&mut args, "--host")?,
            "--port" => {
                options.port = next_value(&mut args, "--port")?
                    .parse()
                    .map_err(|_| CliError::new("--port must be an integer"))?;
            }
            "--dry-run" => options.dry_run = true,
            "--timeout" => {
                options.timeout_seconds = Some(
                    next_value(&mut args, "--timeout")?
                        .parse()
                        .map_err(|_| CliError::new("--timeout must be a number"))?,
                );
            }
            other => return Err(CliError::new(format!("unknown serve option '{other}'"))),
        }
    }
    Ok(options)
}

fn parse_chat<I>(args: I) -> Result<Option<ChatOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = ChatOptions::default();
    let mut args = args.into_iter();
    let mut prompt_parts = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--theme" => options.theme = next_value(&mut args, "--theme")?,
            "--threshold" => {
                options.threshold = Some(
                    next_value(&mut args, "--threshold")?
                        .parse()
                        .map_err(|_| CliError::new("--threshold must be a number"))?,
                );
            }
            "--why" => options.show_why = true,
            "--dry-run" => options.dry_run = true,
            "--no-stream" => options.stream = false,
            "--base-url" => options.base_url = Some(next_value(&mut args, "--base-url")?),
            "--thread-dir" => {
                options.thread_dir = Some(next_value(&mut args, "--thread-dir")?.into())
            }
            "--" => {
                prompt_parts.extend(args);
                break;
            }
            other if other.starts_with('-') => {
                return Err(CliError::new(format!("unknown chat option '{other}'")));
            }
            text => prompt_parts.push(text.to_string()),
        }
    }
    if !prompt_parts.is_empty() {
        options.input = Some(prompt_parts.join(" "));
    }
    Ok(Some(options))
}

fn next_value<I>(args: &mut I, flag: &str) -> Result<String, CliError>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| CliError::new(format!("{flag} requires a value")))
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::run;

    #[test]
    fn run_accepts_serve_shape() {
        let output = run([
            "serve",
            "--host",
            "0.0.0.0",
            "--port",
            "9000",
            "--dry-run",
            "--timeout",
            "10",
        ])
        .expect("serve args should parse");

        assert!(output.contains("serve"));
        assert!(output.contains("0.0.0.0:9000"));
        assert!(output.contains("dry-run"));
    }

    #[test]
    fn run_accepts_chat_shape() {
        let output = run([
            "chat",
            "--theme",
            "dark",
            "--threshold",
            "0.3",
            "--why",
            "--dry-run",
            "--no-stream",
            "--base-url",
            "http://127.0.0.1:8088",
        ])
        .expect("chat args should parse");

        assert!(output.contains("wayfinder-router chat"));
        assert!(output.contains("theme: dark"));
        assert!(output.contains("dry-run"));
    }

    #[test]
    fn run_chat_accepts_prompt_text_from_args() {
        let output = run(["chat", "--dry-run", "What", "is", "DNS?"])
            .expect("chat prompt args should parse");

        assert!(output.contains("prompt: What is DNS?"));
        assert!(output.contains("route: local"));
        assert!(output.contains("gateway: skipped"));
    }

    #[test]
    fn run_chat_accepts_prompt_text_from_stdin() {
        let output = super::run_with_input(
            ["chat", "--dry-run", "--why"],
            Some("What is DNS?\n".to_string()),
        )
        .expect("chat stdin should parse");

        assert!(output.contains("prompt: What is DNS?"));
        assert!(output.contains("why:"));
    }

    #[test]
    fn chat_help_prints_usage_and_command_summary() {
        let output = run(["chat", "--help"]).expect("chat --help should succeed");

        assert!(output.contains("usage: wayfinder-router chat"));
        assert!(output.contains("--theme"));
        assert!(output.contains("--thread-dir"));
        assert!(output.contains("commands"));
        assert!(output.contains("/why"));
    }

    #[test]
    fn chat_help_short_flag_matches_long_flag() {
        let long = run(["chat", "--help"]).expect("chat --help should succeed");
        let short = run(["chat", "-h"]).expect("chat -h should succeed");

        assert_eq!(long, short);
    }
}
