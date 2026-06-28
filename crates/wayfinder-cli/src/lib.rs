use std::error::Error;
use std::fmt;

use wayfinder_internal_gateway::{serve_placeholder, ServeOptions};
use wayfinder_internal_tui::{chat_placeholder, ChatOptions};

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

pub fn run<I>(args: I) -> Result<String, CliError>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    match args.next().as_deref() {
        Some("serve") => Ok(serve_placeholder(&parse_serve(args)?)),
        Some("chat") => Ok(chat_placeholder(&parse_chat(args)?)),
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

fn parse_chat<I>(args: I) -> Result<ChatOptions, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = ChatOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
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
            other => return Err(CliError::new(format!("unknown chat option '{other}'"))),
        }
    }
    Ok(options)
}

fn next_value<I>(args: &mut I, flag: &str) -> Result<String, CliError>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| CliError::new(format!("{flag} requires a value")))
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

        assert!(output.contains("chat"));
        assert!(output.contains("theme=dark"));
        assert!(output.contains("dry-run"));
    }
}
