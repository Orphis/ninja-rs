//! `.ninjars` files: options read before the command line, with the format
//! and features of Bazel's `.bazelrc`.
//!
//! A line is `COMMAND[:CONFIG] OPTIONS...`, `import FILE` or
//! `try-import FILE` (which skips a missing file). `#` starts a comment, a
//! line ending with `\` goes on with the next one, and options are split
//! like shell words (quotes, backslashes). `%workspace%` is the workspace
//! directory, in imports and options.
//!
//! The commands: `common` and `always` lines apply to every invocation,
//! `build` lines to builds and tools, `exec` lines to `-t exec` (which
//! takes the `build` ones too). `common` skips the options ninja-rs doesn't
//! have, and with a note those whose values it can't take, so that one
//! file can serve Bazel and ninja-rs; so do all the lines of Bazel's own
//! files (named `*bazelrc`, and those they import, whatever their names:
//! `.bazelrc.user`, `tools/bazel.rc`). Those lines are read as Bazel reads
//! them: short options are Bazel's (`-c opt`), and a separate value is the
//! option's whatever it starts with (`--copt -fPIC`). Bazel's other
//! commands (`test`, `query`, `startup`...) are skipped. Lines with a
//! config apply when `--config=CONFIG` is given, where it is given.
//!
//! The files, read in this order, later options winning:
//! 1. the system's: `/etc/ninjars` (`%ProgramData%\ninjars` on Windows);
//! 2. the workspace's: the first `.ninjars` found from the build
//!    directory up (short of the home directory), whose directory is the
//!    workspace;
//! 3. the user's: `~/.ninjars`;
//! 4. those given with `--ninjars=FILE`, in order (`/dev/null` ends them);
//!
//! then the command line. In each, `common` lines go first, then `build`
//! lines, then `exec` ones. `--nosystem_rc`, `--noworkspace_rc`,
//! `--nohome_rc` and `--ignore_all_rc_files` leave files out. The system's
//! and the workspace's files are skipped, with a warning, when someone
//! other than the user and the administrators could have written them, or
//! (the workspace's) put them in the directories from the build directory
//! up.

use std::path::{Path, PathBuf};

use crate::options::{self, RcOptionError};

pub const FILE_NAME: &str = ".ninjars";

/// Bazel's commands that ninja-rs doesn't have: their lines are skipped.
const BAZEL_COMMANDS: &[&str] = &[
    "analyze-profile",
    "aquery",
    "canonicalize-flags",
    "clean",
    "coverage",
    "cquery",
    "dump",
    "fetch",
    "help",
    "info",
    "license",
    "mobile-install",
    "mod",
    "print_action",
    "query",
    "run",
    "shutdown",
    "startup",
    "sync",
    "test",
    "vendor",
    "version",
];

/// The commands whose lines apply to an invocation, least specific first:
/// within a level, lines go in the order they were read.
pub fn commands(tool: Option<&str>) -> Vec<&'static [&'static str]> {
    let mut levels: Vec<&'static [&'static str]> = vec![&["common", "always"], &["build"]];
    if tool == Some("exec") {
        levels.push(&["exec"]);
    }
    levels
}

/// What the command line says before any file is read.
#[derive(Clone, Debug, PartialEq)]
pub struct Prescan {
    pub working_dir: Option<String>,
    pub tool: Option<String>,
    /// Where the options end: the arguments of `-t TOOL`, or after `--`.
    pub end: usize,
    pub system: bool,
    pub workspace: bool,
    pub home: bool,
    pub ignore_all: bool,
    /// `--ninjars=FILE`.
    pub files: Vec<String>,
}

/// Finds `-C`, `-t` and the options choosing the files in the command
/// line, getopt style like `options::parse_args`.
pub fn prescan(args: &[String]) -> Prescan {
    let mut p = Prescan { working_dir: None, tool: None, end: args.len(), system: true, workspace: true, home: true, ignore_all: false, files: Vec::new() };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" {
            p.end = i - 1;
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            let flag = |base: &str| -> Option<bool> {
                if name == base {
                    Some(inline.is_none_or(|v| options::parse_bool(name, v).unwrap_or(true)))
                } else {
                    (name.strip_prefix("no") == Some(base)).then_some(false)
                }
            };
            if name == "ninjars" {
                match inline {
                    Some(v) => p.files.push(v.to_string()),
                    None => {
                        if let Some(v) = args.get(i) {
                            p.files.push(v.clone());
                            i += 1;
                        }
                    }
                }
            } else if let Some(v) = flag("system_rc") {
                p.system = v;
            } else if let Some(v) = flag("workspace_rc") {
                p.workspace = v;
            } else if let Some(v) = flag("home_rc") {
                p.home = v;
            } else if let Some(v) = flag("ignore_all_rc_files") {
                p.ignore_all = v;
            }
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            let chars: Vec<char> = a[1..].chars().collect();
            for (ci, &c) in chars.iter().enumerate() {
                if !matches!(c, 'd' | 'f' | 'j' | 'k' | 'l' | 't' | 'w' | 'C') {
                    continue;
                }
                let v = if ci + 1 < chars.len() {
                    chars[ci + 1..].iter().collect()
                } else {
                    i += 1;
                    args.get(i - 1).cloned().unwrap_or_default()
                };
                match c {
                    'C' => p.working_dir = Some(v),
                    't' => {
                        p.tool = Some(v);
                        p.end = i;
                        return p;
                    }
                    _ => {}
                }
                break;
            }
        }
    }
    p
}

/// A line of a file.
#[derive(Clone, Debug)]
struct Line {
    /// `file:line`, for messages.
    origin: String,
    file: PathBuf,
    /// Of Bazel's files (see `is_bazel_file`): shared with Bazel.
    bazel: bool,
    command: String,
    config: Option<String>,
    args: Vec<String>,
}

/// Whether `file` is one of Bazel's, read as Bazel reads it: named
/// `*bazelrc`, or imported by one (`bazel_import`).
fn is_bazel_file(file: &Path, bazel_import: bool) -> bool {
    bazel_import || file.file_name().is_some_and(|f| f.to_string_lossy().to_ascii_lowercase().ends_with("bazelrc"))
}

/// The lines of the files read.
#[derive(Clone, Debug, Default)]
pub struct RcFiles {
    /// The files read, in order (not their imports).
    pub files: Vec<PathBuf>,
    /// The workspace directory: where the workspace's file is, else the
    /// build directory.
    pub workspace: PathBuf,
    lines: Vec<Line>,
    /// Lines for commands nobody has.
    pub warnings: Vec<String>,
}

/// The system's file, unless someone other than the user and the
/// administrators could have written it: anyone can create files in
/// `%ProgramData%` (but not replace those of others). One made after this
/// check isn't read.
fn system_file(warnings: &mut Vec<String>) -> Option<PathBuf> {
    let f =
        if cfg!(windows) { PathBuf::from(std::env::var_os("ProgramData").filter(|d| !d.is_empty())?).join("ninjars") } else { PathBuf::from("/etc/ninjars") };
    if !f.is_file() {
        return None;
    }
    if !trusted_file(&f) {
        warnings.push(format!("{} skipped: other users can change it{}", f.display(), CHANGE_HINT));
        return None;
    }
    Some(f)
}

/// The workspace's file: the first `.ninjars` from `dir` up to the
/// repository's root, short of the home directory (whose file is the
/// user's) and of a file system's root (`/`, `D:\`). A file someone else
/// could have put there is skipped, with a warning: one in or above a
/// directory others can write to (`/tmp`), or one others can change.
pub fn workspace_file(dir: &Path, warnings: &mut Vec<String>) -> Option<PathBuf> {
    workspace_file_below(dir, options::home_dir().as_deref(), warnings)
}

/// `workspace_file`, with `home` the home directory.
fn workspace_file_below(dir: &Path, home: Option<&Path>, warnings: &mut Vec<String>) -> Option<PathBuf> {
    // Also spelt otherwise: another case on Windows, through a link.
    let home_real = home.and_then(|h| std::fs::canonicalize(h).ok());
    let is_home = |d: &Path| home == Some(d) || home_real.as_ref().is_some_and(|h| std::fs::canonicalize(d).is_ok_and(|r| r == *h));
    // Past a directory others can write to, files are only looked for to
    // say why they're skipped.
    let mut shared = None;
    for d in dir.ancestors() {
        if d.parent().is_none() || is_home(d) {
            break;
        }
        shared = shared.or_else(|| writable_by_others(d).then_some(d));
        let f = d.join(FILE_NAME);
        if f.is_file() {
            match shared {
                Some(s) => {
                    warnings.push(format!("{} skipped: other users can write to {}{}", f.display(), s.display(), SHARED_HINT));
                    return None;
                }
                None if trusted_file(&f) => return Some(f),
                None => warnings.push(format!("{} skipped: other users can change it{}", f.display(), CHANGE_HINT)),
            }
        }
        // A repository's root ends the search.
        if ninja_daemon::watch::VCS_DIRS.iter().any(|m| d.join(m).exists()) {
            break;
        }
    }
    None
}

/// What to do about a workspace file skipped in a directory others can
/// write to.
#[cfg(unix)]
const SHARED_HINT: &str = ": move the workspace out of it, or give the file with --ninjars=FILE";
#[cfg(windows)]
const SHARED_HINT: &str = " (below a drive's root, Authenticated Users may modify directories): \
     keep the workspace under your profile, take their write access away (icacls), \
     or give the file with --ninjars=FILE";

/// What to do about a settings file others can change.
#[cfg(unix)]
const CHANGE_HINT: &str = ": make it yours and not writable by others (chmod go-w), or give it with --ninjars=FILE";
#[cfg(windows)]
const CHANGE_HINT: &str = ": take others' write access away (icacls), or give it with --ninjars=FILE";

/// A directory shared by every user, as `/tmp` is: writable by all, with
/// the sticky bit. (Writable by all alone is how WSL shows Windows drives,
/// on a single-user host.)
#[cfg(unix)]
fn writable_by_others(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(dir).is_ok_and(|m| m.permissions().mode() & 0o1002 == 0o1002)
}

/// A file only the user and root can change: owned by either, not writable
/// by its group or others. Unless the permissions are those of every file
/// in a directory writable by all without the sticky bit, as WSL shows
/// Windows drives (which keep no Unix permissions): anyone who can write to
/// the file could replace it there anyway.
#[cfg(unix)]
fn trusted_file(file: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let Ok(m) = std::fs::metadata(file) else { return false };
    let open_dir = || file.parent().and_then(|d| std::fs::metadata(d).ok()).is_some_and(|d| d.permissions().mode() & 0o1002 == 0o0002);
    (m.uid() == uid || m.uid() == 0) && (m.mode() & 0o022 == 0 || open_dir())
}

/// A directory in which someone other than the user and the administrators
/// can add or replace files: its owner, or its permissions say so (those
/// below a drive's root usually let Authenticated Users modify them). One
/// whose permissions can't be read counts; one that doesn't exist holds
/// nothing.
#[cfg(windows)]
fn writable_by_others(dir: &Path) -> bool {
    sys::private(dir).map_or_else(|e| e.kind() != std::io::ErrorKind::NotFound, |private| !private)
}

/// A file only the user and the administrators can change: owned by one of
/// them, with no one else allowed to write to it.
#[cfg(windows)]
fn trusted_file(file: &Path) -> bool {
    sys::private(file).unwrap_or(false)
}

/// Windows: who owns a file or directory and who may change it, from its
/// security descriptor.
#[cfg(windows)]
mod sys {
    use std::io;
    use std::path::Path;
    use std::sync::OnceLock;

    /// Rights that change a file, or what a directory holds: write data
    /// (add a file), append data (add a subdirectory), delete a child,
    /// delete, change the permissions or the owner, and the generic rights
    /// that include them.
    const WRITE: u32 = 0x2 | 0x4 | 0x40 | 0x1_0000 | 0x4_0000 | 0x8_0000 | 0x1000_0000 | 0x4000_0000;

    /// SYSTEM, Administrators and TrustedInstaller, who can change any
    /// file anyway.
    const ADMINISTRATORS: &[&str] = &["S-1-5-18", "S-1-5-32-544", "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"];
    /// CREATOR OWNER and OWNER RIGHTS: the owner.
    const OWNER: &[&str] = &["S-1-3-0", "S-1-3-4"];

    /// Who owns a file and who may change it.
    #[derive(Debug)]
    pub struct Security {
        pub owner: String,
        /// The SIDs the DACL lets change it: everyone without a DACL, or for
        /// an entry whose SID isn't read.
        pub writers: Vec<String>,
    }

    impl Security {
        /// Whether only `user` and the administrators can change it.
        pub fn private_to(&self, user: &str) -> bool {
            let trusted = |sid: &str| sid == user || ADMINISTRATORS.contains(&sid);
            trusted(&self.owner) && self.writers.iter().all(|w| trusted(w) || OWNER.contains(&w.as_str()))
        }
    }

    /// Whether only this user and the administrators can change `path`:
    /// the entry itself (a link someone else made) and what it links to.
    pub fn private(path: &Path) -> io::Result<bool> {
        let Some(user) = current_user() else { return Ok(false) };
        Ok(security(path, false)?.private_to(user) && security(path, true)?.private_to(user))
    }

    /// The security of `path`, or of what it links to (`follow`).
    fn security(path: &Path, follow: bool) -> io::Result<Security> {
        let (owner, writers) = ninja_os::windows::file_security(path, follow, |rights| rights & WRITE != 0)?;
        Ok(Security { owner, writers })
    }

    /// The SID of the user this process runs as.
    fn current_user() -> Option<&'static str> {
        static USER: OnceLock<Option<String>> = OnceLock::new();
        USER.get_or_init(|| ninja_os::windows::current_user().ok()).as_deref()
    }
}

fn is_dev_null(f: &str) -> bool {
    f == "/dev/null" || (cfg!(windows) && f.eq_ignore_ascii_case("NUL"))
}

impl RcFiles {
    /// Reads the files the command line (`prescan`) selects, for a build in
    /// `build_dir` (absolute).
    pub fn discover(p: &Prescan, build_dir: &Path) -> Result<RcFiles, String> {
        let mut skipped = Vec::new();
        let found = workspace_file(build_dir, &mut skipped);
        let workspace = found.as_ref().and_then(|f| f.parent()).unwrap_or(build_dir).to_path_buf();
        let mut rc = RcFiles { workspace, ..Default::default() };
        if p.ignore_all {
            return Ok(rc);
        }
        let mut files: Vec<(PathBuf, bool)> = Vec::new();
        if p.system
            && let Some(f) = system_file(&mut rc.warnings)
        {
            files.push((f, false));
        }
        if p.workspace {
            rc.warnings.append(&mut skipped);
            files.extend(found.map(|f| (f, false)));
        }
        if p.home
            && let Some(h) = options::home_dir()
        {
            files.push((h.join(FILE_NAME), false));
        }
        for f in &p.files {
            if is_dev_null(f) {
                break;
            }
            files.push((std::path::absolute(f).map_err(|e| format!("--ninjars={f}: {e}"))?, true));
        }
        for (f, required) in files {
            // A file found twice (the workspace's is the user's) is read once.
            let key = std::fs::canonicalize(&f).unwrap_or_else(|_| f.clone());
            if rc.files.iter().any(|r| std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()) == key) {
                continue;
            }
            if !f.is_file() {
                if required {
                    return Err(format!("--ninjars={}: no such file", f.display()));
                }
                continue;
            }
            rc.read(&f, &mut Vec::new(), false)?;
            rc.files.push(f);
        }
        Ok(rc)
    }

    /// Reads `files`, as an outer invocation read them (for the configs a
    /// nested invocation is given).
    pub fn reread(files: &[String], workspace: Option<&str>, build_dir: &Path) -> Result<RcFiles, String> {
        let mut rc = RcFiles { workspace: workspace.map(PathBuf::from).unwrap_or_else(|| build_dir.to_path_buf()), ..Default::default() };
        for f in files {
            let f = PathBuf::from(f);
            if f.is_file() {
                rc.read(&f, &mut Vec::new(), false)?;
                rc.files.push(f);
            }
        }
        Ok(rc)
    }

    /// Reads one file and what it imports. `stack`: the files importing it;
    /// `bazel_import`: one of them is Bazel's.
    fn read(&mut self, file: &Path, stack: &mut Vec<PathBuf>, bazel_import: bool) -> Result<(), String> {
        let bazel = is_bazel_file(file, bazel_import);
        let key = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        if stack.contains(&key) {
            let chain: Vec<String> = stack.iter().chain([&key]).map(|f| f.display().to_string()).collect();
            return Err(format!("import loop: {}", chain.join(" -> ")));
        }
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        stack.push(key);
        let workspace = self.workspace.to_string_lossy().into_owned();
        for (number, logical) in logical_lines(&text) {
            let origin = format!("{}:{number}", file.display());
            let tokens = tokenize(&logical).map_err(|e| format!("{origin}: {e}"))?;
            let tokens: Vec<String> = tokens.into_iter().map(|t| t.replace("%workspace%", &workspace)).collect();
            let Some((head, args)) = tokens.split_first() else { continue };
            match head.as_str() {
                "import" | "try-import" => {
                    let [path] = args else {
                        return Err(format!("{origin}: {head} takes one file"));
                    };
                    let path = file.parent().unwrap_or(Path::new(".")).join(path);
                    if head == "try-import" && !path.is_file() {
                        continue;
                    }
                    self.read(&path, stack, bazel).map_err(|e| format!("{origin}: {e}"))?;
                }
                _ => {
                    let (command, config) = match head.split_once(':') {
                        Some((c, n)) if !n.is_empty() => (c, Some(n.to_string())),
                        Some(_) => return Err(format!("{origin}: '{head}' names no config")),
                        None => (head.as_str(), None),
                    };
                    let ours = commands(Some("exec")).concat().contains(&command);
                    if !ours {
                        if !BAZEL_COMMANDS.contains(&command) {
                            self.warnings.push(format!("{origin}: unknown command '{command}', line skipped"));
                        }
                        // A config only Bazel uses is still defined.
                        self.lines.push(Line { origin, file: file.to_path_buf(), bazel, command: command.to_string(), config, args: Vec::new() });
                        continue;
                    }
                    self.lines.push(Line { origin, file: file.to_path_buf(), bazel, command: command.to_string(), config, args: args.to_vec() });
                }
            }
        }
        stack.pop();
        Ok(())
    }

    fn defines(&self, config: &str) -> bool {
        self.lines.iter().any(|l| l.config.as_deref() == Some(config))
    }
}

/// The lines of a file, those ending with `\` joined to the next, with the
/// number of their first line.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (n, raw) in text.lines().enumerate() {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let (number, mut line) = current.take().unwrap_or((n + 1, String::new()));
        match raw.strip_suffix('\\') {
            Some(start) => {
                line.push_str(start);
                current = Some((number, line));
            }
            None => {
                line.push_str(raw);
                out.push((number, line));
            }
        }
    }
    out.extend(current);
    out
}

/// Splits a line in words, like a shell: `'...'` as written, `"..."` with
/// `\"` and `\\`, `\` outside quotes escaping the next character; `#` at a
/// word's start comments out the rest of the line.
fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                out.extend(word.take());
            }
            '#' if word.is_none() => break,
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => w.push(c),
                        None => return Err("unterminated '".into()),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => w.push(c),
                            Some(c) => {
                                w.push('\\');
                                w.push(c);
                            }
                            None => return Err("unterminated \"".into()),
                        },
                        Some(c) => w.push(c),
                        None => return Err("unterminated \"".into()),
                    }
                }
            }
            '\\' => {
                let w = word.get_or_insert_with(String::new);
                if let Some(c) = chars.next() {
                    w.push(c);
                }
            }
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    out.extend(word);
    Ok(out)
}

/// The arguments to parse: the files' options, then the command line's,
/// each `--config` followed by its options.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Expanded {
    pub args: Vec<String>,
    /// Whether each argument was typed on the command line.
    pub explicit: Vec<bool>,
    /// What `--announce_rc` prints.
    pub announcements: Vec<String>,
    /// Options skipped because ninja-rs can't take their values: printed.
    pub notes: Vec<String>,
    /// Options of lines shared with Bazel skipped as Bazel's, with where:
    /// `--announce_rc` prints them (a misspelt option of ninja-rs's is one).
    pub skipped: Vec<String>,
    /// `--announce_rc` was given (last value).
    pub announce: bool,
}

impl Expanded {
    fn push(&mut self, arg: &str, explicit: bool) {
        self.args.push(arg.to_string());
        self.explicit.push(explicit);
    }
}

/// The OS name of `--enable_platform_specific_config`, as Bazel names it.
pub fn platform_config() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "freebsd") {
        "freebsd"
    } else if cfg!(target_os = "openbsd") {
        "openbsd"
    } else {
        "unknown"
    }
}

/// The last value of the boolean option `name` in `args`.
fn last_bool(args: &[String], name: &str) -> Option<bool> {
    let mut value = None;
    for a in args {
        let Some(long) = a.strip_prefix("--") else { continue };
        let (n, inline) = match long.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (long, None),
        };
        if n == name {
            value = Some(inline.is_none_or(|v| options::parse_bool(name, v).unwrap_or(true)));
        } else if n.strip_prefix("no") == Some(name) {
            value = Some(false);
        }
    }
    value
}

/// `--config NAME` or `--config=NAME` at `args[i]`: the name, and how many
/// arguments it takes.
fn config_at(args: &[String], i: usize) -> Option<(String, usize)> {
    match args[i].strip_prefix("--config") {
        Some("") => args.get(i + 1).map(|n| (n.clone(), 2)),
        Some(rest) => rest.strip_prefix('=').map(|n| (n.to_string(), 1)),
        None => None,
    }
}

/// Expands the files' options for an invocation with `levels` (see
/// `commands`) and its command line `cmdline`, whose options end at `end`
/// (see `prescan`). A nested invocation (`outermost` false) got the files'
/// options from the outer one: only its own `--config`s are expanded.
pub fn expand(rc: &RcFiles, levels: &[&[&str]], cmdline: &[String], end: usize, outermost: bool) -> Result<Expanded, String> {
    let mut out = Expanded::default();
    let mut stack = Vec::new();
    if outermost {
        for level in levels {
            let mut announced: Option<(PathBuf, String)> = None;
            for line in rc.lines.iter().filter(|l| l.config.is_none() && level.contains(&l.command.as_str())) {
                if announced.as_ref() != Some(&(line.file.clone(), line.command.clone())) {
                    announced = Some((line.file.clone(), line.command.clone()));
                    out.announcements.push(format!("reading rc options for '{}' from {}:", levels.last().map_or("build", |l| l[0]), line.file.display()));
                }
                let first = out.args.len();
                expand_line(rc, levels, line, &mut stack, &mut out)?;
                out.announcements.push(format!("  '{}' options: {}", line.command, options::redact_args(&out.args[first..]).join(" ")));
            }
        }
    }
    let files_end = out.args.len();
    let mut i = 0;
    while i < cmdline.len() {
        if i < end
            && let Some((name, n)) = config_at(cmdline, i)
        {
            for a in &cmdline[i..i + n] {
                out.push(a, true);
            }
            expand_config(rc, levels, &name, &mut stack, &mut out)?;
            i += n;
            continue;
        }
        out.push(&cmdline[i], true);
        i += 1;
    }
    let options_end = out.args.len() - (cmdline.len() - end.min(cmdline.len()));
    let options = &out.args[..options_end];
    if outermost && last_bool(options, "enable_platform_specific_config") == Some(true) && rc.defines(platform_config()) {
        // Between the files' options and the command line's, as Bazel does.
        let mut platform = Expanded::default();
        expand_config(rc, levels, platform_config(), &mut stack, &mut platform)?;
        out.announcements.extend(platform.announcements);
        out.notes.extend(platform.notes);
        out.skipped.extend(platform.skipped);
        out.args.splice(files_end..files_end, platform.args);
        out.explicit.splice(files_end..files_end, platform.explicit);
    }
    let options_end = out.args.len() - (cmdline.len() - end.min(cmdline.len()));
    out.announce = last_bool(&out.args[..options_end], "announce_rc") == Some(true);
    for s in &out.skipped {
        out.announcements.push(format!("skipped, not an option of ninja-rs (Bazel's?): {s}"));
    }
    Ok(out)
}

/// The options of `config`'s lines, least specific command first.
fn expand_config(rc: &RcFiles, levels: &[&[&str]], config: &str, stack: &mut Vec<String>, out: &mut Expanded) -> Result<(), String> {
    if stack.iter().any(|c| c == config) {
        return Err(format!("config expansion has a cycle: {} -> {config}", stack.join(" -> ")));
    }
    if !rc.defines(config) {
        return Err(format!("config value '{config}' is not defined in any {FILE_NAME} file"));
    }
    stack.push(config.to_string());
    for level in levels {
        for line in rc.lines.iter().filter(|l| l.config.as_deref() == Some(config) && level.contains(&l.command.as_str())) {
            let first = out.args.len();
            expand_line(rc, levels, line, stack, out)?;
            out.announcements.push(format!(
                "found applicable config definition {}:{config} in file {}: {}",
                line.command,
                line.file.display(),
                options::redact_args(&out.args[first..]).join(" ")
            ));
        }
    }
    stack.pop();
    Ok(())
}

/// A line's options, checked, with its `--config`s expanded in place.
fn expand_line(rc: &RcFiles, levels: &[&[&str]], line: &Line, stack: &mut Vec<String>, out: &mut Expanded) -> Result<(), String> {
    let args = &line.args;
    // `common` lines, and Bazel's own files, are shared with Bazel: its
    // options are skipped, and values only Bazel takes.
    let shared = line.command == "common" || line.bazel;
    let mut i = 0;
    while i < args.len() {
        if let Some((name, n)) = config_at(args, i) {
            for a in &args[i..i + n] {
                out.push(a, false);
            }
            expand_config(rc, levels, &name, stack, out).map_err(|e| format!("{}: {e}", line.origin))?;
            i += n;
            continue;
        }
        let a = &args[i];
        if !a.starts_with('-') || a == "-" || a == "--" {
            return Err(format!("{}: '{a}' isn't an option", line.origin));
        }
        let n = if shared { bazel_option_len(args, i) } else { option_len(args, i) };
        let group = &args[i..i + n];
        i += n;
        // Short options there are Bazel's (`-c opt`, `-j 200`), not ninja's.
        if shared && !a.starts_with("--") {
            continue;
        }
        match options::check_rc_option(group) {
            Ok(()) => {
                for a in group {
                    out.push(a, false);
                }
            }
            Err(RcOptionError::Unknown(_)) if shared => {
                let skipped = format!("{}: '{}'", line.origin, options::redact_args(group).join(" "));
                if !out.skipped.contains(&skipped) {
                    out.skipped.push(skipped);
                }
            }
            Err(RcOptionError::Invalid(e)) if shared => {
                let note = format!("{}: '{}' skipped: {e}", line.origin, options::redact_args(group).join(" "));
                if !out.notes.contains(&note) {
                    out.notes.push(note);
                }
            }
            Err(RcOptionError::Unknown(e) | RcOptionError::Invalid(e)) => return Err(format!("{}: {e}", line.origin)),
        }
    }
    Ok(())
}

/// How many words the option at `args[i]` takes: with no targets in
/// files, the next word is its value, unless it is an option or the option
/// a boolean.
fn option_len(args: &[String], i: usize) -> usize {
    let a = &args[i];
    let takes_next = !a.contains('=') && args.get(i + 1).is_some_and(|n| !n.starts_with('-'));
    if takes_next && !(a.starts_with("--") && options::is_bool_option(&a[2..])) { 2 } else { 1 }
}

/// The same on a line shared with Bazel, read as Bazel reads it: an
/// option's separate value is the next word, whatever it starts with
/// (`--copt -fPIC`, `--linkopt -v`). ninja-rs knows which of its options
/// take one. Of Bazel's it doesn't know the booleans: the next word is a
/// value when it isn't an option, and one starting with a single `-` is
/// skipped as a short option is (`--keep_going -c opt`, `--copt -fPIC`), so
/// that only ninja-rs's long options apply, as Bazel would read them.
fn bazel_option_len(args: &[String], i: usize) -> usize {
    let a = &args[i];
    let Some(next) = args.get(i + 1) else { return 1 };
    let takes_next = match a.strip_prefix("--") {
        Some(long) if long.contains('=') => false,
        Some(name) => match options::check_rc_option(std::slice::from_ref(a)) {
            // A boolean, or an option without a value.
            Ok(()) => false,
            Err(RcOptionError::Unknown(_)) => !next.starts_with('-'),
            // Missing its value, or only for the command line (skipped with
            // a note): a boolean (--nosystem_rc) takes none.
            Err(RcOptionError::Invalid(_)) => !options::is_bool_option(name),
        },
        // Bazel's short options: `-c opt`, `-k`.
        None => !next.starts_with('-'),
    };
    if takes_next { 2 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Writes files in a fresh directory: `(relative path, content)`.
    fn tree(name: &str, files: &[(&str, &str)]) -> test_dirs::TestDir {
        let dir = test_dirs::TestDir::new(&format!("ninja-rs-rc-{name}"));
        for (path, content) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        dir
    }

    fn workspace_only() -> Prescan {
        Prescan { system: false, home: false, ..prescan(&[]) }
    }

    fn expand_in(dir: &Path, build_dir: &str, cmdline: &[&str]) -> Result<Expanded, String> {
        let args = strings(cmdline);
        let p = prescan(&args);
        let p = Prescan { system: false, home: false, ..p };
        let rc = RcFiles::discover(&p, &dir.join(build_dir))?;
        expand(&rc, &commands(p.tool.as_deref()), &args, p.end, true)
    }

    #[test]
    fn words() {
        assert_eq!(tokenize(r#"build --a="b c" 'd e'\ f --g=h\ i # comment"#).unwrap(), strings(&["build", "--a=b c", "d e f", "--g=h i"]));
        assert_eq!(tokenize(r#"x "a\"b\\c\d" a#b"#).unwrap(), strings(&["x", r#"a"b\c\d"#, "a#b"]));
        assert!(tokenize("x 'a").is_err());
        assert_eq!(logical_lines("a \\\n  b\nc\r\n"), vec![(1, "a   b".to_string()), (3, "c".to_string())]);
    }

    #[test]
    fn prescan_reads_the_command_line_only() {
        let p =
            prescan(&strings(&["-nC", "out", "--ninjars", "a", "--ninjars=b", "--nohome_rc", "--system_rc=false", "-t", "exec", "--", "cmake", "--ninjars=c"]));
        assert_eq!(p.working_dir.as_deref(), Some("out"));
        assert_eq!(p.tool.as_deref(), Some("exec"));
        assert_eq!(p.end, 9);
        assert_eq!(p.files, strings(&["a", "b"]));
        assert!(!p.home && !p.system && p.workspace && !p.ignore_all);
        assert_eq!(prescan(&strings(&["all", "--", "-t"])).end, 1);
    }

    #[test]
    fn order_and_configs() {
        let dir = tree(
            "order",
            &[(
                ".ninjars",
                "# settings\n\
                 build --remote_jobs=10\n\
                 common --remote_verbose --bazel_only_flag=1 --other_bazel_flag value\n\
                 build:ci --remote_jobs=20 --config=fast\n\
                 common:fast --remote_racing\n\
                 test --test_output=errors\n\
                 test:only_bazel --x\n",
            )],
        );
        // `common` first, whatever the order in the file; unknown options
        // of `common` lines skipped; `test` lines too.
        let e = expand_in(&dir, "build", &["-j8", "all"]).unwrap();
        assert_eq!(e.args, strings(&["--remote_verbose", "--remote_jobs=10", "-j8", "all"]));
        assert_eq!(e.explicit, vec![false, false, true, true]);
        // In place, recursively, least specific command first.
        let e = expand_in(&dir, "build", &["--remote_jobs=5", "--config", "ci", "--remote_jobs=30"]).unwrap();
        assert_eq!(
            e.args,
            strings(&[
                "--remote_verbose",
                "--remote_jobs=10",
                "--remote_jobs=5",
                "--config",
                "ci",
                "--remote_jobs=20",
                "--config=fast",
                "--remote_racing",
                "--remote_jobs=30"
            ])
        );
        // A config defined for Bazel's commands only is defined.
        assert_eq!(expand_in(&dir, "build", &["--config=only_bazel"]).unwrap().args.len(), 3);
        assert!(expand_in(&dir, "build", &["--config=nope"]).unwrap_err().contains("'nope' is not defined"));
        // After -t, the tool's arguments.
        let e = expand_in(&dir, "build", &["-t", "exec", "--", "x", "--config=nope"]).unwrap();
        assert_eq!(e.args[2..], strings(&["-t", "exec", "--", "x", "--config=nope"]));
    }

    /// A `.bazelrc` imported: its lines take what ninja-rs knows, skip the
    /// rest, and note values only Bazel takes; `common` lines do too.
    #[test]
    fn shared_with_bazel() {
        let dir = tree(
            "shared",
            &[
                (".ninjars", "import %workspace%/.bazelrc\ncommon --remote_jobs=many\nbuild --disk_cache=\n"),
                (
                    ".bazelrc",
                    "build --remote_executor=grpcs://e:1 --compilation_mode opt --disk_cache=/c\n\
                     build --remote_download_outputs=toplevel --bes_timeout=60s --remote_jobs=fifty\n\
                     common --remote_header=x-token=SECRET\n",
                ),
            ],
        );
        let e = expand_in(&dir, "build", &[]).unwrap();
        assert_eq!(
            e.args,
            strings(&[
                "--remote_header=x-token=SECRET",
                "--remote_executor=grpcs://e:1",
                "--disk_cache=/c",
                "--remote_download_outputs=toplevel",
                "--bes_timeout=60s",
                "--disk_cache="
            ])
        );
        assert_eq!(e.notes.len(), 2, "{:?}", e.notes);
        assert!(e.notes[0].contains(".ninjars:2: '--remote_jobs=many' skipped: invalid --remote_jobs"), "{:?}", e.notes);
        assert!(e.notes[1].contains(".bazelrc:2: '--remote_jobs=fifty' skipped"), "{:?}", e.notes);
        // A .ninjars file's build lines stay strict.
        let dir = tree("strict", &[(".ninjars", "build --compilation_mode opt\n")]);
        assert!(expand_in(&dir, "build", &[]).unwrap_err().contains("unrecognized option '--compilation_mode'"));
    }

    /// Lines shared with Bazel are read with Bazel's syntax: a value
    /// starting with `-` is the option's (`--copt -fPIC` isn't `-f PIC`),
    /// short options are Bazel's (`-j 200`, `-c opt`), and ninja-rs's
    /// options take the next word as their value whatever it starts with.
    #[test]
    fn shared_lines_have_bazel_syntax() {
        let dir = tree(
            "bazel_syntax",
            &[
                (".ninjars", "import %workspace%/.bazelrc\ncommon --linkopt -v -k 0 --remote_jobs 3\nbuild -j 4\n"),
                (
                    ".bazelrc",
                    "build --copt -fPIC -j 200 -c opt --keep_going -c dbg --remote_instance_name -main\n\
                     build --host_copt -O2 -s --remote_verbose\n",
                ),
            ],
        );
        let e = expand_in(&dir, "build", &[]).unwrap();
        assert_eq!(e.args, strings(&["--remote_jobs", "3", "--remote_instance_name", "-main", "--remote_verbose", "-j", "4"]));
        assert!(e.notes.is_empty(), "{:?}", e.notes);
    }

    #[test]
    fn errors() {
        for (content, error) in [
            ("build --nope", "unrecognized option '--nope'"),
            ("always --nope", "unrecognized option '--nope'"),
            ("build all", "'all' isn't an option"),
            ("build --remote_verbose all", "'all' isn't an option"),
            ("build --", "'--' isn't an option"),
            ("common -- --remote_verbose", "'--' isn't an option"),
            ("common --remote_verbose all", "'all' isn't an option"),
            ("build -C dir", "only goes on the command line"),
            ("build --ninjars=x", "only goes on the command line"),
            ("build --version", "only goes on the command line"),
            ("build --remote_jobs=x", "invalid --remote_jobs"),
            ("build:a --config=b\nbuild:b --config=a\nbuild --config=a", "cycle: a -> b -> a"),
            ("import", "import takes one file"),
            ("import missing", "missing"),
            ("build \"x", "unterminated"),
        ] {
            let dir = tree("errors", &[(".ninjars", content)]);
            let e = expand_in(&dir, "", &[]).unwrap_err();
            assert!(e.contains(error), "{content}: {e}");
            assert!(e.contains(".ninjars:"), "{content}: {e}");
        }
    }

    #[test]
    fn files() {
        let dir = tree(
            "files",
            &[
                (
                    "src/.ninjars",
                    "build --remote_instance_name=%workspace%\ntry-import %workspace%/user.ninjars\ntry-import missing.ninjars\nimport sub/more.ninjars\n",
                ),
                ("src/user.ninjars", "build --remote_jobs 7\n"),
                ("src/sub/more.ninjars", "build:x --remote_jobs=8\nunknown --y\n"),
                ("extra.ninjars", "build --remote_jobs=9\n"),
                ("loop/.ninjars", "import a\n"),
                ("loop/a", "import .ninjars\n"),
            ],
        );
        let ws = dir.join("src");
        let p = workspace_only();
        let rc = RcFiles::discover(&p, &ws.join("build/sub")).unwrap();
        assert_eq!(rc.workspace, ws);
        assert_eq!(rc.files, vec![ws.join(FILE_NAME)]);
        assert_eq!(rc.warnings.len(), 1);
        let e = expand(&rc, &commands(None), &[], 0, true).unwrap();
        assert_eq!(e.args, vec![format!("--remote_instance_name={}", ws.display()), "--remote_jobs".into(), "7".into()]);
        // --ninjars after the others; /dev/null ends them.
        let extra = dir.join("extra.ninjars").to_string_lossy().into_owned();
        let p = Prescan { files: vec![extra.clone(), "/dev/null".into(), "missing".into()], ..workspace_only() };
        let rc = RcFiles::discover(&p, &ws).unwrap();
        assert_eq!(expand(&rc, &commands(None), &[], 0, true).unwrap().args.last().map(String::as_str), Some("--remote_jobs=9"));
        let p = Prescan { files: vec!["missing".into()], ..workspace_only() };
        assert!(RcFiles::discover(&p, &ws).unwrap_err().contains("no such file"));
        // --noworkspace_rc, --ignore_all_rc_files.
        let p = Prescan { workspace: false, files: vec![extra], ..workspace_only() };
        assert_eq!(RcFiles::discover(&p, &ws).unwrap().files.len(), 1);
        let p = Prescan { ignore_all: true, ..workspace_only() };
        assert!(RcFiles::discover(&p, &ws).unwrap().files.is_empty());
        assert!(RcFiles::discover(&workspace_only(), &dir.join("loop")).unwrap_err().contains("import loop"));
        // A nested invocation: its own --config only.
        let rc = RcFiles::reread(&[ws.join(FILE_NAME).to_string_lossy().into_owned()], Some(&ws.to_string_lossy()), &ws).unwrap();
        assert_eq!(expand(&rc, &commands(None), &strings(&["--config=x"]), 1, false).unwrap().args, strings(&["--config=x", "--remote_jobs=8"]));
    }

    #[test]
    fn platform_and_announce() {
        let os = platform_config();
        let dir = tree(
            "platform",
            &[(".ninjars", &format!("common --enable_platform_specific_config --announce_rc\nbuild:{os} --remote_jobs=3\nbuild --remote_jobs=1\n"))],
        );
        let e = expand_in(&dir, "", &["--remote_jobs=2"]).unwrap();
        assert_eq!(e.args, strings(&["--enable_platform_specific_config", "--announce_rc", "--remote_jobs=1", "--remote_jobs=3", "--remote_jobs=2"]));
        assert!(e.announce);
        assert!(e.announcements.iter().any(|a| a.contains(&format!("build:{os}"))));
        let e = expand_in(&dir, "", &["--noenable_platform_specific_config", "--announce_rc=false"]).unwrap();
        assert!(!e.args.contains(&"--remote_jobs=3".to_string()));
        assert!(!e.announce);
    }

    /// The files a `.bazelrc` imports are Bazel's too, whatever their names
    /// (`.bazelrc.user`, `tools/bazel.rc`): read as Bazel reads them. Those
    /// a `.ninjars` file imports stay strict.
    #[test]
    fn bazel_imports() {
        let dir = tree(
            "bazel_imports",
            &[
                (
                    ".ninjars",
                    "import %workspace%/.bazelrc
try-import %workspace%/local.rc
",
                ),
                (
                    ".bazelrc",
                    "build --copt -fPIC
try-import %workspace%/.bazelrc.user
import %workspace%/tools/bazel.rc
",
                ),
                (
                    ".bazelrc.user",
                    "build --copt=-O2 --remote_jobs=4
",
                ),
                (
                    "tools/bazel.rc",
                    "build --cxxopt=-std=c++17 --linkopt -v
",
                ),
                (
                    "local.rc",
                    "build --remote_jobs=5
",
                ),
            ],
        );
        let e = expand_in(&dir, "", &[]).unwrap();
        assert_eq!(e.args, strings(&["--remote_jobs=4", "--remote_jobs=5"]));
        std::fs::write(
            dir.join("local.rc"),
            "build --copt=-O2
",
        )
        .unwrap();
        assert!(expand_in(&dir, "", &[]).unwrap_err().contains("local.rc:1: unrecognized option '--copt'"));
    }

    /// Options of shared lines skipped as Bazel's are announced: a misspelt
    /// option of ninja-rs's is one.
    #[test]
    fn skipped_options_are_announced() {
        let dir = tree(
            "announce_skipped",
            &[(
                ".ninjars",
                "common --remote_jbos=4 --remote_jobs=3
",
            )],
        );
        let e = expand_in(&dir, "", &["--announce_rc"]).unwrap();
        assert_eq!(e.args, strings(&["--remote_jobs=3", "--announce_rc"]));
        assert_eq!(e.skipped.len(), 1);
        assert!(e.announcements.iter().any(|a| a.contains(".ninjars:1: '--remote_jbos=4'")), "{:?}", e.announcements);
    }

    /// The search stops at the home directory however its path is spelt:
    /// its file is the user's (`--nohome_rc` leaves it out).
    #[test]
    fn workspace_search_stops_at_home() {
        let dir = tree(
            "home",
            &[
                (
                    ".ninjars",
                    "build --remote_jobs=1
",
                ),
                ("builds/proj/x", ""),
            ],
        );
        let build = dir.join("builds/proj");
        let mut warnings = Vec::new();
        assert_eq!(workspace_file_below(&build, Some(&dir), &mut warnings), None);
        assert_eq!(workspace_file_below(&build, None, &mut warnings), Some(dir.join(FILE_NAME)));
        #[cfg(windows)]
        {
            let other_case = PathBuf::from(dir.to_string_lossy().to_uppercase());
            assert_eq!(workspace_file_below(&build, Some(&other_case), &mut warnings), None);
        }
        #[cfg(unix)]
        {
            let link = dir.with_file_name(format!("{}-link", dir.file_name().unwrap().to_string_lossy()));
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&dir, &link).unwrap();
            assert_eq!(workspace_file_below(&build, Some(&link), &mut warnings), None);
            let _ = std::fs::remove_file(&link);
        }
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// On Unix, a file the user owns that others can write to isn't read;
    /// unless every file of its directory looks so (writable by all, no
    /// sticky bit: WSL's view of Windows drives).
    #[cfg(unix)]
    #[test]
    fn writable_files_are_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tree("writable", &[(".ninjars", ""), ("build/x", "")]);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut warnings = Vec::new();
        assert_eq!(workspace_file(&dir.join("build"), &mut warnings), Some(dir.join(FILE_NAME)));
        for mode in [0o664, 0o646] {
            std::fs::set_permissions(dir.join(FILE_NAME), std::fs::Permissions::from_mode(mode)).unwrap();
            warnings.clear();
            assert_eq!(workspace_file(&dir.join("build"), &mut warnings), None, "{mode:o}");
            assert!(warnings[0].contains("skipped: other users can change it"), "{warnings:?}");
        }
        std::fs::set_permissions(dir.join(FILE_NAME), std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(workspace_file(&dir.join("build"), &mut warnings), Some(dir.join(FILE_NAME)));
    }

    #[test]
    fn workspace_search_bounds() {
        // The repository's root ends the search: a file above it isn't the
        // workspace's.
        let dir = tree("bounds", &[(".ninjars", "build --remote_jobs=1\n"), ("repo/.git/HEAD", ""), ("repo/build/x", "")]);
        let mut warnings = Vec::new();
        assert_eq!(workspace_file(&dir.join("repo/build"), &mut warnings), None);
        std::fs::write(dir.join("repo/.ninjars"), "").unwrap();
        assert_eq!(workspace_file(&dir.join("repo/build"), &mut warnings), Some(dir.join("repo").join(FILE_NAME)));
        assert!(warnings.is_empty(), "{warnings:?}");
        // Nor in a directory others can write to.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let shared = tree("shared", &[(".ninjars", ""), ("b/x", "")]);
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
            assert_eq!(workspace_file(&shared.join("b"), &mut warnings), None);
            assert!(warnings[0].contains("skipped: other users can write to"), "{warnings:?}");
        }
    }

    /// Grants `access` (icacls's syntax) to `sid` on `path`.
    #[cfg(windows)]
    fn grant(path: &Path, sid: &str, access: &str) {
        let out = std::process::Command::new("icacls").arg(path).arg("/grant").arg(format!("*{sid}:{access}")).output().unwrap();
        assert!(out.status.success(), "icacls: {}", String::from_utf8_lossy(&out.stdout));
    }

    #[cfg(windows)]
    const AUTHENTICATED_USERS: &str = "S-1-5-11";
    #[cfg(windows)]
    const USERS: &str = "S-1-5-32-545";

    /// On Windows, the owners and permissions: below a drive's root,
    /// directories let Authenticated Users modify them (`C:\build`).
    #[cfg(windows)]
    #[test]
    fn workspace_permissions() {
        let mut warnings = Vec::new();
        let dir = tree("acl", &[(".ninjars", ""), ("build/x", "")]);
        assert_eq!(workspace_file(&dir.join("build"), &mut warnings), Some(dir.join(FILE_NAME)));
        // Others can write to the directories (inherited by what they hold).
        grant(&dir, AUTHENTICATED_USERS, "(OI)(CI)(M)");
        assert_eq!(workspace_file(&dir.join("build"), &mut warnings), None);
        assert_eq!(warnings, vec![format!("{} skipped: other users can write to {}{SHARED_HINT}", dir.join(FILE_NAME).display(), dir.join("build").display())]);
        // Others can change the file only.
        let dir = tree("acl_file", &[(".ninjars", ""), ("build/x", "")]);
        grant(&dir.join(FILE_NAME), USERS, "(W)");
        warnings.clear();
        assert_eq!(workspace_file(&dir.join("build"), &mut warnings), None);
        assert!(warnings[0].ends_with(&format!(".ninjars skipped: other users can change it{CHANGE_HINT}")), "{warnings:?}");
        // Rights for what a directory will hold (inherit-only) don't count.
        let dir = tree("acl_inherit", &[(".ninjars", "")]);
        grant(&dir, AUTHENTICATED_USERS, "(CI)(IO)(M)");
        let later = dir.join("later");
        std::fs::create_dir(&later).unwrap();
        assert_eq!(workspace_file(&dir, &mut warnings), Some(dir.join(FILE_NAME)));
        assert_eq!(workspace_file(&later, &mut warnings), None);
        // A link to a file others can change, and a link others can change
        // (where symbolic links can be made).
        let dir = tree("acl_link", &[("target", ""), ("ws/build/x", ""), ("trusted", ""), ("ws2/build/x", "")]);
        grant(&dir.join("target"), USERS, "(M)");
        if std::os::windows::fs::symlink_file(dir.join("target"), dir.join("ws").join(FILE_NAME)).is_ok() {
            assert_eq!(workspace_file(&dir.join("ws/build"), &mut warnings), None);
            let link = dir.join("ws2").join(FILE_NAME);
            std::os::windows::fs::symlink_file(dir.join("trusted"), &link).unwrap();
            assert_eq!(workspace_file(&dir.join("ws2/build"), &mut warnings), Some(link.clone()));
            let out = std::process::Command::new("icacls").arg(&link).args(["/L", "/grant", &format!("*{USERS}:(M)")]).output().unwrap();
            assert!(out.status.success());
            assert_eq!(workspace_file(&dir.join("ws2/build"), &mut warnings), None);
        }
    }

    /// On Windows, `%ProgramData%\ninjars`: anyone can create files in
    /// `%ProgramData%`, so its file is read only when the user or the
    /// administrators own it and no one else can change it.
    #[cfg(windows)]
    #[test]
    fn system_file_permissions() {
        // As %ProgramData%'s: users can add files and directories.
        let dir = tree("programdata", &[("ninjars", "build --remote_jobs=5\n")]);
        grant(&dir, USERS, "(CI)(WD,AD,WEA,WA)");
        let old = std::env::var_os("ProgramData");
        let discover = || {
            // SAFETY: always safe on Windows; no other test reads it.
            unsafe { std::env::set_var("ProgramData", dir.path()) };
            let rc = RcFiles::discover(&Prescan { workspace: false, home: false, ..prescan(&[]) }, &dir);
            match &old {
                // SAFETY: as above.
                Some(old) => unsafe { std::env::set_var("ProgramData", old) },
                None => unsafe { std::env::remove_var("ProgramData") },
            }
            rc.unwrap()
        };
        let rc = discover();
        assert_eq!(rc.files, vec![dir.join("ninjars")]);
        assert!(rc.warnings.is_empty(), "{:?}", rc.warnings);
        grant(&dir.join("ninjars"), USERS, "(M)");
        let rc = discover();
        assert!(rc.files.is_empty());
        assert_eq!(rc.warnings, vec![format!("{} skipped: other users can change it{CHANGE_HINT}", dir.join("ninjars").display())]);
    }

    /// On Windows, who may own and change a file: the user, the
    /// administrators, SYSTEM and TrustedInstaller.
    #[cfg(windows)]
    #[test]
    fn trusted_principals() {
        let user = "S-1-5-21-1-2-3-1000";
        // A file another user made (this user can't make one).
        let planted = sys::Security { owner: "S-1-5-21-1-2-3-1001".into(), writers: vec!["S-1-5-18".into()] };
        assert!(!planted.private_to(user));
        let own = sys::Security { owner: user.into(), writers: vec!["S-1-5-18".into(), "S-1-3-4".into(), user.into()] };
        assert!(own.private_to(user));
        let admins = sys::Security { owner: "S-1-5-32-544".into(), writers: vec!["S-1-5-32-544".into()] };
        assert!(admins.private_to(user));
        // TrustedInstaller owns the system's directories, which only it,
        // SYSTEM and the administrators can write to.
        let windows = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        assert!(!writable_by_others(&windows), "{}", windows.display());
    }

    #[test]
    fn exec_takes_build_lines() {
        let dir = tree("exec", &[(".ninjars", "exec --remote_jobs=2\nbuild --remote_jobs=1\n")]);
        assert_eq!(expand_in(&dir, "", &["-t", "exec", "x"]).unwrap().args, strings(&["--remote_jobs=1", "--remote_jobs=2", "-t", "exec", "x"]));
        assert_eq!(expand_in(&dir, "", &[]).unwrap().args, strings(&["--remote_jobs=1"]));
    }
}
