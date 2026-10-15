//! Running a program for its output, for a limited time: the processes it
//! starts are a group (POSIX) or a job (Windows), killed together when the
//! time is up, so that none is left running (`gcc` leaves `cc1`, a script
//! its commands) or holding the output pipes.

use std::io::{self, Read, Write};
use std::process::{Child, Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Runs `cmd` (its standard streams as the caller set them) with `input` on
/// its standard input when that is piped, until it exited and its output
/// ended, for at most `timeout`: then every process it started is killed
/// and the error is `TimedOut`.
pub fn output(mut cmd: Command, input: Option<Vec<u8>>, timeout: Duration) -> io::Result<Output> {
    let deadline = Instant::now() + timeout;
    Tree::prepare(&mut cmd);
    let mut child = cmd.spawn()?;
    let tree = match Tree::attach(&child) {
        Ok(t) => t,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let writer = child.stdin.take().map(|mut stdin| std::thread::spawn(move || stdin.write_all(&input.unwrap_or_default())));
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
    let stdout = read(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = read(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    drop(ended);
    let (mut status, mut pipes) = (None, 0);
    let mut pause = Duration::from_millis(1);
    while status.is_none() || pipes < 2 {
        if status.is_none() {
            status = child.try_wait()?;
        }
        while pipes_done.try_recv().is_ok() {
            pipes += 1;
        }
        if status.is_some() && pipes == 2 {
            break;
        }
        if Instant::now() >= deadline {
            tree.kill();
            let _ = child.kill();
            let _ = child.wait();
            // The readers end with the pipes (unless a process that left the
            // group or job holds them): not waited for.
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("timed out after {}s", timeout.as_secs_f32())));
        }
        // Woken by a pipe that ends, else polling the process.
        if pipes < 2 {
            if pipes_done.recv_timeout(pause).is_ok() {
                pipes += 1;
            }
        } else {
            std::thread::sleep(pause);
        }
        pause = (pause * 2).min(Duration::from_millis(50));
    }
    if let Some(w) = writer {
        let _ = w.join();
    }
    let status = status.expect("exited");
    Ok(Output { status, stdout: stdout.join().unwrap_or_default(), stderr: stderr.join().unwrap_or_default() })
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
    fn prepare(cmd: &mut Command) {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    fn attach(child: &Child) -> io::Result<Tree> {
        Ok(Tree { group: i32::try_from(child.id()).ok() })
    }

    fn kill(&self) {
        if let Some(g) = self.group {
            // SAFETY: plain system call; the group is the command's own, alive
            // while the command or what it started runs.
            unsafe { libc::killpg(g, libc::SIGKILL) };
        }
    }
}

#[cfg(windows)]
impl Tree {
    fn prepare(cmd: &mut Command) {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
    }

    /// Puts the child in a new job (when it can: a process may not be
    /// allowed to join one), then resumes it.
    fn attach(child: &Child) -> io::Result<Tree> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        };
        // SAFETY: a new anonymous job, owned from then on, and the child's
        // valid process handle.
        let job = unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                None
            } else {
                let job = OwnedHandle::from_raw_handle(job);
                // A process asking to leave the job (a server a wrapper starts)
                // may: its creation would fail otherwise.
                let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                    BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION { LimitFlags: JOB_OBJECT_LIMIT_BREAKAWAY_OK, ..Default::default() },
                    ..Default::default()
                };
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                (AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) != 0).then_some(job)
            }
        };
        resume(child.id())?;
        Ok(Tree { job })
    }

    fn kill(&self) {
        use std::os::windows::io::AsRawHandle;
        if let Some(job) = &self.job {
            // SAFETY: the handle is open.
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(job.as_raw_handle(), 1) };
        }
    }
}

/// Resumes the threads of process `pid`, created suspended.
#[cfg(windows)]
fn resume(pid: u32) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    // SAFETY: a snapshot handle, closed below, and an entry of its size.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut entry = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut resumed = 0;
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    if ResumeThread(thread) != u32::MAX {
                        resumed += 1;
                    }
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        if resumed == 0 {
            return Err(io::Error::other("resuming the process: no thread"));
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
impl Tree {
    fn prepare(_cmd: &mut Command) {}

    fn attach(_child: &Child) -> io::Result<Tree> {
        Ok(Tree {})
    }

    fn kill(&self) {}
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
}
