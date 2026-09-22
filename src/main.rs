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

    let args: Vec<String> = std::env::args().skip(1).collect();
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
