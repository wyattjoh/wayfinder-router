use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use wayfinder_internal_tui::{
    apply_chat_line, list_thread_summaries, load_thread, render_route_decision, run_chat,
    save_thread, should_launch_interactive, ChatCommand, ChatOptions, ChatState, RouteMode,
};

#[test]
fn terminal_without_input_launches_interactive() {
    // The interactive app is the new default: a real terminal with nothing to route.
    assert!(should_launch_interactive(true, false));
}

#[test]
fn prompt_or_piped_input_keeps_transcript() {
    // A prompt argument or piped stdin populates `input`, so the scriptable
    // transcript path is used even on a terminal.
    assert!(!should_launch_interactive(true, true));
    assert!(!should_launch_interactive(false, true));
}

#[test]
fn non_terminal_without_input_keeps_transcript() {
    // No terminal and no input (for example a closed/empty pipe) stays on the
    // transcript path rather than launching a UI with no stdin to drive it.
    assert!(!should_launch_interactive(false, false));
}

#[test]
fn transcript_path_renders_for_prompt_input() {
    // Coverage for the non-interactive path the decision falls back to.
    let options = ChatOptions {
        dry_run: true,
        input: Some("What is DNS?".to_string()),
        ..ChatOptions::default()
    };

    let output = run_chat(&options).expect("transcript should render");

    assert!(output.contains("wayfinder-router chat"));
    assert!(output.contains("prompt: What is DNS?"));
    assert!(output.contains("route: local"));
}

#[test]
fn parser_recognizes_chat_commands_and_plain_prompts() {
    assert_eq!(ChatCommand::parse("/why"), ChatCommand::Why);
    assert_eq!(ChatCommand::parse("/route"), ChatCommand::Route);
    assert_eq!(
        ChatCommand::parse("/local"),
        ChatCommand::SetRoute(RouteMode::Local)
    );
    assert_eq!(
        ChatCommand::parse("/cloud"),
        ChatCommand::SetRoute(RouteMode::Cloud)
    );
    assert_eq!(
        ChatCommand::parse("Explain DNS in one sentence."),
        ChatCommand::Prompt("Explain DNS in one sentence.".to_string())
    );
}

#[test]
fn route_decision_rendering_uses_core_score() {
    let state = ChatState::default();
    let output = render_route_decision("What is DNS?", &state);

    assert!(output.contains("route: local"));
    assert!(output.contains("score:"));
    assert!(output.contains("mode: tiered"));
}

#[test]
fn dry_run_outputs_route_and_why_without_gateway_call() {
    let options = ChatOptions {
        dry_run: true,
        show_why: true,
        input: Some("What is DNS?".to_string()),
        ..ChatOptions::default()
    };

    let output = run_chat(&options).expect("dry-run chat should render");

    assert!(output.contains("wayfinder-router chat"));
    assert!(output.contains("dry-run: true"));
    assert!(output.contains("route: local"));
    assert!(output.contains("why:"));
    assert!(output.contains("gateway: skipped"));
}

#[test]
fn chat_state_tracks_route_override_and_why_for_last_prompt() {
    let mut state = ChatState::default();

    assert_eq!(
        apply_chat_line(&mut state, "/cloud"),
        "route override: cloud"
    );
    assert_eq!(state.route_mode, RouteMode::Cloud);

    let prompt_output = apply_chat_line(&mut state, "Prove something hard.");
    assert!(prompt_output.contains("route: cloud"));

    let why_output = apply_chat_line(&mut state, "/why");
    assert!(why_output.contains("why:"));
    assert!(why_output.contains("Prove something hard."));
}

#[test]
fn thread_json_saves_loads_and_lists_core_shape() {
    let dir = unique_temp_dir();
    fs::create_dir_all(&dir).expect("temp dir should be created");

    let saved = save_thread(&dir, "thread-a", ["What is DNS?", "What is HTTP?"])
        .expect("thread should save");
    let loaded = load_thread(&dir, "thread-a").expect("thread should load");
    let summaries = list_thread_summaries(&dir).expect("threads should list");

    assert_eq!(saved, loaded);
    assert_eq!(loaded.id, "thread-a");
    assert_eq!(loaded.title, "What is DNS?");
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(summaries, vec!["thread-a\tWhat is DNS?".to_string()]);
}

fn unique_temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("wayfinder-tui-test-{}-{nanos}", std::process::id()))
}
