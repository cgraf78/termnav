use std::env;
use std::io;
use std::process;

fn main() {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut raw = env::args_os();
    let program = raw.next().unwrap_or_else(|| "termnav".into());
    let arguments = termnav::cli::normalize_argv(&program, raw);

    match termnav::cli::run(arguments, &mut stdout, &mut stderr) {
        Ok(code) => process::exit(code),
        Err(error) => {
            eprintln!("termnav: {error}");
            process::exit(1);
        }
    }
}
