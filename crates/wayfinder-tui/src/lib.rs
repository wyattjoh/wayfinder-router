use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use wayfinder_internal_core::complexity::{score_complexity, ComplexityScore, RoutingConfig};
use wayfinder_internal_core::threads::{title_from, Thread};

pub const COMMAND_NAME: &str = "chat";

#[derive(Debug, Clone, PartialEq)]
pub struct ChatOptions {
    pub theme: String,
    pub threshold: Option<f64>,
    pub show_why: bool,
    pub dry_run: bool,
    pub stream: bool,
    pub base_url: Option<String>,
    pub input: Option<String>,
    pub thread_dir: Option<PathBuf>,
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
            input: None,
            thread_dir: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RouteMode {
    #[default]
    Auto,
    Local,
    Cloud,
}

impl RouteMode {
    fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Local => "local",
            Self::Cloud => "cloud",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatCommand {
    Prompt(String),
    Why,
    Route,
    SetRoute(RouteMode),
    ListThreads,
    LoadThread(String),
    SaveThread(String),
    Empty,
    Unknown(String),
}

impl ChatCommand {
    pub fn parse(line: &str) -> Self {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Self::Empty;
        }
        if !trimmed.starts_with('/') {
            return Self::Prompt(trimmed.to_string());
        }

        let mut parts = trimmed.split_whitespace();
        match parts.next() {
            Some("/why") => Self::Why,
            Some("/route") => Self::Route,
            Some("/local") => Self::SetRoute(RouteMode::Local),
            Some("/cloud") => Self::SetRoute(RouteMode::Cloud),
            Some("/auto") => Self::SetRoute(RouteMode::Auto),
            Some("/threads") => Self::ListThreads,
            Some("/load") => Self::LoadThread(parts.collect::<Vec<_>>().join(" ")),
            Some("/save") => Self::SaveThread(parts.collect::<Vec<_>>().join(" ")),
            Some(other) => Self::Unknown(other.to_string()),
            None => Self::Empty,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatState {
    pub route_mode: RouteMode,
    routing_config: RoutingConfig,
    last_prompt: Option<String>,
    last_score: Option<ComplexityScore>,
    prompts: Vec<String>,
    thread_dir: Option<PathBuf>,
}

impl Default for ChatState {
    fn default() -> Self {
        Self {
            route_mode: RouteMode::Auto,
            routing_config: RoutingConfig::default(),
            last_prompt: None,
            last_score: None,
            prompts: Vec::new(),
            thread_dir: None,
        }
    }
}

impl ChatState {
    fn with_options(options: &ChatOptions) -> Self {
        let routing_config = options
            .threshold
            .map(RoutingConfig::binary)
            .unwrap_or_default();
        Self {
            routing_config,
            thread_dir: options.thread_dir.clone(),
            ..Self::default()
        }
    }
}

#[derive(Debug)]
pub enum ChatError {
    Io(io::Error),
    Json(serde_json::Error),
    InvalidThreadId(String),
}

impl fmt::Display for ChatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::InvalidThreadId(id) => write!(f, "invalid thread id '{id}'"),
        }
    }
}

impl Error for ChatError {}

impl From<io::Error> for ChatError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for ChatError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub fn run_chat(options: &ChatOptions) -> Result<String, ChatError> {
    let mut state = ChatState::with_options(options);
    let mut lines = vec![
        "wayfinder-router chat".to_string(),
        format!("dry-run: {}", options.dry_run),
        format!("theme: {}", options.theme),
        format!("stream: {}", options.stream),
        format!(
            "gateway: {}",
            if options.dry_run {
                "skipped"
            } else {
                "not configured"
            }
        ),
        "key status: not checked".to_string(),
    ];

    let Some(input) = options
        .input
        .as_deref()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    else {
        lines.push("input: enter a prompt on stdin or pass prompt text after chat".to_string());
        lines.push(
            "commands: /why /route /local /cloud /auto /threads /load <id> /save <id>".to_string(),
        );
        return Ok(lines.join("\n"));
    };

    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        let rendered = apply_chat_line(&mut state, line);
        if !rendered.is_empty() {
            lines.push(rendered);
        }
        if options.show_why && matches!(ChatCommand::parse(line), ChatCommand::Prompt(_)) {
            lines.push(render_last_why(&state));
        }
    }

    Ok(lines.join("\n"))
}

pub fn apply_chat_line(state: &mut ChatState, line: &str) -> String {
    match ChatCommand::parse(line) {
        ChatCommand::Prompt(prompt) => {
            let score = score_complexity(&prompt, &state.routing_config);
            state.last_prompt = Some(prompt.clone());
            state.last_score = Some(score);
            state.prompts.push(prompt.clone());
            render_route_decision(&prompt, state)
        }
        ChatCommand::Why => render_last_why(state),
        ChatCommand::Route => match state.last_prompt.as_deref() {
            Some(prompt) => render_route_decision(prompt, state),
            None => format!("route mode: {}", state.route_mode.label()),
        },
        ChatCommand::SetRoute(mode) => {
            state.route_mode = mode;
            format!("route override: {}", mode.label())
        }
        ChatCommand::ListThreads => match state.thread_dir.as_deref() {
            Some(dir) => match list_thread_summaries(dir) {
                Ok(summaries) if summaries.is_empty() => "threads: none".to_string(),
                Ok(summaries) => summaries.join("\n"),
                Err(err) => format!("threads: {err}"),
            },
            None => "threads: no thread directory configured".to_string(),
        },
        ChatCommand::LoadThread(id) => match state.thread_dir.as_deref() {
            Some(dir) if !id.is_empty() => match load_thread(dir, &id) {
                Ok(thread) => {
                    state.prompts = thread_user_prompts(&thread.messages);
                    state.last_prompt = state.prompts.last().cloned();
                    state.last_score = state
                        .last_prompt
                        .as_deref()
                        .map(|prompt| score_complexity(prompt, &state.routing_config));
                    format!("loaded thread: {}\t{}", thread.id, thread.title)
                }
                Err(err) => format!("load thread: {err}"),
            },
            Some(_) => "load thread: missing id".to_string(),
            None => "load thread: no thread directory configured".to_string(),
        },
        ChatCommand::SaveThread(id) => match state.thread_dir.as_deref() {
            Some(dir) if !id.is_empty() => match save_thread(dir, &id, state.prompts.iter()) {
                Ok(thread) => format!("saved thread: {}\t{}", thread.id, thread.title),
                Err(err) => format!("save thread: {err}"),
            },
            Some(_) => "save thread: missing id".to_string(),
            None => "save thread: no thread directory configured".to_string(),
        },
        ChatCommand::Empty => String::new(),
        ChatCommand::Unknown(command) => format!("unknown command: {command}"),
    }
}

pub fn render_route_decision(prompt: &str, state: &ChatState) -> String {
    let score = state
        .last_prompt
        .as_deref()
        .filter(|last| *last == prompt)
        .and_then(|_| state.last_score.as_ref().cloned())
        .unwrap_or_else(|| score_complexity(prompt, &state.routing_config));
    let route = effective_route(&score, state.route_mode);
    let mut lines = vec![
        format!("prompt: {prompt}"),
        format!("route: {route}"),
        format!("score: {:.2}", score.score),
        format!("mode: {}", score.mode),
    ];
    if state.route_mode != RouteMode::Auto {
        lines.push(format!("core route: {}", score.recommendation));
    }
    lines.join("\n")
}

fn render_last_why(state: &ChatState) -> String {
    let Some(prompt) = state.last_prompt.as_deref() else {
        return "why: no prompt yet".to_string();
    };
    let score = state
        .last_score
        .clone()
        .unwrap_or_else(|| score_complexity(prompt, &state.routing_config));
    let route = effective_route(&score, state.route_mode);
    format!(
        "why:\nprompt: {prompt}\nroute: {route}\nscore: {:.2}\nfeatures: words={}, headings={}, lists={}, code_blocks={}, questions={}",
        score.score,
        score.features.word_count,
        score.features.heading_count,
        score.features.list_item_count,
        score.features.code_block_count,
        score.features.question_count
    )
}

fn effective_route(score: &ComplexityScore, mode: RouteMode) -> String {
    match mode {
        RouteMode::Auto => score.recommendation.clone(),
        RouteMode::Local => "local".to_string(),
        RouteMode::Cloud => "cloud".to_string(),
    }
}

pub fn save_thread<I, S>(dir: &Path, id: &str, prompts: I) -> Result<Thread, ChatError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    validate_thread_id(id)?;
    fs::create_dir_all(dir)?;
    let now = timestamp();
    let messages = prompts
        .into_iter()
        .map(|prompt| json!({ "role": "user", "content": prompt.as_ref() }))
        .collect::<Vec<_>>();
    let thread = Thread {
        id: id.to_string(),
        title: title_from(&messages, 50),
        created: now.clone(),
        updated: now,
        messages,
    };
    let encoded = serde_json::to_string_pretty(&thread)?;
    fs::write(thread_path(dir, id), format!("{encoded}\n"))?;
    Ok(thread)
}

pub fn load_thread(dir: &Path, id: &str) -> Result<Thread, ChatError> {
    validate_thread_id(id)?;
    let text = fs::read_to_string(thread_path(dir, id))?;
    Ok(serde_json::from_str(&text)?)
}

pub fn list_thread_summaries(dir: &Path) -> Result<Vec<String>, ChatError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut summaries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(path)?;
        let thread: Thread = serde_json::from_str(&text)?;
        summaries.push(format!("{}\t{}", thread.id, thread.title));
    }
    summaries.sort();
    Ok(summaries)
}

fn validate_thread_id(id: &str) -> Result<(), ChatError> {
    let valid = !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(ChatError::InvalidThreadId(id.to_string()))
    }
}

fn thread_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn thread_user_prompts(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .filter_map(|message| message.get("content").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("unix:{seconds}")
}
