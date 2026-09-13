mod cli;

fn main() {
    if let Err(error) = cli::run() {
        eprintln!("error: {error}");
        // A stage failure is only actionable when its cause is visible: the top
        // message says which stage failed, not what the store or the target said.
        let mut source = error.source();
        while let Some(cause) = source {
            eprintln!("  caused by: {cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}
