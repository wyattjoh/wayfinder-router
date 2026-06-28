#[test]
fn chat_stub_mentions_current_tui_shape() {
    let options = wayfinder_internal_tui::ChatOptions {
        theme: "auto".to_owned(),
        threshold: Some(0.5),
        show_why: true,
        dry_run: true,
        stream: false,
        base_url: Some("http://127.0.0.1:8088".to_owned()),
    };

    let message = wayfinder_internal_tui::chat_placeholder(&options);

    assert!(message.contains("chat"));
    assert!(message.contains("theme=auto"));
    assert!(message.contains("dry-run"));
    assert!(message.contains("not implemented"));
}
