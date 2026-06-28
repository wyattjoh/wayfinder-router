fn main() {
    match wayfinder_internal_cli::run(std::env::args().skip(1)) {
        Ok(message) => println!("{message}"),
        Err(err) => {
            eprintln!("wayfinder-router: {err}");
            std::process::exit(2);
        }
    }
}
