//! ninja-rs: a ninja-compatible build executor with REAPI v2 support.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ninja_core::status::{print_err, print_out};
use ninja_core::subprocess::Interrupt;
use ninja_rs::Service;
use ninja_rs::bes::Bes;
use ninja_rs::options::{Out, ParseOutcome, error_text, parse_invocation};
use ninja_rs::rc;

fn main() {
    let cmdline: Vec<String> = std::env::args().skip(1).collect();
    // `--version`, `--help`, the listings: from the command line alone, as
    // ninja answers them before anything else.
    if let Some((text, code, out)) = ninja_rs::options::answer(&cmdline) {
        print(&text, out);
        std::process::exit(code);
    }
    // A nested invocation takes the outer one's settings first: they
    // include the options of the .ninjars files it read.
    let parent = ninja_rs::nested::parent();
    // Errors exit with 1, as ninja's `Fatal`.
    let (rc, expanded) = match settings(&cmdline, parent.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            print_err(&format!("ninja: fatal: {e}\n"));
            std::process::exit(1);
        }
    };
    let mut args: Vec<String> = parent.iter().flat_map(|p| p.flags.iter().cloned()).collect();
    let mut explicit = vec![false; args.len()];
    args.extend(expanded.args);
    explicit.extend(expanded.explicit);
    let mut opts = match parse_invocation(&args, &explicit) {
        Ok(ParseOutcome::Run(o)) => o,
        Ok(ParseOutcome::Exit(text, code, out)) => {
            print(&text, out);
            std::process::exit(code);
        }
        Err(e) => {
            print_err(&error_text(&e));
            std::process::exit(1);
        }
    };
    for w in &opts.warnings {
        print_err(&format!("ninja: warning: {w}\n"));
    }
    opts.argv = std::iter::once("ninja-rs".to_string()).chain(cmdline).collect();
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
            print_out(&format!("ninja: Entering directory `{dir}'\n"));
        }
        if let Err(e) = std::env::set_current_dir(dir) {
            print_err(&format!("ninja: fatal: chdir to '{dir}' - {}\n", strerror(&e)));
            std::process::exit(1);
        }
    }

    // Tools writing to a pipe closed by its reader end as ninja's do (not
    // `-t exec`, a build, nor `-t daemon`, which talks over sockets).
    if opts.tool.as_deref().is_some_and(|t| !matches!(t, "exec" | "daemon")) {
        ninja_core::status::die_on_broken_pipe();
    }

    // Before any thread starts: what nested invocations get.
    let (relay, secret_flags) = ninja_rs::nested::export(&mut opts);
    let relay_address = relay.as_ref().map(|r| r.address.clone());

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            print_err(&format!("ninja: fatal: starting runtime: {e}\n"));
            std::process::exit(1);
        }
    };
    // SIGINT, SIGTERM and SIGHUP (Ctrl-C and Ctrl-Break) interrupt a build,
    // which stops its commands, ends its build events and returns; a second
    // one kills the commands.
    let interrupt = Interrupt::new();
    {
        let _runtime = runtime.enter();
        interrupt.install_handlers();
    }
    let mut service = Service::new(interrupt.clone());
    service.skip_teardown = true;
    service.relay = relay;
    let events = service.events.clone();
    // The invocation runs on a thread of its own (with the main thread's
    // stack), so that an interrupt ends it even while it doesn't yield
    // (loading the manifest, a tool).
    let (done, finished) = tokio::sync::oneshot::channel();
    let handle = runtime.handle().clone();
    let invocation = std::thread::Builder::new().stack_size(8 << 20).spawn(move || {
        let _ = done.send(handle.block_on(service.run(&opts)));
    });
    let code = runtime.block_on(async {
        tokio::select! {
            code = finished => code.ok(),
            // Outside a build's loop, one ends the invocation, as ninja dies
            // of the signal there: once its build events end.
            signal = interrupt.unhandled() => {
                end_events(&events, &interrupt, 128 + signal).await;
                Some(128 + signal)
            }
            // A third one exits at once, whatever ninja-rs waits for.
            _ = interrupt.wait_beyond(2) => Some(ninja_rs::EXIT_INTERRUPTED),
        }
    });
    // None: the invocation panicked (or couldn't start).
    let code = code.unwrap_or_else(|| match invocation.map(|t| t.join()) {
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Ok(Ok(())) => ninja_rs::EXIT_FAILURE,
        Err(e) => {
            print_err(&format!("ninja: fatal: starting a thread: {e}\n"));
            ninja_rs::EXIT_FAILURE
        }
    });
    // Where nested invocations sent their build events: ending them removes
    // it, but an interrupt may have cut that short.
    if let Some(a) = &relay_address {
        ninja_rs::nested::Relay::remove(a);
    }
    // `exit` runs no destructors: the runtime isn't shut down (the
    // invocation's thread may still use it), nor waited for (pipes held
    // open by background processes).
    drop(secret_flags);
    std::process::exit(code);
}

/// The system's message for `e`, as `strerror` gives it to ninja: without
/// Rust's ` (os error N)`.
fn strerror(e: &std::io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(code) => text.strip_suffix(&format!(" (os error {code})")).map_or_else(|| text.clone(), str::to_string),
        None => text,
    }
}

/// Prints an answer of the command line.
fn print(text: &str, out: Out) {
    match out {
        Out::Stdout => print_out(text),
        Out::Stderr => print_err(text),
    }
}

/// After an interrupt outside a build's loop (loading the manifest,
/// connecting, waiting for another build), the invocation's build events
/// end, as they do when a build is interrupted: within 2 s, or until another
/// interrupt.
async fn end_events(events: &Mutex<Option<Arc<Bes>>>, interrupt: &Interrupt, code: i32) {
    let Some(bes) = events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() else { return };
    let seen = interrupt.count();
    tokio::select! {
        _ = bes.finish(code, true) => {}
        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        _ = interrupt.wait_beyond(seen) => {}
    }
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
            print_err(&format!("ninja: warning: {w}\n"));
        }
        for n in &expanded.notes {
            print_err(&format!("ninja: note: {n}\n"));
        }
    }
    if expanded.announce {
        for line in &expanded.announcements {
            if line.starts_with(' ') {
                print_err(&format!("{line}\n"));
            } else {
                print_err(&format!("ninja: {line}\n"));
            }
        }
    }
    Ok((files, expanded))
}
