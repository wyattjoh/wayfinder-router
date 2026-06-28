fn main() {
    let mut options = wayfinder_internal_gateway::ServeOptions::default();
    let mut args = std::env::args().skip(1);
    if matches!(args.next().as_deref(), Some("serve")) {
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--host" => options.host = args.next().unwrap_or(options.host),
                "--port" => {
                    options.port = args
                        .next()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(options.port);
                }
                "--dry-run" => options.dry_run = true,
                "--timeout" => {
                    options.timeout_seconds = args.next().and_then(|value| value.parse().ok())
                }
                _ => {}
            }
        }
    }
    println!(
        "{}",
        wayfinder_internal_gateway::serve_placeholder(&options)
    );
}
