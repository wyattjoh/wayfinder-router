fn main() {
    let mut options = wayfinder_internal_tui::ChatOptions::default();
    let mut args = std::env::args().skip(1);
    if matches!(args.next().as_deref(), Some("chat")) {
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--theme" => options.theme = args.next().unwrap_or(options.theme),
                "--threshold" => {
                    options.threshold = args.next().and_then(|value| value.parse().ok())
                }
                "--why" => options.show_why = true,
                "--dry-run" => options.dry_run = true,
                "--no-stream" => options.stream = false,
                "--base-url" => options.base_url = args.next(),
                _ => {}
            }
        }
    }
    println!("{}", wayfinder_internal_tui::chat_placeholder(&options));
}
