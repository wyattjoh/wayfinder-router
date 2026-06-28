#[test]
fn serve_summary_mentions_current_gateway_shape() {
    let options = wayfinder_internal_gateway::ServeOptions {
        host: "127.0.0.1".to_owned(),
        port: 8088,
        dry_run: true,
        timeout_seconds: Some(30.0),
    };

    let message = wayfinder_internal_gateway::serve_summary(&options);

    assert!(message.contains("serve"));
    assert!(message.contains("127.0.0.1:8088"));
    assert!(message.contains("dry-run"));
    assert!(!message.contains("not implemented"));
}
