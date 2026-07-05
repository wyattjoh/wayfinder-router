//! The interactive Ratatui app shell ported from the Python `WayfinderChat`
//! (`wayfinder_router/tui.py`).
//!
//! This module owns the terminal lifecycle, the crossterm event loop, the layout
//! (welcome + transcript, status bar, bordered composer, footer hints), the input
//! mechanics (prompt-history recall, multiline staging, inline slash suggestions), and
//! the threaded reply workers with cooperative cancel. The decision-first core loop
//! (route a plain prompt through [`decide`] then the relay) lives here, alongside the full
//! slash-command surface ported from the Python `_handle_command`: the commands enter
//! through the [`App::dispatch_command`] seam and drive the same pure state methods.
//!
//! The state transitions are factored as plain methods on [`App`] so they are callable
//! without a real terminal (the loop is a thin driver over them), which keeps the shell
//! testable with `TestBackend` and lets the command dispatch be exercised without one.

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

use wayfinder_internal_core::pricing::{estimate_tokens, Date, SavingsLedger};
use wayfinder_internal_core::threads::{
    list_threads, new_thread, save_thread, threads_dir, Thread,
};
use wayfinder_internal_gateway::bootstrap::{
    key_status, missing_keys, render_config, render_env_example, resolve_keys, DEFAULT_PRESET,
    PRESETS,
};
use wayfinder_internal_gateway::{
    invoke_messages, load_gateway_models, stream_messages, GatewayModel, RelayMessage,
};

use crate::commands::{parse_command, HELP, SCOPES};
use crate::cost::{account_turn, cost_summary, fold_turn, load_ledger, savings_path, SessionCost};
use crate::decision::{
    decide, decide_with_context, pin_label, resolve_target, Decision, DecisionContext, TuiState,
};
use crate::remote::{friendly_error, remote_reply};
use crate::render::{
    footer_bar, render_cost, render_decision, render_empty_state, render_keys, render_models,
    render_reply, render_settings, render_threads, render_welcome, status_bar,
};
use crate::theme::{palette_for, resolve_theme, Palette};
use crate::{ChatError, ChatOptions};

/// Slash commands offered as inline autocomplete in the composer (typing `/` suggests).
///
/// Mirrors the Python `_SLASH_COMMANDS`. These drive the inline autocomplete; the
/// dispatch in [`App::dispatch_command`] backs the full set.
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
    /// The remote gateway returned no usable reply (`None`): the turn is rolled back with
    /// no reply widget, mirroring the Python `_remote_worker` `reply is None` branch.
    DiscardTurn,
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
    /// The chosen arm and the always-cloud baseline, for the persisted savings ledger.
    route: String,
    baseline: String,
    /// The remote (`--base-url`) backend, where the gateway both decides and replies.
    ///
    /// It makes one non-streaming request after the user turn was already pushed and
    /// persisted, so a failure (relay error, cancel, or a `None` reply) must roll that turn
    /// back instead of leaving it as orphaned, unanswered context. The in-process stream
    /// worker deliberately keeps the user turn on failure, so this stays `false` there.
    remote: bool,
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

    data_dir: PathBuf,
    thread: Thread,
    thread_list: Vec<Thread>,
    ledger: SavingsLedger,

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

        // Where conversations and the savings ledger persist (WF-ADR-0030, WF-DESIGN-0007).
        let data_dir = options.thread_dir.clone().unwrap_or_else(threads_dir);
        let ledger = load_ledger(&data_dir);
        let thread = new_thread();

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
            data_dir,
            thread,
            thread_list: Vec::new(),
            ledger,
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
            // No models configured (in-process): the onboarding panel points at /init.
            let panel = render_empty_state(&self.palette);
            self.append_block(panel);
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
        self.append_styled(message.into(), self.palette.muted);
    }

    fn append_warn(&mut self, message: impl Into<String>) {
        self.append_styled(message.into(), self.palette.warn);
    }

    /// Append a (possibly multi-line) message styled in one color, one transcript line per
    /// `\n` (the Python notes lean on rich's `Text`, which splits newlines itself).
    fn append_styled(&mut self, message: String, color: ratatui::style::Color) {
        let lines: Vec<Line<'static>> = message
            .split('\n')
            .map(|line| Line::styled(line.to_owned(), Style::new().fg(color)))
            .collect();
        self.append_block(Text::from(lines));
    }

    /// A submitted prompt echoed into the transcript. `aside` dims a `/btw` one-off so it
    /// reads as not part of the thread.
    fn append_user_line(&mut self, line: &str, aside: bool) {
        let spans = if aside {
            vec![
                Span::styled("↪ btw  ", Style::new().fg(self.palette.muted)),
                Span::styled(line.to_owned(), Style::new().fg(self.palette.muted)),
            ]
        } else {
            vec![
                Span::styled("› ", Style::new().fg(self.palette.accent)),
                Span::styled(line.to_owned(), Style::new().fg(self.palette.text)),
            ]
        };
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
                if self.pending_remote() {
                    // Remote backend: the (non-streaming) request was cancelled before any
                    // reply. Note the cancel without an empty reply widget, and roll the
                    // just-added user turn back so it is not resent as context.
                    self.append_note("⨯ cancelled");
                    self.rollback_user_turn();
                } else {
                    let live = self.live_reply();
                    live.body = partial;
                    live.status = ReplyStatus::Cancelled;
                }
                self.follow = true;
                *finished = true;
            }
            ReplyEvent::Error(message) => {
                if self.pending_remote() {
                    // Remote backend: the relay failed. Warn and roll the user turn back
                    // (the in-process stream worker keeps it; see `PendingReply::remote`).
                    self.append_warn(message);
                    self.rollback_user_turn();
                } else {
                    let live = self.live_reply();
                    live.body = message;
                    live.status = ReplyStatus::Error;
                }
                self.follow = true;
                *finished = true;
            }
            ReplyEvent::DiscardTurn => {
                self.rollback_user_turn();
                self.follow = true;
                *finished = true;
            }
        }
    }

    /// Whether the in-flight worker is the remote backend (controls failure rollback).
    fn pending_remote(&self) -> bool {
        self.pending.as_ref().is_some_and(|pending| pending.remote)
    }

    /// Roll the just-added user turn back after a remote failure, and re-save the thread
    /// without it. Only the remote backend rolls back, and only for a turn it kept (an
    /// ephemeral `/btw` never added one), so this is a no-op when there is nothing to drop.
    fn rollback_user_turn(&mut self) {
        let kept_turn = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.remember);
        if !kept_turn {
            return;
        }
        self.messages.pop();
        self.persist();
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
            self.persist();
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
            // Also fold the turn into the persisted ledger so /cost can show periods.
            fold_turn(
                &mut self.ledger,
                tokens,
                pending.chosen_cost,
                pending.cloud_cost,
                &pending.route,
                &pending.baseline,
                Date::today_utc(),
            );
            let _ = self.ledger.save(savings_path(&self.data_dir));
        }
    }

    /// Save the active thread to disk (UI-free, so it is safe to run after a reply).
    fn persist(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        self.thread.messages = self
            .messages
            .iter()
            .map(|message| json!({ "role": message.role, "content": message.content }))
            .collect();
        let _ = save_thread(&mut self.thread, &self.data_dir);
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
        match parse_command(&full) {
            (Some(cmd), arg) => self.dispatch_command(&cmd, &arg),
            (None, prompt) => self.route_prompt(prompt),
        }
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

    /// Dispatch a slash command onto state, renderers, and workers (ports `_handle_command`).
    ///
    /// `cmd` is the already-lowercased name and `arg` its trimmed remainder. Panel commands
    /// append a rendered block; the routing controls mutate [`TuiState`]; the one-shot forces
    /// (`/local <msg>`, `/cloud <msg>`, `/btw`) drive a turn through [`Self::route_message`].
    /// In `--base-url` mode the model/key/init commands defer to the remote gateway with a
    /// note, mirroring the Python client.
    fn dispatch_command(&mut self, cmd: &str, arg: &str) {
        match cmd {
            "quit" | "q" | "exit" => self.should_quit = true,
            "help" => self.append_note(HELP),
            "settings" => {
                let panel = render_settings(&self.state, &self.palette);
                self.append_block(panel);
            }
            "models" => self.handle_models(),
            "keys" => self.handle_keys(),
            "cost" => {
                let panel = render_cost(&self.cost, &self.palette, Some(&self.ledger));
                self.append_block(panel);
            }
            "init" => self.handle_init(arg),
            "new" => self.handle_new(),
            "threads" => {
                self.thread_list = list_threads(&self.data_dir).unwrap_or_default();
                let panel = render_threads(&self.thread_list, &self.palette);
                self.append_block(panel);
            }
            "open" | "thread" => self.handle_open(arg),
            "route" => self.handle_route(arg),
            "auto" => {
                self.set_pinned(None);
                self.append_note("routing: auto");
            }
            "local" | "cloud" => {
                let sentinel = if cmd == "local" {
                    "prefer-local"
                } else {
                    "prefer-hosted"
                };
                if !arg.is_empty() {
                    // One-shot force for this turn, kept in the thread.
                    self.route_message(arg.to_owned(), Some(sentinel.to_owned()), false);
                    return;
                }
                self.set_pinned(Some(sentinel.to_owned()));
                self.append_note(format!(
                    "pinned → {cmd} every turn · /auto to resume routing"
                ));
            }
            "btw" => {
                if arg.is_empty() {
                    self.append_warn(
                        "usage: /btw <quick question>  - a one-off aside routed local",
                    );
                    return;
                }
                self.route_message(arg.to_owned(), Some("prefer-local".to_owned()), true);
            }
            "threshold" => match arg.parse::<f64>() {
                Ok(value) => {
                    let value = value.clamp(0.0, 1.0);
                    self.state.threshold = Some(value);
                    self.threshold = Some(value);
                    self.append_note(format!("threshold {value:.2}"));
                }
                Err(_) => self.append_warn("threshold must be a number 0..1"),
            },
            "scope" => {
                if SCOPES.contains(&arg) {
                    self.state.scope = arg.to_owned();
                    self.append_note(format!("scope {arg}"));
                } else {
                    self.append_warn("scope must be turn|last_user|user|all");
                }
            }
            "sticky" => self.handle_sticky(arg),
            "theme" => {
                if matches!(arg, "dark" | "light" | "auto") {
                    self.state.theme = resolve_theme(arg).to_owned();
                    self.palette = palette_for(arg);
                    self.append_note(format!("theme {}", self.state.theme));
                } else {
                    self.append_warn("theme dark|light|auto");
                }
            }
            "why" => self.handle_why(arg),
            "stream" => {
                let value = arg.to_lowercase();
                if matches!(value.as_str(), "on" | "off" | "") {
                    self.state.stream = value != "off";
                    self.stream = self.state.stream;
                    let label = if self.state.stream { "on" } else { "off" };
                    self.append_note(format!("stream {label}"));
                } else {
                    self.append_warn("stream on|off");
                }
            }
            other => self.append_warn(format!("unknown command /{other} - /help")),
        }
    }

    /// Set (or clear) the standing route override.
    fn set_pinned(&mut self, pinned: Option<String>) {
        self.state.pinned = pinned;
    }

    fn handle_models(&mut self) {
        if let Some(base) = &self.base_url {
            let note = format!("models are managed by the remote gateway at {base}");
            self.append_note(note);
            return;
        }
        let panel = render_models(&self.models, &self.palette);
        self.append_block(panel);
    }

    /// The in-chat `doctor`: re-resolve keys from their secret stores and report.
    fn handle_keys(&mut self) {
        if let Some(base) = &self.base_url {
            let note = format!("keys are managed by the remote gateway at {base}");
            self.append_note(note);
            return;
        }
        let errors = resolve_keys(&self.models);
        let panel = render_keys(&self.models, &self.palette, Some(&errors));
        self.append_block(panel);
    }

    /// Scaffold a `wayfinder-router.toml` from a preset and load its models in place.
    fn handle_init(&mut self, arg: &str) {
        if self.base_url.is_some() {
            self.append_note(
                "connected to a remote gateway - configure its models there, not here",
            );
            return;
        }
        let name = if arg.is_empty() { DEFAULT_PRESET } else { arg };
        let Some(preset) = PRESETS.get(name) else {
            let names = PRESETS.keys().copied().collect::<Vec<_>>().join(", ");
            self.append_warn(format!("unknown preset '{name}' - try: {names}"));
            return;
        };
        let config_path = self.start_dir.join("wayfinder-router.toml");
        if config_path.exists() {
            self.append_warn(format!(
                "{} already exists - edit it, or run `wayfinder-router init --force` in a shell",
                config_path.display()
            ));
            return;
        }
        if let Err(err) = std::fs::write(&config_path, render_config(preset)) {
            self.append_warn(format!("could not write config: {err}"));
            return;
        }
        let mut extra = String::new();
        if !preset.env_vars.is_empty() {
            let env_path = self.start_dir.join(".env.example");
            if !env_path.exists() && std::fs::write(&env_path, render_env_example(preset)).is_ok() {
                extra = " (+ .env.example)".to_owned();
            }
        }
        self.append_note(format!(
            "wrote {}{extra} · preset {}",
            config_path.display(),
            preset.name
        ));
        match load_gateway_models(&self.start_dir) {
            Ok(models) => {
                self.models = models;
                let _ = resolve_keys(&self.models);
            }
            Err(err) => {
                self.append_warn(err.to_string());
                return;
            }
        }
        let panel = render_models(&self.models, &self.palette);
        self.append_block(panel);
        let missing = missing_keys(&key_status(&self.models));
        if missing.is_empty() {
            self.append_note("models ready - type a prompt");
        } else {
            self.append_note(format!(
                "{} not set - /keys to add it (1Password, keychain, …), no restart; keyless local works now",
                missing.join(", ")
            ));
        }
    }

    fn handle_new(&mut self) {
        self.persist(); // the current thread is already saved; make sure
        self.messages.clear();
        self.history.clear();
        self.thread = new_thread();
        self.entries.clear();
        self.live_index = None;
        self.scroll = 0;
        self.follow = true;
        self.append_note("new conversation - type a prompt");
    }

    fn handle_open(&mut self, arg: &str) {
        let entries = if self.thread_list.is_empty() {
            list_threads(&self.data_dir).unwrap_or_default()
        } else {
            self.thread_list.clone()
        };
        self.thread_list = entries.clone();
        let Ok(index) = arg.parse::<usize>() else {
            self.append_warn("usage: /open <number>  (see /threads)");
            return;
        };
        if index == 0 || index > entries.len() {
            self.append_warn(format!("no thread '{arg}' - /threads to list"));
            return;
        }
        self.persist(); // save the current conversation before switching away
        let thread = entries[index - 1].clone();
        self.load_thread_entry(thread);
    }

    /// Re-render the transcript for a reopened thread (ports `_load_thread`).
    fn load_thread_entry(&mut self, thread: Thread) {
        let messages: Vec<RelayMessage> = thread
            .messages
            .iter()
            .filter_map(|message| {
                let role = message.get("role").and_then(Value::as_str)?;
                let content = message.get("content").and_then(Value::as_str).unwrap_or("");
                Some(RelayMessage::new(role, content))
            })
            .collect();
        let title = if thread.title.is_empty() {
            "(untitled)".to_owned()
        } else {
            thread.title.clone()
        };
        self.thread = thread;
        self.history.clear();
        self.entries.clear();
        self.live_index = None;
        self.scroll = 0;
        self.follow = true;
        self.append_note(format!("thread · {title}"));
        for message in &messages {
            if message.role == "user" {
                self.append_user_line(&message.content, false);
                if self.base_url.is_none() && !message.content.is_empty() {
                    if let Ok(decision) = decide(&message.content, &self.start_dir, self.threshold)
                    {
                        self.history.push(decision.clone());
                        self.append_decision(&decision, None);
                    }
                }
            } else if message.role == "assistant" {
                let reply = render_reply(&message.content);
                self.append_block(reply);
            }
        }
        self.messages = messages;
    }

    fn handle_route(&mut self, arg: &str) {
        if arg.is_empty() {
            // Show current pin + the available targets.
            let names = if self.models.is_empty() {
                "(set by the gateway)".to_owned()
            } else {
                self.models.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            let label = pin_label(self.state.pinned.as_deref());
            self.append_note(format!("routing: {label} · models: {names}"));
            return;
        }
        if matches!(arg, "auto" | "off") {
            self.set_pinned(None);
            self.append_note("routing: auto");
            return;
        }
        if matches!(arg, "local" | "cloud") {
            // Aliases for the tier ends.
            let sentinel = if arg == "local" {
                "prefer-local"
            } else {
                "prefer-hosted"
            };
            self.set_pinned(Some(sentinel.to_owned()));
            self.append_note(format!("pinned → {arg} · /auto to resume routing"));
            return;
        }
        if self.base_url.is_none() && !self.models.is_empty() && !self.models.contains_key(arg) {
            let names = self.models.keys().cloned().collect::<Vec<_>>().join(", ");
            self.append_warn(format!("unknown model '{arg}' - available: {names}"));
            return;
        }
        self.set_pinned(Some(arg.to_owned()));
        self.append_note(format!("pinned → {arg} · /auto to resume routing"));
    }

    fn handle_sticky(&mut self, arg: &str) {
        let mut parts = arg.split_whitespace();
        match parts.next() {
            Some(state @ ("on" | "off")) => {
                self.state.sticky = state == "on";
                if let Some(cooldown) = parts.next().and_then(|value| value.parse::<u32>().ok()) {
                    self.state.cooldown = cooldown;
                }
                let tail = if self.state.sticky {
                    format!(" · cooldown {}", self.state.cooldown)
                } else {
                    String::new()
                };
                let label = if self.state.sticky { "on" } else { "off" };
                self.append_note(format!("sticky {label}{tail}"));
            }
            _ => self.append_warn("sticky on|off [N]"),
        }
    }

    fn handle_why(&mut self, arg: &str) {
        let value = arg.to_lowercase();
        if value == "on" {
            self.state.show_why = true;
            self.append_note("why: auto-expand on");
        } else if value == "off" {
            self.state.show_why = false;
            self.append_note("why: collapsed");
        } else if let Ok(n) = value.parse::<usize>() {
            if n >= 1 && n <= self.history.len() {
                let decision = self.history[n - 1].clone();
                let text = render_decision(&decision, &self.palette, true, None);
                self.append_block(text);
            } else {
                self.append_warn("why [on|off|N]");
            }
        } else if value.is_empty() {
            if let Some(decision) = self.history.last().cloned() {
                let text = render_decision(&decision, &self.palette, true, None);
                self.append_block(text);
            } else {
                self.append_note("nothing to expand yet");
            }
        } else {
            self.append_warn("why [on|off|N]");
        }
    }

    // --- the decision-first core loop ---
    /// Route a plain prompt: pin from the standing override, kept in the thread.
    fn route_prompt(&mut self, text: String) {
        let pin = self.state.pinned.clone();
        self.route_message(text, pin, false);
    }

    /// Route one turn: render the decision, then call the (possibly forced) model.
    ///
    /// `pin` forces the route for this turn (`None` = the natural decision). `ephemeral`
    /// (`/btw`) sends the turn standalone: no history attached, and neither the question
    /// nor the reply is added to the thread. Ports the Python `_route_message`.
    fn route_message(&mut self, text: String, pin: Option<String>, ephemeral: bool) {
        self.append_user_line(&text, ephemeral);
        let convo = if ephemeral {
            vec![RelayMessage::new("user", text.clone())]
        } else {
            self.messages.push(RelayMessage::new("user", text.clone()));
            self.messages.clone()
        };

        if let Some(base_url) = self.base_url.clone() {
            // The remote gateway decides and replies.
            if !ephemeral {
                self.persist();
            }
            self.spawn_remote_worker(base_url, convo, pin, !ephemeral);
            return;
        }

        let decision = match decide_with_context(
            &text,
            &self.start_dir,
            self.threshold,
            DecisionContext {
                scope: self.state.scope.clone(),
                sticky: self.state.sticky,
                cooldown: self.state.cooldown,
                messages: convo.clone(),
            },
        ) {
            Ok(decision) => decision,
            Err(err) => {
                self.append_warn(err.to_string());
                if !ephemeral {
                    self.messages.pop();
                }
                return;
            }
        };
        self.history.push(decision.clone());
        let forced_to = pin
            .as_deref()
            .map(|pin| resolve_target(Some(pin), &decision));
        let forced_ref = forced_to
            .as_ref()
            .map(|(name, local)| (name.as_str(), *local));
        self.append_decision(&decision, forced_ref);

        if ephemeral {
            self.append_note("aside · not added to the thread");
        } else {
            self.persist(); // capture the user turn (and decision-only conversations)
        }

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
        self.spawn_stream_worker(
            model,
            convo,
            !ephemeral,
            target_is_local,
            cloud_cost,
            target,
            cloud_name,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_stream_worker(
        &mut self,
        model: GatewayModel,
        messages: Vec<RelayMessage>,
        remember: bool,
        is_local: bool,
        cloud_cost: Option<f64>,
        route: String,
        baseline: String,
    ) {
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
                remember,
                account: remember,
                is_local,
                chosen_cost,
                cloud_cost,
                sent_tokens,
                route,
                baseline,
                remote: false,
            },
            "streaming… (ctrl-c to cancel)",
        );
    }

    fn spawn_remote_worker(
        &mut self,
        base_url: String,
        messages: Vec<RelayMessage>,
        pin: Option<String>,
        remember: bool,
    ) {
        let messages_json: Vec<Value> = messages
            .iter()
            .map(|m| json!({ "role": m.role, "content": m.content }))
            .collect();
        let model_field = pin.clone().unwrap_or_else(|| "auto".to_owned());
        let threshold = self.threshold;
        let scope = self.state.scope.clone();
        let sticky = self.state.sticky;
        let cooldown = self.state.cooldown;
        let timeout = self.timeout;

        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let cancel_thread = cancel.clone();
        std::thread::spawn(move || {
            match remote_reply(
                &base_url,
                &messages_json,
                &model_field,
                threshold,
                &scope,
                sticky,
                cooldown,
                timeout,
            ) {
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
                    // A present reply (even empty) is rendered; a `None` reply means the
                    // gateway returned nothing usable, so the turn is discarded.
                    let _ = match reply {
                        Some(full) => tx.send(ReplyEvent::Done { full }),
                        None => tx.send(ReplyEvent::DiscardTurn),
                    };
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
                remember,
                account: false,
                is_local: false,
                chosen_cost: None,
                cloud_cost: None,
                sent_tokens: 0,
                route: "local".to_owned(),
                baseline: "cloud".to_owned(),
                remote: true,
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use wayfinder_internal_core::complexity::FeatureContribution;

    /// A unique temp dir so a test's thread/ledger persistence never touches the real data
    /// dir or another test's fixtures (the dispatch tests exercise /new, /open, and routing,
    /// all of which save threads).
    fn temp_data_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wayfinder-tui-dispatch-{}-{seq}-{nanos}",
            std::process::id()
        ))
    }

    fn test_app() -> App {
        let options = ChatOptions {
            dry_run: true,
            thread_dir: Some(temp_data_dir()),
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
                route: "local".to_owned(),
                baseline: "cloud".to_owned(),
                remote: false,
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
                route: "local".to_owned(),
                baseline: "cloud".to_owned(),
                remote: false,
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
                route: "local".to_owned(),
                baseline: "cloud".to_owned(),
                remote: false,
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

    // --- slash-command dispatch (pure state transitions, no terminal) ---

    /// Flatten an entry into one newline-joined string of its span text.
    fn entry_text(entry: &Entry) -> String {
        match entry {
            Entry::Block(text) => text
                .lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Entry::Reply(reply) => reply.body.clone(),
        }
    }

    fn last_text(app: &App) -> String {
        entry_text(app.entries.last().expect("an entry was appended"))
    }

    #[test]
    fn route_pins_via_route_auto_local_cloud() {
        let mut app = test_app();
        app.dispatch_command("local", "");
        assert_eq!(app.state.pinned.as_deref(), Some("prefer-local"));
        app.dispatch_command("cloud", "");
        assert_eq!(app.state.pinned.as_deref(), Some("prefer-hosted"));
        app.dispatch_command("auto", "");
        assert_eq!(app.state.pinned, None);
        // /route also pins by alias and by clearing.
        app.dispatch_command("route", "cloud");
        assert_eq!(app.state.pinned.as_deref(), Some("prefer-hosted"));
        app.dispatch_command("route", "auto");
        assert_eq!(app.state.pinned, None);
    }

    #[test]
    fn local_and_cloud_with_arg_force_a_single_turn() {
        let mut app = test_app(); // dry_run: no models, but the decision still renders
        app.dispatch_command("local", "explain DNS");
        // A one-shot force does not change the standing pin.
        assert_eq!(app.state.pinned, None, "force is per-turn, not a pin");
        // The forced turn is kept in the thread.
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, "user");
        assert_eq!(app.messages[0].content, "explain DNS");
    }

    #[test]
    fn btw_routes_ephemeral_aside_not_added_to_thread() {
        let mut app = test_app();
        app.dispatch_command("btw", "quick aside");
        assert!(
            app.messages.is_empty(),
            "an ephemeral aside is not added to the thread"
        );
        assert!(
            app.entries
                .iter()
                .any(|entry| entry_text(entry).contains("aside")),
            "the aside is noted as not added to the thread"
        );
        // An empty /btw warns instead of routing.
        let mut app = test_app();
        app.dispatch_command("btw", "");
        assert!(last_text(&app).contains("usage: /btw"));
    }

    #[test]
    fn threshold_parses_and_clamps() {
        let mut app = test_app();
        app.dispatch_command("threshold", "0.5");
        assert_eq!(app.state.threshold, Some(0.5));
        assert_eq!(
            app.threshold,
            Some(0.5),
            "the routing threshold stays in sync"
        );
        app.dispatch_command("threshold", "2.0");
        assert_eq!(
            app.state.threshold,
            Some(1.0),
            "out-of-range clamps to 0..1"
        );
        app.dispatch_command("threshold", "-1");
        assert_eq!(app.state.threshold, Some(0.0));
        app.dispatch_command("threshold", "nope");
        assert_eq!(app.state.threshold, Some(0.0), "a bad value is rejected");
        assert!(last_text(&app).contains("threshold must be a number"));
    }

    #[test]
    fn scope_validates() {
        let mut app = test_app();
        app.dispatch_command("scope", "all");
        assert_eq!(app.state.scope, "all");
        app.dispatch_command("scope", "bogus");
        assert_eq!(app.state.scope, "all", "an invalid scope is rejected");
        assert!(last_text(&app).contains("scope must be"));
    }

    #[test]
    fn sticky_on_off_with_cooldown() {
        let mut app = test_app();
        app.dispatch_command("sticky", "on 3");
        assert!(app.state.sticky);
        assert_eq!(app.state.cooldown, 3);
        app.dispatch_command("sticky", "off");
        assert!(!app.state.sticky);
        assert_eq!(
            app.state.cooldown, 3,
            "cooldown is retained when toggled off"
        );
        app.dispatch_command("sticky", "maybe");
        assert!(last_text(&app).contains("sticky on|off"));
    }

    #[test]
    fn why_on_off_and_index() {
        let mut app = test_app();
        app.dispatch_command("why", "on");
        assert!(app.state.show_why);
        app.dispatch_command("why", "off");
        assert!(!app.state.show_why);
        // With a decision in history, /why N expands the Nth decision.
        app.history.push(sample_decision());
        let before = app.entries.len();
        app.dispatch_command("why", "1");
        assert_eq!(app.entries.len(), before + 1);
        assert!(
            last_text(&app).contains("word_count"),
            "expands the breakdown"
        );
        // An out-of-range index warns.
        app.dispatch_command("why", "9");
        assert!(last_text(&app).contains("why [on|off|N]"));
    }

    #[test]
    fn stream_on_off() {
        let mut app = test_app();
        app.dispatch_command("stream", "off");
        assert!(!app.state.stream);
        assert!(!app.stream, "the worker stream flag stays in sync");
        app.dispatch_command("stream", "on");
        assert!(app.state.stream);
        assert!(app.stream);
        app.dispatch_command("stream", "loud");
        assert!(last_text(&app).contains("stream on|off"));
    }

    #[test]
    fn theme_switch_changes_palette() {
        let mut app = test_app();
        assert_eq!(app.palette, crate::theme::DARK);
        app.dispatch_command("theme", "light");
        assert_eq!(app.state.theme, "light");
        assert_eq!(app.palette, crate::theme::LIGHT, "the palette re-applies");
        app.dispatch_command("theme", "purple");
        assert!(last_text(&app).contains("theme dark|light|auto"));
    }

    #[test]
    fn new_clears_transcript_and_messages() {
        let mut app = test_app();
        app.messages.push(RelayMessage::new("user", "hello"));
        app.history.push(sample_decision());
        app.dispatch_command("new", "");
        assert!(app.messages.is_empty());
        assert!(app.history.is_empty());
        // The transcript is cleared down to the fresh "new conversation" note.
        assert_eq!(app.entries.len(), 1);
        assert!(last_text(&app).contains("new conversation"));
    }

    #[test]
    fn open_out_of_range_warns() {
        let mut app = test_app(); // an empty (temp) thread dir
        app.dispatch_command("open", "5");
        assert!(last_text(&app).contains("no thread '5'"));
        // A non-numeric argument is a usage hint.
        app.dispatch_command("open", "abc");
        assert!(last_text(&app).contains("usage: /open"));
    }

    #[test]
    fn unknown_command_warns() {
        let mut app = test_app();
        app.dispatch_command("frobnicate", "");
        assert!(last_text(&app).contains("unknown command /frobnicate"));
    }

    // --- remote backend: roll back the user turn on a non-success reply ---

    fn reply_pending(remote: bool, remember: bool) -> PendingReply {
        PendingReply {
            remember,
            account: false,
            is_local: false,
            chosen_cost: None,
            cloud_cost: None,
            sent_tokens: 0,
            route: "local".to_owned(),
            baseline: "cloud".to_owned(),
            remote,
        }
    }

    fn has_reply_widget(app: &App) -> bool {
        app.entries
            .iter()
            .any(|entry| matches!(entry, Entry::Reply(_)))
    }

    #[test]
    fn remote_error_rolls_back_user_turn() {
        let mut app = test_app();
        // route_message pushed (and persisted) the user turn before spawning the worker.
        app.messages
            .push(RelayMessage::new("user", "ask the gateway"));
        app.pending = Some(reply_pending(true, true));
        let mut finished = false;
        app.apply_reply_event(
            ReplyEvent::Error("can't reach the gateway".to_owned()),
            &mut finished,
        );
        assert!(finished);
        assert!(
            app.messages.is_empty(),
            "a remote relay error rolls the orphaned user turn back"
        );
        assert!(
            last_text(&app).contains("can't reach the gateway"),
            "the error is surfaced as a warning"
        );
        assert!(
            !has_reply_widget(&app),
            "no reply widget for a remote error"
        );
    }

    #[test]
    fn remote_cancel_rolls_back_user_turn() {
        let mut app = test_app();
        app.messages
            .push(RelayMessage::new("user", "ask the gateway"));
        app.pending = Some(reply_pending(true, true));
        let mut finished = false;
        app.apply_reply_event(
            ReplyEvent::Cancelled {
                partial: String::new(),
            },
            &mut finished,
        );
        assert!(
            app.messages.is_empty(),
            "a cancelled remote turn is rolled back"
        );
        assert!(last_text(&app).contains("⨯ cancelled"));
        assert!(
            !has_reply_widget(&app),
            "no reply widget for a cancelled remote turn"
        );
    }

    #[test]
    fn remote_none_reply_discards_turn_without_widget() {
        let mut app = test_app();
        app.messages
            .push(RelayMessage::new("user", "ask the gateway"));
        app.pending = Some(reply_pending(true, true));
        let mut finished = false;
        app.apply_reply_event(ReplyEvent::DiscardTurn, &mut finished);
        assert!(
            app.messages.is_empty(),
            "a None remote reply rolls the user turn back"
        );
        assert!(
            !has_reply_widget(&app),
            "a None remote reply leaves no (empty reply) widget"
        );
    }

    #[test]
    fn ephemeral_remote_failure_keeps_prior_messages() {
        // An ephemeral /btw never added a turn to self.messages (remember = false), so a
        // failure must not pop an unrelated, earlier turn.
        let mut app = test_app();
        app.messages
            .push(RelayMessage::new("user", "an earlier kept turn"));
        app.pending = Some(reply_pending(true, false));
        let mut finished = false;
        app.apply_reply_event(ReplyEvent::DiscardTurn, &mut finished);
        assert_eq!(
            app.messages.len(),
            1,
            "an ephemeral remote failure does not drop a prior turn"
        );
    }

    #[test]
    fn in_process_error_keeps_user_turn() {
        // Parity guard: the in-process stream worker intentionally keeps the user turn on
        // failure (retry on the same context) and renders an error widget.
        let mut app = test_app();
        app.messages.push(RelayMessage::new("user", "stream this"));
        app.pending = Some(reply_pending(false, true));
        let mut finished = false;
        app.apply_reply_event(
            ReplyEvent::Error("upstream blew up".to_owned()),
            &mut finished,
        );
        assert_eq!(
            app.messages.len(),
            1,
            "the in-process stream worker keeps the user turn on failure"
        );
        assert!(
            has_reply_widget(&app),
            "the in-process failure renders an error reply widget"
        );
    }
}
