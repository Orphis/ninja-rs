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
//! have, so that one file can serve Bazel and ninja-rs; Bazel's other
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
//! `--nohome_rc` and `--ignore_all_rc_files` leave files out.

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
    command: String,
    config: Option<String>,
    args: Vec<String>,
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

/// The system's file.
fn system_file() -> Option<PathBuf> {
    if cfg!(windows) { std::env::var_os("ProgramData").map(|d| PathBuf::from(d).join("ninjars")) } else { Some(PathBuf::from("/etc/ninjars")) }
}

/// The workspace's file: the first `.ninjars` from `dir` up, short of the
/// home directory (whose file is the user's).
pub fn workspace_file(dir: &Path) -> Option<PathBuf> {
    let home = options::home_dir();
    dir.ancestors().take_while(|d| home.as_deref() != Some(*d)).map(|d| d.join(FILE_NAME)).find(|f| f.is_file())
}

fn is_dev_null(f: &str) -> bool {
    f == "/dev/null" || (cfg!(windows) && f.eq_ignore_ascii_case("NUL"))
}

impl RcFiles {
    /// Reads the files the command line (`prescan`) selects, for a build in
    /// `build_dir` (absolute).
    pub fn discover(p: &Prescan, build_dir: &Path) -> Result<RcFiles, String> {
        let found = workspace_file(build_dir);
        let workspace = found.as_ref().and_then(|f| f.parent()).unwrap_or(build_dir).to_path_buf();
        let mut rc = RcFiles { workspace, ..Default::default() };
        if p.ignore_all {
            return Ok(rc);
        }
        let mut files: Vec<(PathBuf, bool)> = Vec::new();
        if p.system
            && let Some(f) = system_file()
        {
            files.push((f, false));
        }
        if p.workspace
            && let Some(f) = found
        {
            files.push((f, false));
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
            rc.read(&f, &mut Vec::new())?;
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
                rc.read(&f, &mut Vec::new())?;
                rc.files.push(f);
            }
        }
        Ok(rc)
    }

    /// Reads one file and what it imports. `stack`: the files importing it.
    fn read(&mut self, file: &Path, stack: &mut Vec<PathBuf>) -> Result<(), String> {
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
                    self.read(&path, stack).map_err(|e| format!("{origin}: {e}"))?;
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
                        self.lines.push(Line { origin, file: file.to_path_buf(), command: command.to_string(), config, args: Vec::new() });
                        continue;
                    }
                    self.lines.push(Line { origin, file: file.to_path_buf(), command: command.to_string(), config, args: args.to_vec() });
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
                out.announcements.push(format!("  '{}' options: {}", line.command, out.args[first..].join(" ")));
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
        out.args.splice(files_end..files_end, platform.args);
        out.explicit.splice(files_end..files_end, platform.explicit);
    }
    let options_end = out.args.len() - (cmdline.len() - end.min(cmdline.len()));
    out.announce = last_bool(&out.args[..options_end], "announce_rc") == Some(true);
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
                out.args[first..].join(" ")
            ));
        }
    }
    stack.pop();
    Ok(())
}

/// A line's options, checked, with its `--config`s expanded in place.
fn expand_line(rc: &RcFiles, levels: &[&[&str]], line: &Line, stack: &mut Vec<String>, out: &mut Expanded) -> Result<(), String> {
    let args = &line.args;
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
        if !a.starts_with('-') || a == "-" {
            return Err(format!("{}: '{a}' isn't an option", line.origin));
        }
        // An option's value may be the next word: there are no targets here.
        let takes_next = !a.contains('=') && args.get(i + 1).is_some_and(|n| !n.starts_with('-'));
        let n = if takes_next && !(a.starts_with("--") && options::is_bool_option(&a[2..])) { 2 } else { 1 };
        let group = &args[i..i + n];
        match options::check_rc_option(group) {
            Ok(()) => {
                for a in group {
                    out.push(a, false);
                }
            }
            // `common` lines are shared with Bazel: its options are skipped.
            Err(RcOptionError::Unknown(_)) if line.command == "common" => {}
            Err(RcOptionError::Unknown(e) | RcOptionError::Invalid(e)) => return Err(format!("{}: {e}", line.origin)),
        }
        i += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Writes files in a fresh directory: `(relative path, content)`.
    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ninja-rs-rc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn errors() {
        for (content, error) in [
            ("build --nope", "unrecognized option '--nope'"),
            ("always --nope", "unrecognized option '--nope'"),
            ("build all", "'all' isn't an option"),
            ("build --remote_verbose all", "'all' isn't an option"),
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

    #[test]
    fn exec_takes_build_lines() {
        let dir = tree("exec", &[(".ninjars", "exec --remote_jobs=2\nbuild --remote_jobs=1\n")]);
        assert_eq!(expand_in(&dir, "", &["-t", "exec", "x"]).unwrap().args, strings(&["--remote_jobs=1", "--remote_jobs=2", "-t", "exec", "x"]));
        assert_eq!(expand_in(&dir, "", &[]).unwrap().args, strings(&["--remote_jobs=1"]));
    }
}
