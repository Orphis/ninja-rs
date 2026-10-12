//! ninja-rs: a ninja-compatible build executor with REAPI v2 support.

use ninja_core::subprocess::Interrupt;
use ninja_rs::Service;
use ninja_rs::options::{ParseOutcome, parse_args};
use ninja_rs::rc;

fn main() {
    let cmdline: Vec<String> = std::env::args().skip(1).collect();
    // A nested invocation takes the outer one's settings first: they
    // include the options of the .ninjars files it read.
    let parent = ninja_rs::nested::parent();
    let (rc, expanded) = match settings(&cmdline, parent.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ninja: fatal: {e}");
            std::process::exit(2);
        }
    };
    let mut args: Vec<String> = parent.iter().flat_map(|p| p.flags.iter().cloned()).collect();
    let mut explicit = vec![false; args.len()];
    args.extend(expanded.args);
    explicit.extend(expanded.explicit);
    let mut opts = match parse_args(&args) {
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
    opts.argv = std::iter::once("ninja-rs".to_string()).chain(cmdline).collect();
    for g in &mut opts.given {
        g.explicit = explicit.get(g.index).copied().unwrap_or(true);
    }
    opts.rc.read = rc.files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
    opts.rc.workspace_dir = Some(rc.workspace.to_string_lossy().into_owned());

    if let Some(p) = &parent {
        ninja_rs::nested::adopt(&mut opts, p);
    }

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

    // Before any thread starts: what nested invocations get.
    let relay = ninja_rs::nested::export(&mut opts);

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
        service.relay = relay;
        service.run(&opts).await
    });
    // Don't wait for lingering blocking tasks (e.g. pipes held open by
    // background processes).
    runtime.shutdown_background();
    std::process::exit(code);
}

/// The `.ninjars` files and the arguments to parse: their options, then
/// the command line's, `--config`s expanded. A nested invocation reads the
/// files the outer one read, for its own `--config`s only.
fn settings(cmdline: &[String], parent: Option<&ninja_rs::nested::Parent>) -> Result<(rc::RcFiles, rc::Expanded), String> {
    let pre = rc::prescan(cmdline);
    let cwd = std::env::current_dir().map_err(|e| format!("current directory: {e}"))?;
    let build_dir = match &pre.working_dir {
        Some(d) => std::path::absolute(cwd.join(d)).unwrap_or_else(|_| cwd.join(d)),
        None => cwd,
    };
    let files = match parent {
        Some(p) => rc::RcFiles::reread(&p.rc_files, p.rc_workspace.as_deref(), &build_dir)?,
        None => rc::RcFiles::discover(&pre, &build_dir)?,
    };
    let expanded = rc::expand(&files, &rc::commands(pre.tool.as_deref()), cmdline, pre.end, parent.is_none())?;
    if parent.is_none() {
        for w in &files.warnings {
            eprintln!("ninja: warning: {w}");
        }
    }
    if expanded.announce {
        for line in &expanded.announcements {
            if line.starts_with(' ') {
                eprintln!("{line}");
            } else {
                eprintln!("ninja: {line}");
            }
        }
    }
    Ok((files, expanded))
}
