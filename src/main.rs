use std::env;
use std::io;
use std::process;

fn main() {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let arguments = env::args_os().skip(1);

    match termnav::cli::run(arguments, &mut stdout, &mut stderr) {
        Ok(code) => process::exit(code),
        Err(error) => {
            eprintln!("termnav: {error}");
            process::exit(1);
        }
    }
}
