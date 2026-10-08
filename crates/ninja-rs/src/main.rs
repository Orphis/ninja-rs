//! ninja-rs: a ninja-compatible build executor with REAPI v2 support.

use ninja_core::subprocess::Interrupt;
use ninja_rs::Service;
use ninja_rs::options::{ParseOutcome, parse_args};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&args) {
        Ok(ParseOutcome::Run(o)) => o,
        Ok(ParseOutcome::Exit(msg, code)) => {
            if code == 0 {
                print!("{msg}");
            } else {
                eprint!("{msg}");
            }
            std::process::exit(code);
        }
        Err(e) => {
            eprintln!("ninja: fatal: {e}");
            std::process::exit(2);
        }
    };

    if let Some(dir) = &opts.working_dir {
        // The formatting of this string, complete with funny quotes, is so
        // Emacs can properly identify that the cwd has changed for subsequent
        // commands. Don't print it in tools or quiet mode.
        if opts.tool.is_none() && opts.verbosity != ninja_core::status::Verbosity::NoStatusUpdate {
            println!("ninja: Entering directory `{dir}'");
        }
        if let Err(e) = std::env::set_current_dir(dir) {
            eprintln!("ninja: fatal: chdir to '{dir}' - {e}");
            std::process::exit(1);
        }
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ninja: fatal: starting runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async move {
        let interrupt = Interrupt::new();
        interrupt.install_ctrl_c();
        let mut service = Service::new(interrupt);
        service.skip_teardown = true;
        service.run(&opts).await
    });
    // Don't wait for lingering blocking tasks (e.g. pipes held open by
    // background processes).
    runtime.shutdown_background();
    std::process::exit(code);
}
