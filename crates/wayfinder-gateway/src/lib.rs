use wayfinder_internal_core::{DEFAULT_HOST, DEFAULT_PORT};

pub const COMMAND_NAME: &str = "serve";

#[derive(Debug, Clone, PartialEq)]
pub struct ServeOptions {
    pub host: String,
    pub port: u16,
    pub dry_run: bool,
    pub timeout_seconds: Option<f64>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
            dry_run: false,
            timeout_seconds: None,
        }
    }
}

pub fn serve_placeholder(options: &ServeOptions) -> String {
    let mode = if options.dry_run {
        "dry-run"
    } else {
        "forwarding"
    };
    let timeout = options
        .timeout_seconds
        .map(|value| format!("{value}s"))
        .unwrap_or_else(|| "default timeout".to_owned());
    format!(
        "wayfinder-router serve scaffold: binding {}:{} ({mode}, {timeout}); gateway runtime not implemented yet",
        options.host, options.port
    )
}
