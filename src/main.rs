use std::env;
use std::io;
use std::process;

fn main() {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();

    match termnav::cli::run(env::args_os().skip(1), &mut stdout, &mut stderr) {
        Ok(code) => process::exit(code),
        Err(error) => {
            eprintln!("termnav: {error}");
            process::exit(1);
        }
    }
}
