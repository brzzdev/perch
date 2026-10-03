use std::env;
use std::ffi::OsString;
use std::process;

fn main() {
    if let Some(result) = perch::app::run_internal_reclamation() {
        if result.is_err() {
            process::exit(1);
        }
        return;
    }

    let _ = ctrlc::set_handler(|| {
        // The exit below unwinds nothing, so a running background fetch would
        // never reach its drop guard. End it here, before leaving.
        perch::app::terminate_background_work();
        let _ = console::Term::stderr().show_cursor();
        process::exit(130);
    });

    // `env::args` panics on a non-UTF-8 argument; refs and paths can be one.
    let args: Vec<String> = match env::args_os().skip(1).map(OsString::into_string).collect() {
        Ok(args) => args,
        Err(arg) => {
            eprintln!("error: argument is not valid UTF-8: {}", arg.display());
            process::exit(1);
        }
    };
    let result = perch::run(&args);

    if let Err(e) = result {
        if e.is_interrupt() {
            let _ = console::Term::stderr().show_cursor();
            process::exit(130);
        }
        eprintln!("error: {e}");
        process::exit(1);
    }
}
