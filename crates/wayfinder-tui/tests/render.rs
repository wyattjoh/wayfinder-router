//! Buffer-level tests for the render helpers, exercised through ratatui's `TestBackend`.
//!
//! Each test draws a renderable into an off-screen terminal and asserts on the flattened
//! buffer text, so the decision-first line, the panels, and their styling survive an actual
//! ratatui render pass (not just construction).

use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Terminal;

use wayfinder_internal_core::complexity::FeatureContribution;
use wayfinder_internal_core::threads::Thread;
use wayfinder_internal_tui::{
    palette_for, render_cost, render_decision, render_settings, render_threads, Decision,
    SessionCost, TuiState,
};

/// Flatten a rendered widget into one newline-joined string of cell symbols.
fn buffer_text<W: Widget>(widget: W, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| frame.render_widget(widget, Rect::new(0, 0, width, height)))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            out.push_str(buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

fn decision_text(text: Text<'static>, width: u16, height: u16) -> String {
    buffer_text(Paragraph::new(text), width, height)
}

fn local_decision() -> Decision {
    Decision {
        text: "summarize this".to_owned(),
        model: "ollama".to_owned(),
        score: 0.12,
        mode: "lexical".to_owned(),
        is_local: true,
        contributions: vec![
            FeatureContribution {
                name: "word_count".to_owned(),
                value: 3,
                normalized: 0.10,
                weight: 1.0,
                contribution: 0.10,
            },
            FeatureContribution {
                name: "code_blocks".to_owned(),
                value: 0,
                normalized: 0.0,
                weight: 2.0,
                contribution: 0.0,
            },
        ],
        threshold: None,
        targets: vec!["ollama".to_owned(), "claude".to_owned()],
    }
}

#[test]
fn decision_collapsed_shows_local_glyph_score_and_why_affordance() {
    let palette = palette_for("dark");
    let decision = local_decision();
    let out = decision_text(render_decision(&decision, &palette, false, None), 120, 4);

    assert!(out.contains("LOCAL"), "missing LOCAL role:\n{out}");
    assert!(out.contains('●'), "missing local glyph:\n{out}");
    assert!(out.contains("ollama"), "missing model:\n{out}");
    assert!(out.contains("score 0.12"), "missing score:\n{out}");
    assert!(out.contains("/why"), "missing why affordance:\n{out}");
    // Collapsed: the contributions table is not rendered yet.
    assert!(!out.contains("word_count"), "should be collapsed:\n{out}");
}

#[test]
fn decision_expanded_includes_top_contribution() {
    let palette = palette_for("dark");
    let decision = local_decision();
    let out = decision_text(render_decision(&decision, &palette, true, None), 120, 10);

    assert!(out.contains("LOCAL"), "missing LOCAL role:\n{out}");
    assert!(out.contains("score 0.12"), "missing score:\n{out}");
    // Expanded: the top contribution (sorted by contribution desc) is shown.
    assert!(
        out.contains("word_count"),
        "missing top contribution:\n{out}"
    );
}

#[test]
fn decision_cloud_route_shows_cloud_glyph() {
    let palette = palette_for("dark");
    let mut decision = local_decision();
    decision.is_local = false;
    decision.model = "claude".to_owned();
    decision.score = 0.81;
    let out = decision_text(render_decision(&decision, &palette, false, None), 120, 4);

    assert!(out.contains("CLOUD"), "missing CLOUD role:\n{out}");
    assert!(out.contains('◆'), "missing cloud glyph:\n{out}");
    assert!(out.contains("score 0.81"), "missing score:\n{out}");
}

#[test]
fn decision_forced_marks_override_and_shows_natural_route() {
    let palette = palette_for("dark");
    let decision = local_decision(); // natural route is LOCAL/ollama
    let out = decision_text(
        render_decision(&decision, &palette, false, Some(("claude", false))),
        120,
        4,
    );

    assert!(
        out.contains("CLOUD"),
        "forced target should read CLOUD:\n{out}"
    );
    assert!(out.contains("claude"), "missing forced target:\n{out}");
    assert!(out.contains("forced"), "missing forced marker:\n{out}");
    assert!(
        out.contains("would route ● LOCAL"),
        "missing natural-route note:\n{out}"
    );
}

#[test]
fn settings_panel_shows_controls() {
    let palette = palette_for("dark");
    let state = TuiState::default();
    let out = buffer_text(render_settings(&state, &palette), 100, 14);

    assert!(out.contains("settings"), "missing panel title:\n{out}");
    assert!(out.contains("threshold"), "missing threshold row:\n{out}");
    assert!(
        out.contains("auto (config)"),
        "missing threshold value:\n{out}"
    );
    assert!(out.contains("streaming"), "missing streaming row:\n{out}");
}

#[test]
fn threads_panel_numbers_newest_first() {
    let palette = palette_for("dark");
    let entries = vec![
        Thread {
            id: "a".to_owned(),
            title: "older chat".to_owned(),
            created: "2024-01-01T00:00:00Z".to_owned(),
            updated: "2024-01-01T00:00:00Z".to_owned(),
            messages: Vec::new(),
        },
        Thread {
            id: "b".to_owned(),
            title: "newer chat".to_owned(),
            created: "2024-06-01T00:00:00Z".to_owned(),
            updated: "2024-06-01T00:00:00Z".to_owned(),
            messages: Vec::new(),
        },
    ];
    let out = buffer_text(render_threads(&entries, &palette), 100, 10);

    assert!(out.contains("threads"), "missing panel title:\n{out}");
    assert!(out.contains("newer chat"), "missing newest thread:\n{out}");
    assert!(out.contains("older chat"), "missing older thread:\n{out}");
    // Newest-first: the newer chat is numbered 1, ahead of the older one.
    let one = out.find("newer chat").expect("newer present");
    let two = out.find("older chat").expect("older present");
    assert!(one < two, "newest thread should sort first:\n{out}");
}

#[test]
fn cost_panel_shows_session_mix() {
    let palette = palette_for("dark");
    let tally = SessionCost {
        calls: 4,
        local: 3,
        spent: 0.0,
        saved: 0.0,
        priced: false,
    };
    let out = buffer_text(render_cost(&tally, &palette, None), 100, 12);

    assert!(out.contains("cost"), "missing panel title:\n{out}");
    assert!(out.contains("kept local"), "missing routing mix:\n{out}");
    assert!(out.contains("model calls"), "missing call count:\n{out}");
}
