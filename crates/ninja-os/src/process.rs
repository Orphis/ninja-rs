//! Running a program for its output, for a limited time: the processes it
//! starts are a group (POSIX) or a job (Windows), killed together when the
//! time is up, so that none is left running (`gcc` leaves `cc1`, a script
//! its commands) or holding the output pipes. So are they when the run
//! ends otherwise before the program did (an error, a panic), or ninja-rs
//! exits meanwhile (an interrupt).

use std::io::{self, Read, Write};
use std::process::{Child, Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Runs `cmd` (its standard streams as the caller set them) with `input` on
/// its standard input when that is piped, until it exited and its output
/// ended, for at most `timeout`: then every process it started is killed
/// and the error is `TimedOut`.
pub fn output(cmd: Command, input: Option<Vec<u8>>, timeout: Duration) -> io::Result<Output> {
    output_with_flags(cmd, input, timeout, 0)
}

/// `output`, the command created with Windows' process creation flags
/// `creation_flags` (ignored elsewhere): std can't tell those set on `cmd`,
/// which the run replaces (the command starts suspended, to join its job
/// before it runs).
pub fn output_with_flags(mut cmd: Command, input: Option<Vec<u8>>, timeout: Duration, creation_flags: u32) -> io::Result<Output> {
    let deadline = Instant::now() + timeout;
    Tree::prepare(&mut cmd, creation_flags);
    let mut child = cmd.spawn()?;
    let tree = match Tree::attach(&child) {
        Ok(t) => t,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let mut run = Running { tree, child, done: false };
    let writer = run.child.stdin.take().map(|mut stdin| std::thread::spawn(move || stdin.write_all(&input.unwrap_or_default())));
    // Each reader says when its pipe ended.
    let (ended, pipes_done) = mpsc::channel::<()>();
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        let ended = ended.clone();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut out);
            }
            let _ = ended.send(());
            out
        })
    };
    let stdout = read(run.child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = read(run.child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    drop(ended);
    let timed_out = || io::Error::new(io::ErrorKind::TimedOut, format!("timed out after {}s", timeout.as_secs_f32()));
    // Its output ends (when it exits, unless what it started holds the
    // pipes), then it has exited. When the time is up, it's killed with
    // what it started as `run` drops; the readers end with the pipes
    // (unless a process that left the group or job holds them): not
    // waited for.
    for _ in 0..2 {
        pipes_done.recv_timeout(deadline.saturating_duration_since(Instant::now())).map_err(|_| timed_out())?;
    }
    let status = run.wait(deadline)?.ok_or_else(timed_out)?;
    // What it left running, outside the pipes (a server it started), keeps
    // running, as without a group or job.
    run.done = true;
    run.tree.release();
    if let Some(w) = writer {
        let _ = w.join();
    }
    Ok(Output { status, stdout: stdout.join().unwrap_or_default(), stderr: stderr.join().unwrap_or_default() })
}

/// A command running: killed with what it started when dropped before it's
/// `done` (the time is up, an error, a panic).
struct Running {
    tree: Tree,
    child: Child,
    done: bool,
}

impl Running {
    /// Its exit status, once it exited; None when `deadline` comes first.
    fn wait(&mut self, deadline: Instant) -> io::Result<Option<std::process::ExitStatus>> {
        // (Its pipes ended: it's exiting.)
        #[cfg(not(windows))]
        let mut pause = Duration::from_millis(1);
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(Some(status));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawHandle;
                let ms = left.as_millis().min(u128::from(u32::MAX - 1)) as u32;
                // SAFETY: the child's process handle, open while it's owned.
                unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(self.child.as_raw_handle(), ms) };
            }
            #[cfg(not(windows))]
            {
                std::thread::sleep(pause.min(left));
                pause = (pause * 2).min(Duration::from_millis(50));
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if !self.done {
            self.tree.kill();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The processes a command starts: its process group on POSIX, a job on
/// Windows (where it starts suspended, to join the job before it runs).
struct Tree {
    #[cfg(unix)]
    group: Option<i32>,
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
}

#[cfg(unix)]
impl Tree {
    fn prepare(cmd: &mut Command, _creation_flags: u32) {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    fn attach(child: &Child) -> io::Result<Tree> {
        let group = i32::try_from(child.id()).ok();
        if let Some(g) = group {
            groups::add(g);
        }
        Ok(Tree { group })
    }

    fn kill(&self) {
        if let Some(g) = self.group {
            // SAFETY: plain system call; the group is the command's own, alive
            // while the command or what it started runs.
            unsafe { libc::killpg(g, libc::SIGKILL) };
        }
    }

    /// The run is done: what's left in the group outlives ninja-rs.
    fn release(&self) {
        if let Some(g) = self.group {
            groups::remove(g);
        }
    }
}

#[cfg(unix)]
impl Drop for Tree {
    fn drop(&mut self) {
        self.release();
    }
}

/// The process groups of the runs going on (POSIX), killed when ninja-rs
/// exits meanwhile: `exit` unwinds no thread, and the terminal's signals
/// don't reach a group of its own (on Windows, a job's last handle closing
/// kills its processes).
#[cfg(unix)]
mod groups {
    use std::sync::{Mutex, Once, TryLockError};

    static GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

    pub fn add(group: i32) {
        static AT_EXIT: Once = Once::new();
        // SAFETY: registers a function `exit` runs, which takes nothing.
        AT_EXIT.call_once(|| unsafe {
            libc::atexit(kill_all);
        });
        GROUPS.lock().unwrap_or_else(|e| e.into_inner()).push(group);
    }

    pub fn remove(group: i32) {
        GROUPS.lock().unwrap_or_else(|e| e.into_inner()).retain(|&g| g != group);
    }

    extern "C" fn kill_all() {
        // (Other threads still run: while one starting or ending a run holds
        // the lock, for a moment, the groups are left running.)
        let groups = match GROUPS.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        for &g in groups.iter() {
            // SAFETY: plain system call.
            unsafe { libc::killpg(g, libc::SIGKILL) };
        }
    }
}

#[cfg(windows)]
impl Tree {
    fn prepare(cmd: &mut Command, creation_flags: u32) {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(creation_flags | windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
    }

    /// Puts the child in a new job (when it can: a process may not be
    /// allowed to join one), then resumes it.
    fn attach(child: &Child) -> io::Result<Tree> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE};
        // SAFETY: a new anonymous job, owned from then on, and the child's
        // valid process handle.
        let job = unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                None
            } else {
                let job = OwnedHandle::from_raw_handle(job);
                // Its processes die with its last handle (ninja-rs exiting
                // during the run), until the run is done.
                set_limits(&job, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE);
                (AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) != 0).then_some(job)
            }
        };
        // SAFETY: the child's process handle, with all access rights.
        unsafe { resume(child.as_raw_handle()) }?;
        Ok(Tree { job })
    }

    fn kill(&self) {
        use std::os::windows::io::AsRawHandle;
        if let Some(job) = &self.job {
            // SAFETY: the handle is open.
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(job.as_raw_handle(), 1) };
        }
    }

    /// The run is done: what's left in the job outlives its handle.
    fn release(&self) {
        if let Some(job) = &self.job {
            set_limits(job, 0);
        }
    }
}

/// Sets a job's limits: `flags`, and processes may leave it (a server a
/// wrapper starts asks to: its creation would fail otherwise).
#[cfg(windows)]
fn set_limits(job: &std::os::windows::io::OwnedHandle, flags: u32) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION { LimitFlags: JOB_OBJECT_LIMIT_BREAKAWAY_OK | flags, ..Default::default() },
        ..Default::default()
    };
    // SAFETY: the handle is open; the buffer has the class's size.
    unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
}

/// Resumes `process`, created suspended (`CREATE_SUSPENDED`), whole: std
/// keeps the handle of its thread to itself, and finding the thread among
/// the system's (a Toolhelp snapshot) takes a third of a second on a busy
/// machine. With this ntdll export (long stable, if undocumented;
/// PsSuspend and debuggers use it).
///
/// # Safety
///
/// `process` is a valid handle of a process, with the right to suspend
/// and resume it.
#[cfg(windows)]
pub unsafe fn resume(process: std::os::windows::io::RawHandle) -> io::Result<()> {
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
    }
    // SAFETY: as the caller vouches.
    let status = unsafe { NtResumeProcess(process) };
    if status < 0 {
        return Err(io::Error::other(format!("resuming the process: NTSTATUS {status:#x}")));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
impl Tree {
    fn prepare(_cmd: &mut Command, _creation_flags: u32) {}

    fn attach(_child: &Child) -> io::Result<Tree> {
        Ok(Tree {})
    }

    fn kill(&self) {}

    fn release(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Stdio;

    /// A script that starts a long-running process of its own, then says so
    /// (`started`), then waits: (the command, how to run it).
    fn hanging(dir: &Path) -> Command {
        if cfg!(windows) {
            let script = dir.join("hang.cmd");
            std::fs::write(&script, "@start /b ping -n 60 127.0.0.1 >NUL\r\n@echo x>started\r\n@ping -n 60 127.0.0.1 >NUL\r\n").unwrap();
            let mut cmd = Command::new("cmd");
            cmd.args(["/c", &script.to_string_lossy()]);
            cmd
        } else {
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", "sleep 60 & echo $! >grandchild; : >started; wait"]);
            cmd
        }
    }

    #[test]
    fn output_and_input() {
        let dir = test_dirs::TestDir::new("ninja_os_output");
        let mut cmd = Command::new("sort");
        cmd.current_dir(&dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = output(cmd, Some(b"hello\n".to_vec()), Duration::from_secs(60)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }

    /// When the time is up, what the command started is killed too: on
    /// Windows its directory can then be removed (a process running in it
    /// keeps it), on POSIX the process is gone.
    #[test]
    fn timeouts_kill_the_tree() {
        let dir = test_dirs::TestDir::new("ninja_os_timeout");
        let work = dir.join("work");
        // Until the script got to start its process (a busy machine is
        // slow to): a longer limit each time.
        let mut timeout = Duration::from_millis(500);
        loop {
            std::fs::create_dir_all(&work).unwrap();
            let mut cmd = hanging(&work);
            cmd.current_dir(&work).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let e = output(cmd, None, timeout).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::TimedOut, "{e}");
            if work.join("started").exists() {
                break;
            }
            assert!(timeout < Duration::from_secs(60), "the script never started its process");
            timeout *= 2;
        }
        #[cfg(unix)]
        {
            let pid: i32 = std::fs::read_to_string(work.join("grandchild")).unwrap().trim().parse().unwrap();
            // Killed; gone once reaped (by init: its parent was killed too).
            let start = Instant::now();
            // SAFETY: plain system call; signal 0 only checks the process.
            while unsafe { libc::kill(pid, 0) } == 0 && !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z ")) {
                assert!(start.elapsed() < Duration::from_secs(30), "process {pid} still runs");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        #[cfg(windows)]
        test_dirs::remove_all(&work).unwrap_or_else(|e| panic!("{}: {e} (still in use)", work.display()));
    }

    /// What the child of `job_kills_on_close` reports: the job it's in kills
    /// its processes when its last handle closes. (Run by that test.)
    #[cfg(windows)]
    #[test]
    #[allow(clippy::disallowed_macros)] // Its parent reads what it prints.
    fn job_child() {
        use windows_sys::Win32::System::JobObjects::{
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetPriorityClass};
        if std::env::var_os("NINJA_OS_JOB_CHILD").is_none() {
            return;
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: the current job (a null handle); the buffer has the
        // class's size. A pseudo handle for the current process.
        let (ok, priority) = unsafe {
            let ok = QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectExtendedLimitInformation,
                (&raw mut limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            );
            (ok, GetPriorityClass(GetCurrentProcess()))
        };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        println!("kill on close: {}", limits.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0);
        println!("priority: {priority:#x}");
    }

    /// The command runs in a job that kills it when ninja-rs exits (its
    /// handle closes), created with the caller's flags too.
    #[cfg(windows)]
    #[test]
    fn job_kills_on_close() {
        use windows_sys::Win32::System::Threading::BELOW_NORMAL_PRIORITY_CLASS;
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "process::tests::job_child", "--nocapture", "--test-threads=1"])
            .env("NINJA_OS_JOB_CHILD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = output_with_flags(cmd, None, Duration::from_secs(120), BELOW_NORMAL_PRIORITY_CLASS).unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
        assert!(text.contains("kill on close: true"), "{text}");
        assert!(text.contains(&format!("priority: {BELOW_NORMAL_PRIORITY_CLASS:#x}")), "{text}");
    }

    /// A run that ends before the command did (here a panic while it runs)
    /// kills it.
    #[test]
    fn unfinished_runs_kill_the_tree() {
        let dir = test_dirs::TestDir::new("ninja_os_unfinished");
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let mut cmd = hanging(&work);
        cmd.current_dir(&work).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        Tree::prepare(&mut cmd, 0);
        let child = cmd.spawn().unwrap();
        let tree = Tree::attach(&child).unwrap();
        // Until the script started its process.
        let start = Instant::now();
        while !work.join("started").exists() {
            assert!(start.elapsed() < Duration::from_secs(60), "the script never started its process");
            std::thread::sleep(Duration::from_millis(10));
        }
        let run = Running { tree, child, done: false };
        let panicked = std::thread::spawn(move || {
            let _run = run;
            panic!("unwinding");
        })
        .join();
        assert!(panicked.is_err());
        assert_killed(&work);
    }

    /// What the child of `exiting_kills_the_tree` does: exits while a run
    /// of `hanging` in `NINJA_OS_EXITING_CHILD` goes on. (Run by that test.)
    #[test]
    fn exiting_child() {
        let Some(work) = std::env::var_os("NINJA_OS_EXITING_CHILD").map(std::path::PathBuf::from) else { return };
        let mut cmd = hanging(&work);
        cmd.current_dir(&work).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        std::thread::spawn(move || output(cmd, None, Duration::from_secs(120)));
        let start = Instant::now();
        while !work.join("started").exists() {
            assert!(start.elapsed() < Duration::from_secs(60), "the script never started its process");
            std::thread::sleep(Duration::from_millis(10));
        }
        // As ninja-rs does, which unwinds no thread.
        std::process::exit(0);
    }

    /// ninja-rs exiting during a run (interrupted) kills what the command
    /// started.
    #[test]
    fn exiting_kills_the_tree() {
        let dir = test_dirs::TestDir::new("ninja_os_exiting");
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        // (Its output to a file: what it starts on Windows inherits its
        // handles, and would hold pipes.)
        let log = std::fs::File::create(dir.join("child.log")).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "process::tests::exiting_child", "--nocapture", "--test-threads=1"])
            .env("NINJA_OS_EXITING_CHILD", &work)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .status()
            .unwrap();
        let said = std::fs::read_to_string(dir.join("child.log")).unwrap_or_default();
        assert!(status.success() && work.join("started").exists(), "{said}");
        assert_killed(&work);
    }

    /// That what `hanging` started in `work` was killed: on Windows its
    /// directory can be removed (a process running in it keeps it), on POSIX
    /// the process is gone.
    fn assert_killed(work: &Path) {
        #[cfg(unix)]
        {
            let pid: i32 = std::fs::read_to_string(work.join("grandchild")).unwrap().trim().parse().unwrap();
            let start = Instant::now();
            // SAFETY: plain system call; signal 0 only checks the process.
            while unsafe { libc::kill(pid, 0) } == 0 && !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z ")) {
                assert!(start.elapsed() < Duration::from_secs(30), "process {pid} still runs");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        #[cfg(windows)]
        test_dirs::remove_all(work).unwrap_or_else(|e| panic!("{}: {e} (still in use)", work.display()));
    }
}
