//! The interactive Ratatui app shell ported from the Python `WayfinderChat`
//! (`wayfinder_router/tui.py`).
//!
//! This module owns the terminal lifecycle, the crossterm event loop, the layout
//! (welcome + transcript, status bar, bordered composer, footer hints), the input
//! mechanics (prompt-history recall, multiline staging, inline slash suggestions), and
//! the threaded reply workers with cooperative cancel. The decision-first core loop
//! (route a plain prompt through [`decide`] then the relay) lives here; the full
//! slash-command set is a later task and enters through the [`App::dispatch_command`]
//! seam, which today only handles `/quit`.
//!
//! The state transitions are factored as plain methods on [`App`] so they are callable
//! without a real terminal (the loop is a thin driver over them), which keeps the shell
//! testable with `TestBackend` and sets up the command-dispatch task.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Alignment, Constraint, Layout, Position};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use serde_json::{json, Value};

use wayfinder_internal_core::pricing::estimate_tokens;
use wayfinder_internal_gateway::bootstrap::{key_status, missing_keys, resolve_keys};
use wayfinder_internal_gateway::{
    invoke_messages, load_gateway_models, stream_messages, GatewayModel, RelayMessage,
};

use crate::cost::{account_turn, cost_summary, SessionCost};
use crate::decision::{decide, resolve_target, Decision, TuiState};
use crate::remote::{friendly_error, remote_reply};
use crate::render::{footer_bar, render_decision, render_reply, render_welcome, status_bar};
use crate::theme::{palette_for, resolve_theme, Palette};
use crate::{ChatError, ChatOptions};

/// Slash commands offered as inline autocomplete in the composer (typing `/` suggests).
///
/// Mirrors the Python `_SLASH_COMMANDS`. The shell only renders the suggestion and routes
/// `/quit`; the rest are wired up in the command-dispatch task.
const SLASH_COMMANDS: &[&str] = &[
    "/init",
    "/models",
    "/keys",
    "/cost",
    "/new",
    "/threads",
    "/open",
    "/route",
    "/auto",
    "/local",
    "/cloud",
    "/btw",
    "/threshold",
    "/scope",
    "/sticky",
    "/why",
    "/stream",
    "/theme",
    "/settings",
    "/help",
    "/quit",
];

/// How many lines PageUp/PageDown (and Ctrl-U) move the transcript per press.
const SCROLL_STEP: u16 = 5;

/// Entry point: run the full-screen interactive chat until the user quits.
///
/// Sets up the terminal (raw mode + alternate screen + a panic hook that restores first),
/// drives the event loop, and always restores the terminal on normal exit, on error, and
/// on panic via the [`TerminalGuard`] RAII drop.
pub fn run_interactive_chat(options: &ChatOptions) -> Result<(), ChatError> {
    let mut app = App::new(options);
    let mut guard = TerminalGuard::new()?;
    let result = app.run(&mut guard.terminal);
    // The guard's Drop restores the terminal here (and also during a panic unwind).
    drop(guard);
    result
}

// --- terminal lifecycle ------------------------------------------------------

/// Runs `f` when dropped, including during a panic unwind.
///
/// The terminal restore is composed through this guard so the restore truly runs via a
/// `Drop` impl: even a panic mid-draw unwinds through it before the process aborts. Paired
/// with the panic hook that [`ratatui::try_init`] installs, the terminal is never left in
/// raw mode or the alternate screen.
struct OnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// Owns the initialized terminal and restores it on drop.
struct TerminalGuard {
    terminal: DefaultTerminal,
    _restore: OnDrop<fn()>,
}

impl TerminalGuard {
    fn new() -> Result<Self, ChatError> {
        // try_init enables raw mode + the alternate screen and installs a panic hook that
        // restores the terminal before the default hook runs.
        let terminal = ratatui::try_init()?;
        // Bracketed paste lets a multi-line paste arrive as one Event::Paste we can stage.
        let _ = execute!(std::io::stdout(), EnableBracketedPaste);
        Ok(Self {
            terminal,
            _restore: OnDrop(restore_terminal),
        })
    }
}

/// Disable bracketed paste and hand the terminal back (idempotent, panic-safe).
fn restore_terminal() {
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, DisableBracketedPaste);
    let _ = stdout.flush();
    ratatui::restore();
}

// --- reply workers -----------------------------------------------------------

/// An event from a reply worker thread, applied to the UI on the main loop.
enum ReplyEvent {
    /// The remote gateway's routing decision (with an optional forced target).
    Decision {
        decision: Box<Decision>,
        forced_to: Option<(String, bool)>,
    },
    /// A streamed token delta to append to the live reply.
    Delta(String),
    /// The reply finished normally with its full text.
    Done { full: String },
    /// Ctrl-C / Esc stopped the stream; carries whatever streamed so far.
    Cancelled { partial: String },
    /// The relay failed; carries a friendly message.
    Error(String),
}

/// The live worker: a receiver for its events plus the cooperative cancel flag.
struct WorkerHandle {
    rx: Receiver<ReplyEvent>,
    cancel: Arc<AtomicBool>,
}

/// What the main loop needs once a reply finishes: whether to keep it in the thread and how
/// to account its cost.
struct PendingReply {
    remember: bool,
    account: bool,
    is_local: bool,
    chosen_cost: Option<f64>,
    cloud_cost: Option<f64>,
    sent_tokens: usize,
}

// --- transcript model --------------------------------------------------------

/// One streaming / finalized model reply in the transcript.
struct ReplyEntry {
    body: String,
    status: ReplyStatus,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplyStatus {
    Streaming,
    Done,
    Cancelled,
    Error,
}

/// A block in the scrollable transcript.
enum Entry {
    /// Pre-rendered static content (welcome, notes, decisions).
    Block(Text<'static>),
    /// A model reply that streams in place and then finalizes.
    Reply(ReplyEntry),
}

// --- app ---------------------------------------------------------------------

/// The interactive chat state plus the pure transitions the event loop drives.
struct App {
    palette: Palette,
    state: TuiState,
    start_dir: PathBuf,
    base_url: Option<String>,
    dry_run: bool,
    stream: bool,
    threshold: Option<f64>,
    timeout: Duration,
    models: BTreeMap<String, GatewayModel>,

    entries: Vec<Entry>,
    history: Vec<Decision>,
    messages: Vec<RelayMessage>,
    live_index: Option<usize>,

    input: String,
    cursor: usize,
    input_history: Vec<String>,
    hist_index: Option<usize>,
    draft_lines: Vec<String>,

    note: Option<String>,
    scroll: u16,
    follow: bool,

    busy: bool,
    worker: Option<WorkerHandle>,
    pending: Option<PendingReply>,
    cost: SessionCost,

    should_quit: bool,
}

impl App {
    fn new(options: &ChatOptions) -> Self {
        let resolved = resolve_theme(&options.theme);
        let palette = palette_for(&options.theme);
        let start_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let state = TuiState {
            threshold: options.threshold,
            show_why: options.show_why,
            stream: options.stream,
            theme: resolved.to_owned(),
            ..TuiState::default()
        };

        let mut models = BTreeMap::new();
        let mut config_warning = None;
        if options.base_url.is_none() && !options.dry_run {
            match load_gateway_models(&start_dir) {
                Ok(loaded) => {
                    models = loaded;
                    // Fill keys from a secret store at launch (in-process only, WF-DESIGN-0006).
                    let _ = resolve_keys(&models);
                }
                Err(err) => config_warning = Some(err.to_string()),
            }
        }

        let mut app = Self {
            palette,
            state,
            start_dir,
            base_url: options.base_url.clone(),
            dry_run: options.dry_run,
            stream: options.stream,
            threshold: options.threshold,
            timeout: Duration::from_secs(60),
            models,
            entries: Vec::new(),
            history: Vec::new(),
            messages: Vec::new(),
            live_index: None,
            input: String::new(),
            cursor: 0,
            input_history: Vec::new(),
            hist_index: None,
            draft_lines: Vec::new(),
            note: None,
            scroll: 0,
            follow: true,
            busy: false,
            worker: None,
            pending: None,
            cost: SessionCost::default(),
            should_quit: false,
        };
        app.setup_welcome(config_warning);
        app
    }

    /// The on-mount chrome: the welcome header plus a one-line status of the backend.
    fn setup_welcome(&mut self, config_warning: Option<String>) {
        let subtitle = "deterministic LLM routing - local vs cloud";
        self.entries
            .push(Entry::Block(render_welcome(&self.palette, subtitle, false)));
        if let Some(warning) = config_warning {
            self.append_warn(warning);
        }
        if let Some(base) = self.base_url.clone() {
            self.append_note(format!("connected · remote gateway {base}"));
        } else if !self.models.is_empty() {
            let names: Vec<String> = self.models.keys().cloned().collect();
            self.append_note(format!("connected · routing between {}", names.join(", ")));
            let missing = missing_keys(&key_status(&self.models));
            if !missing.is_empty() {
                self.append_warn(format!(
                    "{} not set - /keys to add it (1Password, keychain, …); keyless local still works",
                    missing.join(", ")
                ));
            }
        } else if self.dry_run {
            self.append_note("preview · --dry-run: routing decisions only, no model calls");
        } else {
            self.append_note("no models configured · routing decisions only for now");
            self.append_note("add a model with /init once the command set lands");
        }
    }

    // --- main loop (thin driver over the pure transitions) ---
    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<(), ChatError> {
        while !self.should_quit {
            terminal.draw(|frame| self.render(frame))?;
            self.poll_worker();
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
                    Event::Paste(text) => self.stage_paste(text),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    // --- transcript helpers ---
    fn append_block(&mut self, text: Text<'static>) {
        self.entries.push(Entry::Block(text));
        self.follow = true;
    }

    fn append_note(&mut self, message: impl Into<String>) {
        let line = Line::styled(message.into(), Style::new().fg(self.palette.muted));
        self.append_block(Text::from(line));
    }

    fn append_warn(&mut self, message: impl Into<String>) {
        let line = Line::styled(message.into(), Style::new().fg(self.palette.warn));
        self.append_block(Text::from(line));
    }

    fn append_user_line(&mut self, line: &str) {
        let spans = vec![
            Span::styled("› ", Style::new().fg(self.palette.accent)),
            Span::styled(line.to_owned(), Style::new().fg(self.palette.text)),
        ];
        self.append_block(Text::from(Line::from(spans)));
    }

    fn append_decision(&mut self, decision: &Decision, forced_to: Option<(&str, bool)>) {
        let text = render_decision(decision, &self.palette, self.state.show_why, forced_to);
        self.append_block(text);
    }

    // --- worker plumbing ---
    fn poll_worker(&mut self) {
        if self.worker.is_none() {
            return;
        }
        let mut finished = false;
        loop {
            let event = match self.worker.as_ref().unwrap().rx.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            };
            self.apply_reply_event(event, &mut finished);
        }
        if finished {
            self.worker = None;
            self.pending = None;
            self.live_index = None;
            self.busy = false;
            self.note = None;
        }
    }

    fn apply_reply_event(&mut self, event: ReplyEvent, finished: &mut bool) {
        match event {
            ReplyEvent::Decision {
                decision,
                forced_to,
            } => {
                self.history.push((*decision).clone());
                let forced_ref = forced_to
                    .as_ref()
                    .map(|(name, local)| (name.as_str(), *local));
                self.append_decision(&decision, forced_ref);
            }
            ReplyEvent::Delta(delta) => {
                self.live_reply().body.push_str(&delta);
                self.follow = true;
            }
            ReplyEvent::Done { full } => {
                {
                    let live = self.live_reply();
                    live.body = full.clone();
                    live.status = ReplyStatus::Done;
                }
                self.finish_reply(&full);
                self.follow = true;
                *finished = true;
            }
            ReplyEvent::Cancelled { partial } => {
                let live = self.live_reply();
                live.body = partial;
                live.status = ReplyStatus::Cancelled;
                self.follow = true;
                *finished = true;
            }
            ReplyEvent::Error(message) => {
                let live = self.live_reply();
                live.body = message;
                live.status = ReplyStatus::Error;
                self.follow = true;
                *finished = true;
            }
        }
    }

    /// The live reply entry, created lazily on the first delta / completion.
    fn live_reply(&mut self) -> &mut ReplyEntry {
        if self.live_index.is_none() {
            self.entries.push(Entry::Reply(ReplyEntry {
                body: String::new(),
                status: ReplyStatus::Streaming,
            }));
            self.live_index = Some(self.entries.len() - 1);
        }
        match &mut self.entries[self.live_index.unwrap()] {
            Entry::Reply(reply) => reply,
            Entry::Block(_) => unreachable!("live_index always points at a reply entry"),
        }
    }

    /// On a successful reply: keep the assistant turn (unless ephemeral) and account cost.
    fn finish_reply(&mut self, full: &str) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if full.is_empty() {
            return;
        }
        if pending.remember {
            self.messages
                .push(RelayMessage::new("assistant", full.to_owned()));
        }
        if pending.account {
            let tokens = pending.sent_tokens + estimate_tokens(full);
            account_turn(
                &mut self.cost,
                pending.is_local,
                tokens,
                pending.chosen_cost,
                pending.cloud_cost,
            );
        }
    }

    fn cancel_reply(&mut self) {
        if let Some(worker) = &self.worker {
            worker.cancel.store(true, Ordering::Relaxed);
        }
        self.note = Some("cancelling…".to_owned());
    }

    // --- key handling ---
    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // Always available, even mid-reply.
            KeyCode::Char('c') if ctrl => {
                if self.busy {
                    self.cancel_reply();
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('d') if ctrl => self.should_quit = true,
            KeyCode::Char('u') if ctrl => self.scroll_up(),
            KeyCode::Esc => {
                if self.busy {
                    self.cancel_reply();
                } else {
                    self.input.clear();
                    self.cursor = 0;
                    self.hist_index = None;
                }
            }
            KeyCode::PageUp => self.scroll_up(),
            KeyCode::PageDown => self.scroll_down(),
            KeyCode::Tab => self.expand_why(),
            KeyCode::Up => self.recall(-1),
            KeyCode::Down => self.recall(1),
            // The composer is disabled while a reply is in flight (mirrors the Python entry).
            _ if self.busy => {}
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.move_right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) => self.insert_char(c),
            _ => {}
        }
    }

    fn scroll_up(&mut self) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(SCROLL_STEP);
    }

    fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_add(SCROLL_STEP);
        // render() re-enables follow once the offset reaches the bottom.
    }

    fn expand_why(&mut self) {
        if let Some(decision) = self.history.last().cloned() {
            let text = render_decision(&decision, &self.palette, true, None);
            self.append_block(text);
        } else {
            self.append_note("nothing to expand yet");
        }
    }

    /// Recall the previous / next submitted line into the composer (mirrors `_recall`).
    fn recall(&mut self, direction: isize) {
        if self.busy || self.input_history.is_empty() {
            return;
        }
        if self.hist_index.is_none() {
            if direction > 0 {
                return; // already at the live (empty) line
            }
            self.hist_index = Some(self.input_history.len());
        }
        let next = self.hist_index.unwrap() as isize + direction;
        if next >= self.input_history.len() as isize {
            self.hist_index = None;
            self.input.clear();
            self.cursor = 0;
            return;
        }
        let index = next.max(0) as usize;
        self.hist_index = Some(index);
        self.input = self.input_history[index].clone();
        self.cursor = self.input.chars().count();
    }

    // --- composer editing ---
    fn byte_index(&self) -> usize {
        self.input
            .char_indices()
            .map(|(i, _)| i)
            .nth(self.cursor)
            .unwrap_or(self.input.len())
    }

    fn insert_char(&mut self, c: char) {
        let index = self.byte_index();
        self.input.insert(index, c);
        self.cursor += 1;
        self.hist_index = None;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let from = self
            .input
            .char_indices()
            .map(|(i, _)| i)
            .nth(self.cursor - 1)
            .unwrap_or(0);
        let to = self.byte_index();
        self.input.replace_range(from..to, "");
        self.cursor -= 1;
        self.hist_index = None;
    }

    /// Right-arrow moves the cursor, or accepts the inline slash suggestion at line end.
    fn move_right(&mut self) {
        let count = self.input.chars().count();
        if self.cursor < count {
            self.cursor += 1;
        } else if let Some(rest) = self.current_suggestion() {
            self.input.push_str(&rest);
            self.cursor = self.input.chars().count();
        }
    }

    /// The remainder of the first slash command that completes the current input.
    fn current_suggestion(&self) -> Option<String> {
        if !self.input.starts_with('/') || self.input.len() < 2 {
            return None;
        }
        SLASH_COMMANDS
            .iter()
            .find(|command| command.starts_with(&self.input) && **command != self.input)
            .map(|command| command[self.input.len()..].to_owned())
    }

    // --- submission and multiline staging ---
    fn submit(&mut self) {
        let Some(full) = self.take_submission() else {
            return;
        };
        if let Some(rest) = full.strip_prefix('/') {
            self.dispatch_command(rest.trim());
            return;
        }
        self.route_prompt(full);
    }

    /// Resolve the current line into a complete submission, handling multiline staging and
    /// prompt-history dedup. Returns `None` when the line was staged or empty.
    fn take_submission(&mut self) -> Option<String> {
        let raw = std::mem::take(&mut self.input);
        self.cursor = 0;
        if raw.trim_end().ends_with('\\') {
            // A trailing backslash continues onto a new line.
            let trimmed = raw.trim_end();
            self.draft_lines
                .push(trimmed[..trimmed.len() - 1].to_owned());
            self.update_draft_indicator();
            return None;
        }
        let full = if self.draft_lines.is_empty() {
            raw.trim().to_owned()
        } else {
            let mut lines = std::mem::take(&mut self.draft_lines);
            lines.push(raw);
            self.update_draft_indicator();
            lines.join("\n")
        };
        if full.trim().is_empty() {
            return None;
        }
        if self
            .input_history
            .last()
            .map(|last| *last != full)
            .unwrap_or(true)
        {
            self.input_history.push(full.clone()); // ↑/↓ recall (no consecutive dups)
        }
        self.hist_index = None;
        Some(full)
    }

    /// Stage a multi-line paste: all but the last line, with the tail left to edit.
    fn stage_paste(&mut self, text: String) {
        if self.busy {
            return;
        }
        let lines: Vec<&str> = text.split('\n').collect();
        if lines.len() <= 1 {
            for c in text.chars() {
                self.insert_char(c);
            }
            return;
        }
        self.draft_lines.push(format!("{}{}", self.input, lines[0]));
        for middle in &lines[1..lines.len() - 1] {
            self.draft_lines.push((*middle).to_owned());
        }
        self.input = lines[lines.len() - 1].to_owned();
        self.cursor = self.input.chars().count();
        self.update_draft_indicator();
    }

    fn update_draft_indicator(&mut self) {
        let count = self.draft_lines.len();
        if count == 0 {
            self.note = None;
            return;
        }
        let plural = if count != 1 { "s" } else { "" };
        self.note = Some(format!(
            "{count} line{plural} staged · Enter sends · end a line with \\ or paste to add more"
        ));
    }

    /// The command-dispatch seam filled by the next task. Today only `/quit` is wired up.
    fn dispatch_command(&mut self, command: &str) {
        let name = command.split_whitespace().next().unwrap_or("");
        match name {
            "quit" | "q" | "exit" => self.should_quit = true,
            other => self.append_note(format!("/{other} is not wired up yet")),
        }
    }

    // --- the decision-first core loop ---
    fn route_prompt(&mut self, text: String) {
        self.append_user_line(&text);
        self.messages.push(RelayMessage::new("user", text.clone()));

        if let Some(base_url) = self.base_url.clone() {
            self.spawn_remote_worker(base_url);
            return;
        }

        let decision = match decide(&text, &self.start_dir, self.threshold) {
            Ok(decision) => decision,
            Err(err) => {
                self.append_warn(err.to_string());
                self.messages.pop();
                return;
            }
        };
        self.history.push(decision.clone());
        let forced_to = self
            .state
            .pinned
            .as_deref()
            .map(|pin| resolve_target(Some(pin), &decision));
        let forced_ref = forced_to
            .as_ref()
            .map(|(name, local)| (name.as_str(), *local));
        self.append_decision(&decision, forced_ref);

        if self.models.is_empty() {
            return;
        }
        let (target, target_is_local) = forced_to
            .clone()
            .unwrap_or_else(|| (decision.model.clone(), decision.is_local));
        let Some(model) = self.models.get(&target).cloned() else {
            self.append_note(format!("no model configured for '{target}'"));
            return;
        };
        let cloud_name = decision
            .targets
            .last()
            .cloned()
            .unwrap_or_else(|| target.clone());
        let cloud_cost = self
            .models
            .get(&cloud_name)
            .and_then(|model| model.cost_per_1k);
        self.spawn_stream_worker(model, target_is_local, cloud_cost);
    }

    fn spawn_stream_worker(
        &mut self,
        model: GatewayModel,
        is_local: bool,
        cloud_cost: Option<f64>,
    ) {
        let messages = self.messages.clone();
        let timeout = self.timeout;
        let stream = self.stream;
        let chosen_cost = model.cost_per_1k;
        let sent_tokens: usize = messages.iter().map(|m| estimate_tokens(&m.content)).sum();

        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let cancel_thread = cancel.clone();
        // The relay is blocking, so it must run off any async runtime: a plain thread.
        std::thread::spawn(move || {
            if stream {
                let mut parts = String::new();
                for item in stream_messages(&model, &messages, timeout) {
                    if cancel_thread.load(Ordering::Relaxed) {
                        let _ = tx.send(ReplyEvent::Cancelled { partial: parts });
                        return;
                    }
                    match item {
                        Ok(delta) => {
                            parts.push_str(&delta);
                            if tx.send(ReplyEvent::Delta(delta)).is_err() {
                                return;
                            }
                        }
                        Err(err) => {
                            let _ = tx.send(ReplyEvent::Error(friendly_error(
                                &err.to_string(),
                                &model.base_url,
                            )));
                            return;
                        }
                    }
                }
                if cancel_thread.load(Ordering::Relaxed) {
                    let _ = tx.send(ReplyEvent::Cancelled { partial: parts });
                } else {
                    let _ = tx.send(ReplyEvent::Done { full: parts });
                }
            } else {
                match invoke_messages(&model, &messages, timeout) {
                    Ok(full) => {
                        let _ = tx.send(ReplyEvent::Done { full });
                    }
                    Err(err) => {
                        let _ = tx.send(ReplyEvent::Error(friendly_error(
                            &err.to_string(),
                            &model.base_url,
                        )));
                    }
                }
            }
        });

        self.start_worker(
            WorkerHandle { rx, cancel },
            PendingReply {
                remember: true,
                account: true,
                is_local,
                chosen_cost,
                cloud_cost,
                sent_tokens,
            },
            "streaming… (ctrl-c to cancel)",
        );
    }

    fn spawn_remote_worker(&mut self, base_url: String) {
        let messages_json: Vec<Value> = self
            .messages
            .iter()
            .map(|m| json!({ "role": m.role, "content": m.content }))
            .collect();
        let pin = self.state.pinned.clone();
        let model_field = pin.clone().unwrap_or_else(|| "auto".to_owned());
        let threshold = self.threshold;
        let timeout = self.timeout;

        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let cancel_thread = cancel.clone();
        std::thread::spawn(move || {
            match remote_reply(&base_url, &messages_json, &model_field, threshold, timeout) {
                Ok((decision, reply)) => {
                    if cancel_thread.load(Ordering::Relaxed) {
                        let _ = tx.send(ReplyEvent::Cancelled {
                            partial: String::new(),
                        });
                        return;
                    }
                    if let Some(decision) = decision {
                        let forced_to = pin
                            .as_deref()
                            .map(|pin| resolve_target(Some(pin), &decision));
                        let _ = tx.send(ReplyEvent::Decision {
                            decision: Box::new(decision),
                            forced_to,
                        });
                    }
                    let _ = tx.send(ReplyEvent::Done {
                        full: reply.unwrap_or_default(),
                    });
                }
                Err(message) => {
                    // remote_reply already maps transport failures to a friendly hint.
                    let _ = tx.send(ReplyEvent::Error(message));
                }
            }
        });

        self.start_worker(
            WorkerHandle { rx, cancel },
            PendingReply {
                remember: true,
                account: false,
                is_local: false,
                chosen_cost: None,
                cloud_cost: None,
                sent_tokens: 0,
            },
            "asking gateway… (ctrl-c to cancel)",
        );
    }

    fn start_worker(&mut self, worker: WorkerHandle, pending: PendingReply, note: &str) {
        self.busy = true;
        self.note = Some(note.to_owned());
        self.worker = Some(worker);
        self.pending = Some(pending);
        self.live_index = None;
    }

    // --- rendering ---
    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(
            Block::default().style(Style::new().bg(self.palette.bg)),
            area,
        );
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);
        self.render_transcript(frame, chunks[0]);
        self.render_status(frame, chunks[1]);
        self.render_composer(frame, chunks[2]);
        self.render_footer(frame, chunks[3]);
    }

    fn render_transcript(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let bg = self.palette.bg;
        let fg = self.palette.text;
        // The transcript is not wrapped, so one logical line is one row: the row count is
        // exact, which keeps scroll clamping (and follow-to-bottom) reliable. Long lines
        // truncate at the right edge. (Paragraph::line_count, which would let us wrap and
        // still measure, is gated behind an unstable ratatui feature.)
        let lines = self.transcript_lines();
        let total = lines.len() as u16;
        let max_scroll = total.saturating_sub(area.height);
        if self.follow {
            self.scroll = max_scroll;
        } else {
            self.scroll = self.scroll.min(max_scroll);
            if self.scroll >= max_scroll {
                self.follow = true;
            }
        }
        let paragraph = Paragraph::new(Text::from(lines))
            .scroll((self.scroll, 0))
            .style(Style::new().fg(fg).bg(bg));
        frame.render_widget(paragraph, area);
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        for entry in &self.entries {
            match entry {
                Entry::Block(text) => lines.extend(text.lines.iter().cloned()),
                Entry::Reply(reply) => lines.extend(self.reply_lines(reply)),
            }
            lines.push(Line::raw("")); // a blank line between blocks
        }
        lines
    }

    fn reply_lines(&self, reply: &ReplyEntry) -> Vec<Line<'static>> {
        let text = self.palette.text;
        let warn = self.palette.warn;
        match reply.status {
            ReplyStatus::Streaming => {
                let mut lines: Vec<Line<'static>> = reply
                    .body
                    .split('\n')
                    .map(|line| Line::styled(line.to_owned(), Style::new().fg(text)))
                    .collect();
                let cursor = Span::styled("▏", Style::new().fg(text));
                match lines.last_mut() {
                    Some(last) => last.spans.push(cursor),
                    None => lines.push(Line::from(cursor)),
                }
                lines
            }
            ReplyStatus::Done => {
                if reply.body.is_empty() {
                    vec![Line::styled(
                        "(empty reply)",
                        Style::new().fg(self.palette.muted),
                    )]
                } else {
                    render_reply(&reply.body).lines
                }
            }
            ReplyStatus::Cancelled => {
                let mut lines = if reply.body.is_empty() {
                    Vec::new()
                } else {
                    render_reply(&reply.body).lines
                };
                lines.push(Line::styled("⨯ cancelled", Style::new().fg(warn)));
                lines
            }
            ReplyStatus::Error => reply
                .body
                .split('\n')
                .map(|line| Line::styled(line.to_owned(), Style::new().fg(warn)))
                .collect(),
        }
    }

    fn render_status(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let bg = Style::new().bg(self.palette.bg);
        let (left, right) = status_bar(&self.state, &self.palette, self.note.as_deref());
        frame.render_widget(Paragraph::new(left).style(bg), area);
        frame.render_widget(
            Paragraph::new(right).alignment(Alignment::Right).style(bg),
            area,
        );
    }

    fn render_composer(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(self.palette.accent))
            .style(Style::new().bg(self.palette.bg));
        let inner = block.inner(area);

        let mut spans = vec![Span::styled("› ", Style::new().fg(self.palette.accent))];
        if self.input.is_empty() && self.draft_lines.is_empty() {
            spans.push(Span::styled(
                "Send a message - Wayfinder routes it…",
                Style::new().fg(self.palette.muted),
            ));
        } else {
            spans.push(Span::styled(
                self.input.clone(),
                Style::new().fg(self.palette.text),
            ));
            if let Some(rest) = self.current_suggestion() {
                spans.push(Span::styled(rest, Style::new().fg(self.palette.muted)));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);

        if !self.busy {
            let x = inner.x + 2 + self.cursor as u16;
            let max_x = inner.x + inner.width.saturating_sub(1);
            frame.set_cursor_position(Position {
                x: x.min(max_x),
                y: inner.y,
            });
        }
    }

    fn render_footer(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let bg = Style::new().bg(self.palette.bg);
        let right = if self.note.is_some() {
            "routing…".to_owned()
        } else {
            let summary = cost_summary(&self.cost);
            if summary.is_empty() {
                "no model call to decide".to_owned()
            } else {
                summary
            }
        };
        let (left, right_line) = footer_bar(&self.palette, &right);
        frame.render_widget(Paragraph::new(left).style(bg), area);
        frame.render_widget(
            Paragraph::new(right_line)
                .alignment(Alignment::Right)
                .style(bg),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::AssertUnwindSafe;

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use wayfinder_internal_core::complexity::FeatureContribution;

    fn test_app() -> App {
        let options = ChatOptions {
            dry_run: true,
            ..ChatOptions::default()
        };
        App::new(&options)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn render_to_string(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[Position { x, y }].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn sample_decision() -> Decision {
        Decision {
            text: "explain the routing".to_owned(),
            model: "local-llama".to_owned(),
            score: 0.42,
            mode: "tiered".to_owned(),
            is_local: true,
            contributions: vec![FeatureContribution {
                name: "word_count".to_owned(),
                value: 12,
                normalized: 0.4,
                weight: 1.0,
                contribution: 0.4,
            }],
            threshold: None,
            targets: vec!["local-llama".to_owned(), "cloud-gpt".to_owned()],
        }
    }

    /// The terminal restore composes through `OnDrop`, so its Drop must run even when the
    /// scope unwinds from a panic mid-draw.
    #[test]
    fn on_drop_runs_during_panic_unwind() {
        let restored = Arc::new(AtomicBool::new(false));
        let flag = restored.clone();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = OnDrop(|| flag.store(true, Ordering::SeqCst));
            panic!("boom mid-draw");
        }));
        assert!(result.is_err());
        assert!(
            restored.load(Ordering::SeqCst),
            "the guard must restore during a panic unwind"
        );
    }

    #[test]
    fn streamed_reply_renders_incrementally_then_finalizes() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        app.start_worker(
            WorkerHandle { rx, cancel },
            PendingReply {
                remember: false,
                account: false,
                is_local: true,
                chosen_cost: None,
                cloud_cost: None,
                sent_tokens: 0,
            },
            "streaming…",
        );

        tx.send(ReplyEvent::Delta("Hel".to_owned())).unwrap();
        app.poll_worker();
        assert!(app.busy, "still streaming");
        assert!(render_to_string(&mut app).contains("Hel"));

        tx.send(ReplyEvent::Delta("lo there".to_owned())).unwrap();
        app.poll_worker();
        assert!(render_to_string(&mut app).contains("Hello there"));

        tx.send(ReplyEvent::Done {
            full: "Hello there".to_owned(),
        })
        .unwrap();
        app.poll_worker();
        assert!(!app.busy, "reply finished clears busy");
        assert!(app.worker.is_none());
        assert!(render_to_string(&mut app).contains("Hello there"));
    }

    #[test]
    fn ctrl_c_quits_only_when_idle() {
        // Idle: Ctrl-C quits.
        let mut app = test_app();
        app.on_key(ctrl(KeyCode::Char('c')));
        assert!(app.should_quit);

        // In flight: Ctrl-C cancels and stays.
        let mut app = test_app();
        let (_tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        app.start_worker(
            WorkerHandle {
                rx,
                cancel: cancel.clone(),
            },
            PendingReply {
                remember: false,
                account: false,
                is_local: true,
                chosen_cost: None,
                cloud_cost: None,
                sent_tokens: 0,
            },
            "streaming…",
        );
        app.on_key(ctrl(KeyCode::Char('c')));
        assert!(!app.should_quit, "cancel must not quit while busy");
        assert!(cancel.load(Ordering::Relaxed), "cancel flag is set");
    }

    #[test]
    fn esc_cancels_in_flight_without_quitting() {
        let mut app = test_app();
        let (_tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        app.start_worker(
            WorkerHandle {
                rx,
                cancel: cancel.clone(),
            },
            PendingReply {
                remember: false,
                account: false,
                is_local: true,
                chosen_cost: None,
                cloud_cost: None,
                sent_tokens: 0,
            },
            "streaming…",
        );
        app.on_key(press(KeyCode::Esc));
        assert!(!app.should_quit);
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn history_recall_walks_submitted_lines() {
        let mut app = test_app();
        app.input_history = vec!["first".to_owned(), "second".to_owned()];
        app.recall(-1);
        assert_eq!(app.input, "second");
        app.recall(-1);
        assert_eq!(app.input, "first");
        app.recall(-1);
        assert_eq!(app.input, "first", "clamps at the oldest entry");
        app.recall(1);
        assert_eq!(app.input, "second");
        app.recall(1);
        assert_eq!(
            app.input, "",
            "stepping past the newest returns the live line"
        );
    }

    #[test]
    fn history_skips_consecutive_duplicates() {
        let mut app = test_app();
        app.input = "hello".to_owned();
        app.cursor = app.input.chars().count();
        let _ = app.take_submission();
        app.input = "hello".to_owned();
        app.cursor = app.input.chars().count();
        let _ = app.take_submission();
        assert_eq!(app.input_history, vec!["hello".to_owned()]);
    }

    #[test]
    fn trailing_backslash_stages_multiline_message() {
        let mut app = test_app();
        app.input = "line one \\".to_owned();
        assert_eq!(app.take_submission(), None, "a staged line does not submit");
        assert_eq!(app.draft_lines, vec!["line one ".to_owned()]);

        app.input = "line two".to_owned();
        assert_eq!(
            app.take_submission(),
            Some("line one \nline two".to_owned()),
            "the final line assembles the staged message"
        );
        assert!(app.draft_lines.is_empty());
    }

    #[test]
    fn multiline_paste_stages_all_but_last_line() {
        let mut app = test_app();
        app.stage_paste("alpha\nbeta\ngamma".to_owned());
        assert_eq!(app.draft_lines, vec!["alpha".to_owned(), "beta".to_owned()]);
        assert_eq!(app.input, "gamma");
    }

    #[test]
    fn tab_appends_an_expanded_decision() {
        let mut app = test_app();
        let before = app.entries.len();
        app.expand_why();
        assert_eq!(
            app.entries.len(),
            before + 1,
            "with no decision, a note is added"
        );

        app.history.push(sample_decision());
        let before = app.entries.len();
        app.on_key(press(KeyCode::Tab));
        assert_eq!(app.entries.len(), before + 1);
        // The expanded view renders the score breakdown contribution row.
        match app.entries.last().unwrap() {
            Entry::Block(text) => {
                let rendered: String = text
                    .lines
                    .iter()
                    .flat_map(|line| line.spans.iter())
                    .map(|span| span.content.as_ref())
                    .collect();
                assert!(
                    rendered.contains("word_count"),
                    "expanded why shows the breakdown"
                );
            }
            Entry::Reply(_) => panic!("expected a decision block"),
        }
    }
}
