pub const COMMAND_NAME: &str = "chat";

#[derive(Debug, Clone, PartialEq)]
pub struct ChatOptions {
    pub theme: String,
    pub threshold: Option<f64>,
    pub show_why: bool,
    pub dry_run: bool,
    pub stream: bool,
    pub base_url: Option<String>,
}

impl Default for ChatOptions {
    fn default() -> Self {
        Self {
            theme: "auto".to_owned(),
            threshold: None,
            show_why: false,
            dry_run: false,
            stream: true,
            base_url: None,
        }
    }
}

pub fn chat_placeholder(options: &ChatOptions) -> String {
    let threshold = options
        .threshold
        .map(|value| value.to_string())
        .unwrap_or_else(|| "config".to_owned());
    let mode = if options.dry_run {
        "dry-run"
    } else {
        "gateway-backed"
    };
    let stream = if options.stream {
        "streaming"
    } else {
        "no-stream"
    };
    let base_url = options.base_url.as_deref().unwrap_or("in-process gateway");
    let why = if options.show_why {
        "why=on"
    } else {
        "why=off"
    };
    format!(
        "wayfinder-router chat scaffold: theme={}, threshold={threshold}, {mode}, {stream}, base_url={base_url}, {why}; TUI runtime not implemented yet",
        options.theme
    )
}
