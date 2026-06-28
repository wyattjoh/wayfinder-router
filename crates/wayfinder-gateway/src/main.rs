use wayfinder_internal_gateway::{serve_blocking, ServeOptions, COMMAND_NAME};

fn main() {
    match parse_args(std::env::args().skip(1)) {
        Ok(options) => {
            if let Err(err) = serve_blocking(options) {
                eprintln!("wayfinder-router-gateway: {err}");
                std::process::exit(1);
            }
        }
        Err(err) => {
            eprintln!("wayfinder-router-gateway: {err}");
            std::process::exit(2);
        }
    }
}

fn parse_args<I>(args: I) -> Result<ServeOptions, String>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let mut args = args.into_iter().map(Into::into).peekable();
    if matches!(args.peek().map(String::as_str), Some(COMMAND_NAME)) {
        args.next();
    }

    let mut options = ServeOptions::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => options.host = next_value(&mut args, "--host")?,
            "--port" => {
                options.port = next_value(&mut args, "--port")?
                    .parse()
                    .map_err(|_| "--port must be an integer".to_owned())?;
            }
            "--dry-run" => options.dry_run = true,
            "--timeout" => {
                options.timeout_seconds = Some(
                    next_value(&mut args, "--timeout")?
                        .parse()
                        .map_err(|_| "--timeout must be a number".to_owned())?,
                );
            }
            other => return Err(format!("unknown serve option '{other}'")),
        }
    }
    Ok(options)
}

fn next_value<I>(args: &mut I, flag: &str) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}
