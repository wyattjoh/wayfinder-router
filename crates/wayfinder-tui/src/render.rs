//! Pure ratatui render helpers ported from the Python TUI's rich renderables
//! (`render_welcome`, `render_decision`, the `/settings` `/models` `/keys` `/threads`
//! `/cost` panels, `render_empty_state`, `_status_bar`, `_footer_bar`, `render_reply`).
//!
//! Every function here is a pure builder: it takes data plus a [`Palette`] and returns a
//! ratatui [`Text`] (panels frame their body in a rounded box, also drawn as lines) with no
//! terminal I/O and no event loop, so the app shell owns the loop and these stay testable
//! with `TestBackend`. The brand em dashes from the Python source are spelled as hyphens or
//! the middle dot here, per the repo's no-em-dash rule.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

use wayfinder_internal_core::pricing::SavingsLedger;
use wayfinder_internal_core::threads::Thread;
use wayfinder_internal_gateway::bootstrap::{key_status, suggest_key_commands};
use wayfinder_internal_gateway::GatewayModel;

use crate::decision::{pin_label, Decision, TuiState};
use crate::theme::Palette;

/// The wordmark that heads the transcript (pyfiglet "ansi_shadow", baked so figlet is
/// never a runtime dependency). It spells WAYFINDER in box-drawing blocks.
const WORDMARK: &str = "\
██╗    ██╗ █████╗ ██╗   ██╗███████╗██╗███╗   ██╗██████╗ ███████╗██████╗
██║    ██║██╔══██╗╚██╗ ██╔╝██╔════╝██║████╗  ██║██╔══██╗██╔════╝██╔══██╗
██║ █╗ ██║███████║ ╚████╔╝ █████╗  ██║██╔██╗ ██║██║  ██║█████╗  ██████╔╝
██║███╗██║██╔══██║  ╚██╔╝  ██╔══╝  ██║██║╚██╗██║██║  ██║██╔══╝  ██╔══██╗
╚███╔███╔╝██║  ██║   ██║   ██║     ██║██║ ╚████║██████╔╝███████╗██║  ██║
 ╚══╝╚══╝ ╚═╝  ╚═╝   ╚═╝   ╚═╝     ╚═╝╚═╝  ╚═══╝╚═════╝ ╚══════╝╚═╝  ╚═╝";

/// Cost-panel period windows: today / 7d / 30d / all-time (mirrors `_COST_PERIODS`).
const COST_PERIODS: [(&str, Option<i64>); 4] = [
    ("today", Some(1)),
    ("7 days", Some(7)),
    ("30 days", Some(30)),
    ("all time", None),
];

// --- small span/line builders ------------------------------------------------

fn span(content: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(content.into(), Style::new().fg(color))
}

fn bold(content: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(
        content.into(),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    )
}

/// A right-justified label paired with its value, the shape the rich `Table.grid` panels use.
fn kv_line(label: &str, value: Span<'static>, width: usize, muted: Color) -> Line<'static> {
    Line::from(vec![
        span(format!("{label:>width$}"), muted),
        Span::raw("   "),
        value,
    ])
}

/// Frame `body` in a rounded, content-width box titled `title`, drawn as plain lines.
///
/// The Python panels lean on rich's `Panel` (rounded border, `padding=(1, 2)`,
/// `expand=False`); the chat shell renders the transcript as one flat list of [`Line`]s,
/// so the border is drawn here into [`Text`] once rather than re-implemented per panel.
/// Width is measured from the body (shrink-to-content, like `expand=False`); each line's
/// display width is approximated by its char count, which the panels' mostly-ASCII content
/// makes exact.
fn panel(title: &'static str, body: Text<'static>, palette: &Palette) -> Text<'static> {
    let accent = palette.accent;
    let line_width = |line: &Line| -> usize {
        line.spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum()
    };

    let content_width = body.lines.iter().map(line_width).max().unwrap_or(0);
    // The interior between the verticals: two-space padding on each side of the content.
    let title_min = title.chars().count() + 3; // "─ {title} "
    let inner = (content_width + 4).max(title_min);

    let border = |text: String| Span::styled(text, Style::new().fg(accent));
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(body.lines.len() + 4);

    let prefix = format!("─ {title} ");
    let fill = inner.saturating_sub(prefix.chars().count());
    lines.push(Line::from(border(format!(
        "╭{prefix}{}╮",
        "─".repeat(fill)
    ))));
    lines.push(Line::from(border(format!("│{}│", " ".repeat(inner)))));
    for line in body.lines {
        let trailing = inner.saturating_sub(4 + line_width(&line));
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 3);
        spans.push(border("│".to_owned()));
        spans.push(Span::raw("  ".to_owned()));
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(trailing + 2)));
        spans.push(border("│".to_owned()));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(border(format!("│{}│", " ".repeat(inner)))));
    lines.push(Line::from(border(format!("╰{}╯", "─".repeat(inner)))));
    Text::from(lines)
}

fn glyph_role(is_local: bool) -> (&'static str, &'static str) {
    if is_local {
        ("●", "LOCAL")
    } else {
        ("◆", "CLOUD")
    }
}

// --- welcome -----------------------------------------------------------------

/// The transcript header: the wordmark, brand subtitle, and a functional hint.
///
/// `compact` swaps the block wordmark for a plain "Wayfinder" line on narrow terminals.
pub fn render_welcome(palette: &Palette, subtitle: &str, compact: bool) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = vec![Line::raw("")];
    if compact {
        lines.push(Line::from(bold("Wayfinder", palette.accent)).centered());
    } else {
        for row in WORDMARK.lines() {
            lines.push(Line::from(bold(row.to_owned(), palette.accent)).centered());
        }
    }
    lines.push(Line::from(span(subtitle.to_owned(), palette.muted)).centered());
    lines.push(Line::raw(""));
    lines.push(
        Line::from(span(
            "type a prompt - Wayfinder routes it and shows the score + why",
            palette.text,
        ))
        .centered(),
    );
    lines.push(Line::raw(""));
    lines.push(
        Line::from(vec![
            span("local ", palette.muted),
            span("✓   ", palette.accent),
            span("cloud ", palette.muted),
            span("✓   ", palette.cloud),
            span("offline routing ", palette.muted),
            span("✓", palette.accent),
        ])
        .centered(),
    );
    lines.push(Line::raw(""));
    Text::from(lines)
}

// --- decision ----------------------------------------------------------------

/// The decision line; collapsed shows a `/why` affordance, expanded adds the table.
///
/// `forced_to` is `(model_name, is_local)` when the route was overridden: the forced
/// target is shown as the primary, flagged `· forced`, with the natural route the scorer
/// would have picked shown alongside (decision-first transparency).
pub fn render_decision(
    decision: &Decision,
    palette: &Palette,
    expanded: bool,
    forced_to: Option<(&str, bool)>,
) -> Text<'static> {
    let (muted, text_c) = (palette.muted, palette.text);
    let caret = if expanded { "⌃" } else { "⌄" };

    let head: Vec<Span<'static>> = match forced_to {
        Some((f_name, f_local)) => {
            let (f_glyph, f_role) = glyph_role(f_local);
            let role_color = if f_local {
                palette.accent
            } else {
                palette.cloud
            };
            let mut spans = vec![
                bold(format!("{f_glyph} {f_role}"), role_color),
                span(format!("  {f_name}"), text_c),
                span("  · forced", palette.warn),
                span(format!("   score {:.2}", decision.score), muted),
            ];
            if f_name != decision.model {
                let (n_glyph, n_role) = glyph_role(decision.is_local);
                spans.push(span(format!("   would route {n_glyph} {n_role}"), muted));
            }
            if !decision.contributions.is_empty() {
                spans.push(span(format!("   /why {caret}"), muted));
            }
            spans
        }
        None => {
            let (glyph, role) = glyph_role(decision.is_local);
            let role_color = if decision.is_local {
                palette.accent
            } else {
                palette.cloud
            };
            let mut spans = vec![
                bold(format!("{glyph} {role}"), role_color),
                span(format!("  {}", decision.model), text_c),
                span(format!("   score {:.2}", decision.score), muted),
            ];
            if decision.is_local {
                spans.push(span("  · kept local", muted));
            }
            if !decision.contributions.is_empty() {
                spans.push(span(format!("   /why {caret}"), muted));
            }
            spans
        }
    };

    let mut lines = vec![Line::from(head)];
    if expanded && !decision.contributions.is_empty() {
        let mut sorted = decision.contributions.clone();
        sorted.sort_by(|a, b| {
            b.contribution
                .partial_cmp(&a.contribution)
                .unwrap_or(Ordering::Equal)
        });
        for fc in sorted.into_iter().take(5) {
            lines.push(Line::from(vec![
                span(format!("  {}", fc.name), muted),
                span(format!("   {}", fc.value), muted),
                span(
                    format!(
                        "   {:.2}×{} = {:.3}",
                        fc.normalized, fc.weight, fc.contribution
                    ),
                    muted,
                ),
            ]));
        }
    }
    Text::from(lines)
}

// --- settings ----------------------------------------------------------------

/// A settings panel: the live routing controls and how to change them.
pub fn render_settings(state: &TuiState, palette: &Palette) -> Text<'static> {
    let (muted, text_c) = (palette.muted, palette.text);
    let rows: Vec<(&str, String)> = vec![
        (
            "forced route",
            match state.pinned.as_deref() {
                Some(pin) => pin_label(Some(pin)),
                None => "auto (routing)".to_owned(),
            },
        ),
        (
            "threshold",
            match state.threshold {
                Some(threshold) => format!("{threshold:.2}"),
                None => "auto (config)".to_owned(),
            },
        ),
        ("routing scope", state.scope.clone()),
        (
            "sticky",
            if state.sticky {
                format!("on · cooldown {}", state.cooldown)
            } else {
                "off".to_owned()
            },
        ),
        (
            "why breakdown",
            if state.show_why {
                "expanded"
            } else {
                "collapsed"
            }
            .to_owned(),
        ),
        (
            "streaming",
            if state.stream { "on" } else { "off" }.to_owned(),
        ),
        ("theme", state.theme.clone()),
    ];
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let mut lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|(label, value)| kv_line(label, span(value, text_c), width, muted))
        .collect();
    lines.push(Line::raw(""));
    lines.push(span(
        "change:  /route  /local  /cloud  /threshold  /scope  /sticky  /why  /stream  /theme   ·   /help",
        muted,
    ).into());
    panel("settings", Text::from(lines), palette)
}

// --- models ------------------------------------------------------------------

/// A panel of the configured models and whether each one's key resolves.
///
/// The in-chat equivalent of `wayfinder-router doctor`: keys are read from the
/// environment, never stored (WF-ADR-0004); this only reports `set` / `not set`.
pub fn render_models(models: &BTreeMap<String, GatewayModel>, palette: &Palette) -> Text<'static> {
    let (accent, muted, text_c, cloud, warn) = (
        palette.accent,
        palette.muted,
        palette.text,
        palette.cloud,
        palette.warn,
    );
    if models.is_empty() {
        let body = Text::from(span(
            "no models configured - type /init to scaffold one",
            muted,
        ));
        return panel("models", body, palette);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    for status in key_status(models) {
        let (key_text, glyph_color) = match (&status.env_var, status.ok) {
            (None, _) => (span("keyless ✓", accent), accent),
            (Some(var), true) => {
                let mut label = format!("{var} ✓ set");
                if status.cmd.is_some() {
                    label.push_str(" (via command)");
                }
                (span(label, accent), accent)
            }
            (Some(var), false) => (span(format!("{var} ✗ not set"), warn), cloud),
        };
        lines.push(Line::from(vec![
            span(format!("{}  ", status.name), text_c),
            span(format!("{}  ", status.model), muted),
            span(format!("{}  ", status.base_url), muted),
            span("● ", glyph_color),
            key_text,
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(
        span(
            "keys live in your environment · /init to add models · /route to pin",
            muted,
        )
        .into(),
    );
    panel("models", Text::from(lines), palette)
}

// --- keys --------------------------------------------------------------------

/// A focused, actionable view of each model's key: the in-chat `doctor`.
///
/// `errors` maps a model name to a failed `api_key_cmd` message (re-resolution happens in
/// the caller). Keys are read from the environment or a secret store at request time,
/// never written to disk (WF-ADR-0004, WF-DESIGN-0006).
pub fn render_keys(
    models: &BTreeMap<String, GatewayModel>,
    palette: &Palette,
    errors: Option<&BTreeMap<String, String>>,
) -> Text<'static> {
    let (accent, muted, text_c, cloud, warn) = (
        palette.accent,
        palette.muted,
        palette.text,
        palette.cloud,
        palette.warn,
    );
    let empty = BTreeMap::new();
    let errors = errors.unwrap_or(&empty);
    if models.is_empty() {
        let body = Text::from(span(
            "no models configured - type /init to scaffold one",
            muted,
        ));
        return panel("keys", body, palette);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for status in key_status(models) {
        let (status_text, glyph_color) = match (&status.env_var, status.ok) {
            (None, _) => (span("keyless - no key needed", muted), accent),
            (Some(var), true) => {
                let via = if status.cmd.is_some() {
                    "resolved via command"
                } else {
                    "set in environment"
                };
                (span(format!("{var}  ✓ {via}"), accent), accent)
            }
            (Some(var), false) => {
                missing.push(var.clone());
                let text = match errors.get(&status.name) {
                    Some(err) => format!("{var}  ✗ command failed - {err}"),
                    None => format!("{var}  ✗ not set"),
                };
                (span(text, warn), cloud)
            }
        };
        lines.push(Line::from(vec![
            span(format!("{}  ", status.name), text_c),
            span("● ", glyph_color),
            status_text,
        ]));
    }

    // Dedupe a var shared across tiers, keeping first-seen order.
    let mut unset: Vec<String> = Vec::new();
    for var in missing {
        if !unset.contains(&var) {
            unset.push(var);
        }
    }
    if !unset.is_empty() {
        lines.push(Line::raw(""));
        lines.push(
            span(
                "to fix - read at request time, never written to disk:",
                muted,
            )
            .into(),
        );
        for var in &unset {
            lines.push(span(format!("  export {var}=…"), text_c).into());
            let suggestions = suggest_key_commands(var);
            if suggestions.is_empty() {
                lines.push(
                    span(
                        "  · or store it in your secret manager and add an api_key_cmd",
                        muted,
                    )
                    .into(),
                );
            } else {
                for cmd in suggestions {
                    lines.push(span(format!("  · or add:  api_key_cmd = \"{cmd}\""), muted).into());
                }
            }
        }
    }
    lines.push(Line::raw(""));
    lines.push(
        span(
            "/keys re-checks · keys live in your environment or your secret store",
            muted,
        )
        .into(),
    );
    panel("keys", Text::from(lines), palette)
}

// --- empty state -------------------------------------------------------------

/// The onboarding panel shown when no models are configured (in-process, no --dry-run).
pub fn render_empty_state(palette: &Palette) -> Text<'static> {
    let (accent, muted, text_c) = (palette.accent, palette.muted, palette.text);
    let preset = |cmd: &str, desc: &str| -> Line<'static> {
        Line::from(vec![
            span(cmd.to_owned(), accent),
            span(desc.to_owned(), muted),
        ])
    };
    let lines: Vec<Line<'static>> = vec![
        span(
            "You're in preview - routing decisions only, no replies yet.",
            text_c,
        )
        .into(),
        Line::raw(""),
        span("Add models without leaving the chat:", muted).into(),
        preset(
            "  /init",
            "          scaffold the hybrid preset (keyless local Ollama → Anthropic cloud)",
        ),
        preset(
            "  /init local",
            "    a single keyless Ollama arm with offline delivery enforced",
        ),
        preset(
            "  /init openai",
            "   two OpenAI tiers (gpt-4o-mini → gpt-4o)",
        ),
        preset(
            "  /init gemini",
            "   two Gemini tiers (gemini-2.5-flash → gemini-2.5-pro)",
        ),
        preset(
            "  /keys",
            "          after /init: check & resolve your keys, with fix-it hints",
        ),
        Line::raw(""),
        Line::from(vec![
            span(
                "Keyless local replies work as soon as Ollama is running ",
                muted,
            ),
            span("(ollama serve)", text_c),
            span(".", muted),
        ]),
    ];
    panel("get started", Text::from(lines), palette)
}

// --- threads -----------------------------------------------------------------

/// A numbered list of saved conversations (newest first); `/open <n>` reopens one.
pub fn render_threads(entries: &[Thread], palette: &Palette) -> Text<'static> {
    let (accent, muted, text_c) = (palette.accent, palette.muted, palette.text);
    if entries.is_empty() {
        let body = Text::from(span(
            "no saved conversations yet - they save automatically as you chat",
            muted,
        ));
        return panel("threads", body, palette);
    }

    let mut ordered: Vec<&Thread> = entries.iter().collect();
    ordered.sort_by(|a, b| {
        let key = |t: &Thread| {
            if t.updated.is_empty() {
                t.created.clone()
            } else {
                t.updated.clone()
            }
        };
        key(b).cmp(&key(a)) // newest first
    });

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, thread) in ordered.iter().enumerate() {
        let stamp = if thread.updated.is_empty() {
            &thread.created
        } else {
            &thread.updated
        };
        let when = stamp.replace('T', " ");
        let when = when.trim_end_matches('Z').to_owned();
        let title = if thread.title.is_empty() {
            "(untitled)".to_owned()
        } else {
            thread.title.clone()
        };
        lines.push(Line::from(vec![
            span(format!("{:>3}", i + 1), accent),
            span(format!("  {title}"), text_c),
            span(format!("  {when}"), muted),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(span("/open <n> to reopen · /new to start fresh", muted).into());
    panel("threads", Text::from(lines), palette)
}

// --- cost --------------------------------------------------------------------

/// A panel breaking down the session's routing mix and estimated savings.
///
/// When a persisted `ledger` is supplied it also shows a per-period view
/// (today / 7d / 30d / all-time), so savings accrue across sessions (WF-DESIGN-0007).
pub fn render_cost(
    tally: &crate::SessionCost,
    palette: &Palette,
    ledger: Option<&SavingsLedger>,
) -> Text<'static> {
    let (accent, muted, text_c) = (palette.accent, palette.muted, palette.text);
    if tally.calls == 0 {
        let body = Text::from(span("no model calls yet this session", muted));
        return panel("cost", body, palette);
    }

    let pct = (100.0 * tally.local as f64 / tally.calls as f64).round() as i64;
    let mut rows: Vec<(&str, String)> = vec![
        ("model calls", tally.calls.to_string()),
        ("kept local", format!("{}  ({pct}%)", tally.local)),
    ];
    if tally.priced {
        rows.push(("est. spent", format!("~${:.4}", tally.spent)));
        rows.push((
            "est. saved",
            format!("~${:.4}  vs always-cloud", tally.saved),
        ));
    }
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);

    let mut lines: Vec<Line<'static>> = vec![span("this session", muted).into()];
    for (label, value) in rows {
        lines.push(kv_line(label, span(value, text_c), width, muted));
    }

    // Per-period view, only when the ledger has accrued anything.
    if let Some(ledger) = ledger {
        if ledger.period(None, None).requests > 0 {
            lines.push(Line::raw(""));
            lines.push(span("by period", muted).into());
            let mut header = vec![span("period", muted), span("   calls", muted)];
            if ledger.priced {
                header.push(span("   saved", accent));
            }
            lines.push(Line::from(header));
            for (label, days) in COST_PERIODS {
                let rep = ledger.period(days, None);
                let mut cols = vec![
                    span(format!("{label:>8}"), muted),
                    span(format!("   {}", rep.requests), text_c),
                ];
                if ledger.priced {
                    cols.push(span(format!("   ~${:.4}", rep.saved), accent));
                }
                lines.push(Line::from(cols));
            }
        }
    }

    let mut tail = "estimated from ~4 chars/token".to_owned();
    if !tally.priced {
        tail.push_str(" · set cost_per_1k on your models for $ figures");
    }
    lines.push(Line::raw(""));
    lines.push(span(tail, muted).into());
    panel("cost", Text::from(lines), palette)
}

// --- status / footer bars ----------------------------------------------------

/// The one-line status bar as `(left, right)`: routing mode + thresholds (or a transient
/// note) on the left, the local/cloud legend on the right. The app shell justifies the two.
pub fn status_bar(
    state: &TuiState,
    palette: &Palette,
    note: Option<&str>,
) -> (Line<'static>, Line<'static>) {
    let (accent, muted, cloud, warn) = (palette.accent, palette.muted, palette.cloud, palette.warn);
    let left = if let Some(note) = note {
        Line::from(vec![span("⠿ ", cloud), span(note.to_owned(), muted)])
    } else if let Some(pin) = state.pinned.as_deref() {
        Line::from(vec![
            span(format!("forced → {}", pin_label(Some(pin))), warn),
            span("  ·  /auto to resume routing", muted),
        ])
    } else {
        let thr = match state.threshold {
            Some(threshold) => format!("{threshold:.2}"),
            None => "auto".to_owned(),
        };
        Line::from(vec![
            span("decision-first routing", accent),
            span(
                format!("  ·  threshold {thr}  ·  scope {}", state.scope),
                muted,
            ),
        ])
    };
    let right = Line::from(vec![
        span("● local", accent),
        span("  /  ", muted),
        span("◆ cloud", cloud),
    ]);
    (left, right)
}

/// The footer hint line as `(left, right)`.
pub fn footer_bar(palette: &Palette, right: &str) -> (Line<'static>, Line<'static>) {
    let muted = palette.muted;
    (
        Line::from(span(
            "/help   ·   ↑↓ history   ·   ctrl-c cancel / quit",
            muted,
        )),
        Line::from(span(right.to_owned(), muted)),
    )
}

// --- model reply -------------------------------------------------------------

/// Render a model reply as markdown-ish text: fenced code blocks render dimmed, list
/// markers become bullets, and everything else stays plain. Mirrors the Python
/// `render_reply` (which leans on rich's Markdown) at the fidelity a buffer needs.
pub fn render_reply(text: &str) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code; // hide the fence markers themselves
            continue;
        }
        if in_code {
            lines.push(Line::styled(
                format!("  {raw}"),
                Style::new().add_modifier(Modifier::DIM),
            ));
            continue;
        }
        if let Some(item) = list_item(trimmed) {
            let indent = &raw[..raw.len() - trimmed.len()];
            lines.push(Line::raw(format!("{indent}• {item}")));
            continue;
        }
        lines.push(Line::raw(raw.to_owned()));
    }
    Text::from(lines)
}

/// The content of a markdown list item (bullet or numbered), or `None` for a plain line.
fn list_item(trimmed: &str) -> Option<String> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some(rest.to_owned());
        }
    }
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        if let Some(rest) = trimmed[digits.len()..].strip_prefix(". ") {
            return Some(format!("{digits}. {rest}"));
        }
    }
    None
}
