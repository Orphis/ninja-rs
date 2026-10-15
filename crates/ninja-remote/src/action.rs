//! Turns a ninja task into a REAPI `Action`: command unwrapping, include
//! scanning, exec root selection, path rewriting and the Merkle input root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ninja_core::runner::Task;
use rbe_client::proto as re;
use rbe_client::{BlobSet, BlobSource, DigestCache, InputTree};

use crate::cmdline::{join_posix_chain, join_rsp, split_rsp, unwrap_command};
use crate::env::Tool;
use crate::paths::{ExecRoot, PathMapper, TreeIndex, normalize_abs};
use crate::scanner::{self, Scanner, Style, decode_rsp, expand_response_files, parse_compile, search_path};
use crate::store::Store;
use crate::toolchains::{Toolchains, long_path, resolve_program};

/// How compiles' inputs are found (`--remote_include_scanner`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IncludeScanner {
    /// Following every `#include`, whatever the conditionals (`scanner`).
    Conservative,
    /// The `cpp-deps` preprocessor, exact for GCC, Clang and cl commands;
    /// the compiler's own when it can't tell.
    #[default]
    Preprocessor,
    /// The `clang-scan-deps` next to the compiler (Clang).
    ClangScanDeps,
    /// The compiler's own preprocessor (`-M`, `/E /showIncludes`).
    Compiler,
}

impl std::str::FromStr for IncludeScanner {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s {
            "conservative" => Self::Conservative,
            "preprocessor" => Self::Preprocessor,
            "clang-scan-deps" => Self::ClangScanDeps,
            "compiler" => Self::Compiler,
            _ => return Err(format!("unknown include scanner '{s}' (conservative, preprocessor, clang-scan-deps or compiler)")),
        })
    }
}

/// Per-build settings shared by all actions.
pub struct Shared {
    pub build_dir: PathBuf,
    pub toolchains: Toolchains,
    pub exec_root: Option<String>,
    pub store: Arc<Store>,
    pub windows: bool,
    /// Outputs of earlier remote actions that only exist remotely.
    pub remote_outputs: Option<Arc<crate::remote_outputs::RemoteOutputs>>,
    /// List every input's digest in `Prepared::input_digests`.
    pub log_inputs: bool,
    pub include_scanner: IncludeScanner,
    pub verbose: bool,
    /// The daemon (`--daemon`): it scans, digests and lists toolchains
    /// with what it kept from earlier builds.
    pub daemon: Option<Arc<ninja_daemon::Client>>,
    /// Why the daemon was given up on during this build.
    pub daemon_error: Mutex<Option<String>>,
    /// Where compiler profiles persist (in the disk cache).
    pub profile_dir: Option<PathBuf>,
    /// Directories the build writes to, the build directory first: their
    /// contents may change during the build, so the scanners check them
    /// every time (the preprocessor's cache, everything under them).
    pub volatile: Vec<PathBuf>,
    /// The directories build edges write to (see `WrittenDirs`): the
    /// conservative scanner runs a computed include there locally.
    pub written: scanner::WrittenDirs,
    /// `--action_env`: variables every action gets, as given.
    pub action_env: Vec<(String, String)>,
    /// Compiles the preprocessor left to the compiler.
    pub compiler_scans: CompilerScans,
}

/// Compiles whose includes the preprocessor's scanner left to the
/// compiler, by reason (without what is the compile's): each reason told
/// once, and those the compiler listed summed up at the end of the build.
#[derive(Default)]
pub struct CompilerScans {
    told: Mutex<std::collections::HashSet<String>>,
    listed: Mutex<BTreeMap<String, usize>>,
}

impl CompilerScans {
    /// Notes a compile left for `reason`: true the first time for it.
    fn note(&self, reason: &str) -> bool {
        self.told.lock().unwrap().insert(reason.to_string())
    }

    /// Notes a compile left for `reason` the compiler listed the includes of.
    fn listed(&self, reason: String) {
        *self.listed.lock().unwrap().entry(reason).or_default() += 1;
    }

    /// A line for the end of the build: how many the compiler listed, and
    /// why (the most frequent reasons).
    pub fn summary(&self) -> Option<String> {
        let reasons = self.listed.lock().unwrap();
        let total: usize = reasons.values().sum();
        let mut by: Vec<(&String, &usize)> = reasons.iter().collect();
        by.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let mut listed: Vec<String> = by.iter().take(4).map(|(why, n)| format!("{n}: {why}")).collect();
        if by.len() > 4 {
            listed.push(format!("{} more reasons", by.len() - 4));
        }
        (total > 0).then(|| format!("{total} compiles' includes were the compiler's, the preprocessor's scan being unable to tell ({}).", listed.join("; ")))
    }
}

impl Shared {
    /// The daemon, unless it failed during this build.
    pub fn daemon(&self) -> Option<&ninja_daemon::Client> {
        if self.daemon_error.lock().unwrap().is_some() { None } else { self.daemon.as_deref() }
    }

    /// Gives up on the daemon for the rest of the build: the build does
    /// the work itself.
    pub fn daemon_failed(&self, e: &std::io::Error) {
        self.daemon_error.lock().unwrap().get_or_insert_with(|| e.to_string());
    }

    /// The environment compilers are probed in: the build's own.
    fn scan_env(&self) -> cpp_deps::driver::Environment {
        cpp_deps::driver::Environment { vars: None, profile_dir: self.profile_dir.clone() }
    }
}

/// An action ready to be looked up or executed.
pub struct Prepared {
    pub action_digest: re::Digest,
    pub blobs: BlobSet,
    pub mapper: PathMapper,
    pub tree_index: TreeIndex,
    /// (path relative to the working directory, local path)
    pub outputs: Vec<(String, PathBuf)>,
    pub depfile: Option<(String, PathBuf)>,
    pub warnings: Vec<String>,
    pub input_files: usize,
    /// Steps of the command that run on the host (local tools), before and
    /// after the remote action, as argv.
    pub local_before: Vec<Vec<String>>,
    pub local_after: Vec<Vec<String>>,
    /// Outputs the command may write besides the declared ones (a link's
    /// PDB or import library): (path relative to the working directory,
    /// local path).
    pub side_outputs: Vec<(String, PathBuf)>,
    /// Inputs that only exist remotely: (local path, task input, digest).
    pub remote_inputs: Vec<(PathBuf, String, re::Digest)>,
    pub command: re::Command,
    /// Response files as the remote action reads them (paths rewritten):
    /// (local path, contents).
    pub rsp_files: Vec<(PathBuf, Vec<u8>)>,
    pub input_root_digest: re::Digest,
    /// Every input with its digest, when `Shared::log_inputs` (for the
    /// execution log).
    pub input_digests: Vec<(String, String)>,
    /// Every program the action runs is a tool the toolchains file
    /// declares: one the generator vouches the action describes, with the
    /// files it sends.
    pub toolchain_declared: bool,
    /// The inputs read from the host's files (not those of toolchain
    /// directories, sent whole), with their stamps when the key's digests
    /// were taken: a local run's result is only the action's if those it
    /// read didn't change since.
    pub local_inputs: Vec<(PathBuf, ninja_os::FileStamp)>,
    /// A compile's sources: read whatever its depfile (or `/showIncludes`
    /// output) says.
    pub sources: Vec<PathBuf>,
    /// Why a local run's result isn't the action's, if it isn't: a
    /// variable set here that a tool of the action reads, which the action
    /// can't carry (see `env`).
    pub env_refused: Option<String>,
    /// The command a local run uses when the action's files span volumes:
    /// None when it's the remote action's.
    pub local: Option<LocalForm>,
}

/// The remote action's command as a local run uses it when the action's
/// files span volumes (a source tree on `D:`, a toolchain on `C:`, a build
/// directory on a share): run from the build directory, it can't reach the
/// other volumes' files by relative paths, so those keep the host's (see
/// `PathMapper::for_host`). The others are the remote action's, as are its
/// outputs.
#[derive(Clone, Debug)]
pub struct LocalForm {
    pub arguments: Vec<String>,
    /// Response files: (local path, contents).
    pub rsp_files: Vec<(PathBuf, Vec<u8>)>,
    pub env: Vec<(String, String)>,
}

/// How a command run from the build directory names its program `exe`:
/// relative to it (`./x` in it), or as the host does when it's on another
/// volume (a local run, see `PathMapper::for_host`). None outside the
/// tree.
fn program_arg(mapper: &mut PathMapper, exe: &Path, windows: bool) -> Option<String> {
    let rel = mapper.tree_path(&to_s(exe))?;
    if mapper.on_other_volume(&rel) {
        return Some(to_s(exe));
    }
    let mut p = mapper.rel_from_work_dir(&rel);
    if !p.contains('/') {
        p = format!("./{p}");
    }
    if windows {
        p = p.replace('/', "\\");
    }
    Some(p)
}

fn to_s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Whether a step's command line is cl-style (cl.exe, clang-cl, MSVC's
/// linkers and librarian): `/D` defines a macro there.
fn cl_style(exe: &Path, argv: &[String]) -> bool {
    let name = exe.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    scanner::compiler_style(&to_s(exe), argv) == Some(Style::Msvc)
        || crate::link::linker_flavor(&to_s(exe)) == Some(crate::link::Flavor::Msvc)
        || name == "lib"
        || name == "llvm-lib"
}

/// Prepares the REAPI action for `task`. Errors mean "run it locally".
pub fn prepare(task: &Task, shared: &Shared) -> Result<Prepared, String> {
    let windows = shared.windows;
    let build_dir = &shared.build_dir;
    let build_dir_s = to_s(build_dir);
    let mut steps = unwrap_command(&task.command, windows)?.steps;
    let mut exes = Vec::new();
    for argv in &steps {
        let exe = resolve_program(&argv[0], build_dir, windows).ok_or_else(|| format!("cannot find program '{}'", argv[0]))?;
        exes.push(long_path(&exe));
    }
    // Steps running local tools run on the host, before or after the others.
    let is_local = |e: &PathBuf| shared.toolchains.find(e).is_some_and(|t| t.local);
    let first = exes.iter().position(|e| !is_local(e)).ok_or("the command only runs local tools")?;
    let last = exes.iter().rposition(|e| !is_local(e)).unwrap_or(first);
    if exes[first..=last].iter().any(is_local) {
        return Err("the command runs local tools between other steps".into());
    }
    let local_after = steps.split_off(last + 1);
    let local_before: Vec<Vec<String>> = steps.drain(..first).collect();
    exes.truncate(last + 1);
    exes.drain(..first);
    if steps.len() > 1 && windows {
        // cmd.exe's quoting does not survive a remote worker's own quoting.
        return Err(format!("the command runs {} steps remotely", steps.len()));
    }
    let argv = &steps[0];
    let exe = &exes[0];
    let toolchains: Vec<_> = exes.iter().filter_map(|e| shared.toolchains.find(e).cloned()).collect();
    let toolchain = shared.toolchains.find(exe).cloned().or_else(|| toolchains.first().cloned());
    let toolchain_declared = exes.iter().all(|e| shared.toolchains.find(e).is_some());
    let mut warnings = Vec::new();

    // Only a single compiler invocation is scanned for includes.
    let style = if steps.len() == 1 { scanner::compiler_style(&exe.to_string_lossy(), argv) } else { None };
    let is_compile = task.binding("rbe_mnemonic").is_some_and(|m| m.ends_with("Compile"));
    if is_compile && style.is_none() {
        return Err(format!("unrecognized compiler '{}'", argv[0]));
    }

    // Files and directories the action reads.
    let mut files: Vec<PathBuf> = Vec::new();
    let mut whole_dirs: Vec<PathBuf> = Vec::new();
    let mut rsp_files: Vec<PathBuf> = Vec::new();
    let (expanded, rsps) = expand_response_files(argv, build_dir, windows);
    // What its tool reads from the environment (see `env`). Options it
    // takes from there (cl's `CL`, link's `LINK`) are part of its command
    // line as the scanner and the check of definitions see it.
    let host_var = |name: &str| std::env::var_os(name);
    // Whether a file a tool looks for is there: on the host, or only
    // remotely (an output left in the remote CAS).
    let present = |p: &Path| {
        let p = scanner::normalize(&build_dir.join(p));
        p.is_file() || shared.remote_outputs.as_ref().zip(p.strip_prefix(build_dir).ok()).is_some_and(|(r, rel)| r.get(&to_s(rel)).is_some())
    };
    let first_style = scanner::compiler_style(&to_s(exe), argv);
    let (env_before, env_after) = crate::env::env_options(&Tool::of_step(exe, argv, &expanded, first_style, windows, &present, None), &host_var);
    let effective: Vec<String> = expanded[..1].iter().chain(&env_before).chain(&expanded[1..]).chain(&env_after).cloned().collect();
    // Steps with cl-style command lines, and their response files.
    let cl_steps: Vec<bool> = steps.iter().zip(&exes).map(|(s, e)| cl_style(e, s)).collect();
    let mut cl_rsp_files: Vec<PathBuf> = if cl_steps[0] { rsps.clone() } else { Vec::new() };
    rsp_files.extend(rsps);
    for (later, cl) in steps[1..].iter().zip(&cl_steps[1..]) {
        let rsps = expand_response_files(later, build_dir, windows).1;
        if *cl {
            cl_rsp_files.extend(rsps.iter().cloned());
        }
        rsp_files.extend(rsps);
    }
    // A driver that compiles nothing links: its linker's inputs are found
    // from its dry run, like a linker's from its arguments.
    let driver_link = style.is_some() && !crate::link::driver_compiles(&effective);
    // A compile's other files (split DWARF, coverage notes...), asked for.
    let compile_outputs =
        if style.is_some() && !driver_link { crate::compile_outputs::side_outputs(&effective, style == Some(Style::Msvc))? } else { Vec::new() };
    // A driver without a dry run sends its declared inputs only. A driver
    // reads its response files itself, in its dry run; a linker's command
    // line is read as the linker reads it: expanded, with what `LINK` and
    // `_LINK_` add.
    let link = if steps.len() == 1 && (driver_link || crate::link::linker_flavor(&exe.to_string_lossy()).is_some()) {
        let line = if driver_link { argv } else { &effective };
        match crate::link::analyze(exe, line, build_dir, driver_link, &host_var, &|p| shared.store.link_directives(p)) {
            Ok(info) => Some(info),
            Err(e) => {
                warnings.push(format!("link inputs: {e}"));
                None
            }
        }
    } else {
        None
    };
    // The chosen scanner first. The preprocessor's leaves what it can't tell
    // exactly to the compiler's; that one leaves what the compiler can't
    // tell (it can't run here, fails, or takes too long) to the
    // conservative one.
    let mut scanned = false;
    let mut sources: Vec<PathBuf> = Vec::new();
    if style.is_some() && !driver_link && steps.len() == 1 && shared.include_scanner != IncludeScanner::Conservative {
        let mut argv = expanded.clone();
        argv[0] = to_s(exe);
        // The compiler's list, with the files a scan found the compile
        // looks for, which it doesn't list (`__has_include`, `#pragma GCC
        // dependency`).
        let compiler = |looked_for: Vec<PathBuf>| {
            let prefix = (!task.msvc_deps_prefix.is_empty()).then_some(task.msvc_deps_prefix.as_str());
            cpp_deps::external::compiler_deps(&argv, build_dir, prefix).map(|deps| deps.into_iter().chain(looked_for).collect::<Vec<_>>())
        };
        let (found, by) = match shared.include_scanner {
            IncludeScanner::Preprocessor => {
                let scanned = match shared.daemon().map(|d| d.scan(&argv, build_dir)) {
                    Some(Ok(r)) => r,
                    failed => {
                        if let Some(Err(e)) = failed {
                            shared.daemon_failed(&e);
                        }
                        shared.store.preprocessor(build_dir, windows).scan_in(&argv, build_dir, &shared.scan_env())
                    }
                };
                // An include it couldn't find (an error for the compile, a
                // file only the remote has, or a lookup it got wrong) means
                // inputs may be missing too: the compiler's then.
                match scanned {
                    Ok(out) if out.exact() && out.missing.is_empty() => (Ok(out.files.into_iter().chain(out.found).collect()), IncludeScanner::Preprocessor),
                    left => {
                        // Why, and the reason without what is the compile's.
                        let (why, reason, looked_for) = match left {
                            Ok(out) => {
                                let (why, reason) = if let Some(w) = out.inexact.first() {
                                    // ("file:line: what: details", or "what".)
                                    let what = w.split_once(": ").map_or(w.as_str(), |(_, w)| w);
                                    (w.clone(), what.split(": ").next().unwrap_or_default().to_string())
                                } else if let Some((o, a)) = out.unknown.first() {
                                    (format!("unknown {o}({a})"), format!("unknown {o}"))
                                } else {
                                    (format!("{} not found", out.missing.first().cloned().unwrap_or_default()), "an #include not found".to_string())
                                };
                                (format!("not exact ({why})"), format!("not exact ({reason})"), out.found)
                            }
                            Err(e) => (e.clone(), e.split(": ").next().unwrap_or_default().to_string(), Vec::new()),
                        };
                        // Every time with --remote_verbose, else once per reason.
                        let why = format!("Preprocessor include scanner: {why}; asking the compiler");
                        if shared.compiler_scans.note(&reason) && !shared.verbose {
                            ninja_core::status::message(&format!("warning: {why} (other compiles for this reason too: --remote_verbose names them)"));
                        }
                        if shared.verbose {
                            warnings.push(why);
                        }
                        let found = compiler(looked_for);
                        if found.is_ok() {
                            shared.compiler_scans.listed(reason);
                        }
                        (found, IncludeScanner::Compiler)
                    }
                }
            }
            IncludeScanner::ClangScanDeps => (cpp_deps::external::clang_scan_deps(&argv, build_dir), IncludeScanner::ClangScanDeps),
            IncludeScanner::Compiler => (compiler(Vec::new()), IncludeScanner::Compiler),
            IncludeScanner::Conservative => unreachable!(),
        };
        match found {
            Ok(found) => {
                files.extend(found);
                let info = parse_compile(&effective, build_dir);
                sources.extend(info.sources.iter().cloned());
                files.extend(info.extra_files);
                whole_dirs.extend(info.extra_dirs);
                scanned = true;
            }
            Err(e) => {
                // Every time with --remote_verbose, else once per reason.
                let why = format!("{by:?} include scanner: {e}; scanning conservatively");
                if shared.store.scan.fell_back(&e) && !shared.verbose {
                    ninja_core::status::message(&format!("warning: {why} (other compiles for this reason too: --remote_verbose names them)"));
                }
                if shared.verbose {
                    warnings.push(why);
                }
            }
        }
    }
    if style.is_some() && !driver_link && !scanned {
        let mut info = parse_compile(&effective, build_dir);
        sources.extend(info.sources.iter().cloned());
        let scanner = Scanner { cache: &shared.store.scan, windows, written: &shared.written };
        let builtin = match scanner.builtin_dirs(exe, &info) {
            Ok(d) => d,
            Err(e) => {
                warnings.push(format!("include scanning: {e}; using sysroot defaults"));
                let mut v = Vec::new();
                if let Some(sr) = &info.sysroot {
                    for sub in ["usr/local/include", "usr/include", "include"] {
                        v.push(sr.join(sub));
                    }
                }
                Arc::new(v)
            }
        };
        let mut sp = search_path(&info, &builtin);
        sp.msvc = scanner.msvc_lookup(exe, &info);
        // Where forced includes are depends on it.
        if sp.msvc != info.msvc_lookup {
            info.place_forced_includes(sp.msvc);
        }
        // The variables naming include directories the compiler reads.
        if info.style == Some(Style::Msvc) {
            let tool = Tool::of_step(exe, argv, &effective, first_style, windows, &present, None);
            for inc in tool.include_dir_vars().filter_map(std::env::var_os) {
                sp.angled.extend(std::env::split_paths(&inc).filter(|p| !p.as_os_str().is_empty()).map(|p| scanner::normalize(&build_dir.join(p))));
            }
        }
        let mut roots = info.sources.clone();
        roots.extend(info.forced_includes.iter().cloned());
        let r = scanner.scan_compile(&roots, &sp, &info.macros);
        if let Some(why) = r.local {
            return Err(why);
        }
        let sent = scanner.computed_files(&r, &sp, &|d| shared.written.contains(d), &|n| scanner.predefined(exe, &info, n))?;
        files.extend(r.files);
        files.extend(sent);
        warnings.extend(r.warnings);
        files.extend(info.extra_files);
        whole_dirs.extend(info.extra_dirs);
    }
    // Declared inputs (explicit, implicit, and deps found by earlier builds).
    // Outputs of remote actions that weren't downloaded go in by digest.
    let mut remote_inputs: Vec<(PathBuf, crate::remote_outputs::Entry, String)> = Vec::new();
    for i in &task.inputs {
        let p = scanner::normalize(&build_dir.join(i));
        if p.is_file() {
            files.push(p);
        } else if let Some(e) = shared.remote_outputs.as_ref().and_then(|r| r.get(i)) {
            remote_inputs.push((p, e, i.clone()));
        }
    }
    // So do files the scan found that only exist remotely (a precompiled
    // header in `-Xclang -include-pch`).
    if let Some(outputs) = &shared.remote_outputs {
        files.retain(|f| {
            if remote_inputs.iter().any(|(p, _, _)| p == f) {
                return false;
            }
            if f.exists() {
                return true;
            }
            let Ok(rel) = f.strip_prefix(build_dir) else { return true };
            let rel = rel.to_string_lossy().replace('\\', "/");
            match outputs.get(&rel) {
                Some(e) => {
                    remote_inputs.push((f.clone(), e, rel));
                    false
                }
                None => true,
            }
        });
    }
    // Thin archives bring their members, which aren't declared inputs (a
    // compile reads none).
    if style.is_none() || driver_link {
        let members = crate::archive::expand(&task.inputs, build_dir, shared.remote_outputs.as_deref(), &shared.store, crate::archive::Outside::Refuse)?;
        for m in members {
            let p = scanner::normalize(&build_dir.join(&m));
            if p.is_file() {
                files.push(p);
            } else if let Some(e) = shared.remote_outputs.as_ref().and_then(|r| r.get(&m)) {
                let i = e.path.clone();
                remote_inputs.push((p, e, i));
            } else {
                return Err(format!("thin archive member {m} is missing"));
            }
        }
    }

    // Toolchain inputs are sent whole; without a declaration, just the
    // executables.
    let mut toolchain_dirs: Vec<PathBuf> = Vec::new();
    for tc in &toolchains {
        for i in &tc.inputs {
            let p = PathBuf::from(i);
            if p.is_dir() {
                toolchain_dirs.push(scanner::normalize(&p));
            } else if p.is_file() {
                files.push(scanner::normalize(&p));
            } else {
                warnings.push(format!("toolchain input {i} does not exist"));
            }
        }
    }
    files.extend(exes.iter().map(|e| scanner::normalize(e)));
    // The link's: its linker and files, and the toolchain's library
    // directories, whole (outside the build directory once links are
    // resolved: the build's own libraries are declared inputs).
    if let Some(link) = &link {
        files.extend(link.linker.iter().cloned());
        files.extend(link.files.iter().cloned());
        let build_canonical = std::fs::canonicalize(build_dir).ok();
        for d in &link.lib_dirs {
            let fixed = match (std::fs::canonicalize(d), &build_canonical) {
                (Ok(c), Some(b)) => !c.starts_with(b),
                _ => false,
            };
            if fixed {
                toolchain_dirs.push(d.clone());
            }
        }
    }

    // Exec root.
    let exec_root = match &shared.exec_root {
        Some(r) => ExecRoot::fixed(r, windows),
        None => {
            let mut all: Vec<String> = vec![normalize_abs(&build_dir_s, windows)];
            for f in files.iter().chain(&toolchain_dirs).chain(&whole_dirs).chain(&rsp_files).chain(remote_inputs.iter().map(|(p, _, _)| p)) {
                all.push(normalize_abs(&to_s(f), windows));
            }
            ExecRoot::common(&all, windows)
        }
    };
    let mut mapper = PathMapper::new(exec_root, &build_dir_s, windows)?;

    // Input tree.
    let store = &shared.store;
    let mut tree = InputTree::new();
    let mut blobs = BlobSet::default();
    let mut tree_files: Vec<String> = Vec::new();
    // Grafted toolchain dirs, as comparison keys. `Path::starts_with` is
    // case-sensitive, but Windows paths are not: the compiler path is
    // canonicalized (true on-disk case) while toolchain dirs keep the case
    // they were declared with.
    let mut grafted: Vec<String> = Vec::new();
    let path_key = |p: &Path| {
        let s = to_s(p).replace('\\', "/");
        let s = if windows { s.to_ascii_lowercase() } else { s };
        s.trim_end_matches('/').to_string()
    };
    let is_under = |p: &Path, dir_key: &str| {
        let k = path_key(p);
        k == dir_key || k.strip_prefix(dir_key).is_some_and(|rest| rest.starts_with('/'))
    };
    toolchain_dirs.sort();
    toolchain_dirs.dedup();
    for d in &toolchain_dirs {
        // Nested toolchain dirs are covered by their parent.
        if grafted.iter().any(|g| is_under(d, g)) {
            continue;
        }
        let rel = mapper.tree_path(&to_s(d)).ok_or_else(|| format!("{} is outside the exec root", d.display()))?;
        let from_daemon = |dir: &Path| {
            shared.daemon().and_then(|c| match c.tree(dir) {
                Ok(t) => Some(t),
                Err(e) => {
                    shared.daemon_failed(&e);
                    None
                }
            })
        };
        let t = store.toolchain_tree(d, &from_daemon).map_err(|e| format!("toolchain input {}: {e:#}", d.display()))?;
        tree.add_subtree(&rel, t.digest.clone()).map_err(|e| format!("{e:#}"))?;
        for (h, (dg, src)) in t.blobs.0.iter() {
            blobs.0.entry(h.clone()).or_insert_with(|| (dg.clone(), src.clone()));
        }
        // Index the toolchain files for mapping remote paths back.
        tree_files.extend(t.files.iter().map(|f| if rel.is_empty() { f.clone() } else { format!("{rel}/{f}") }));
        grafted.push(path_key(d));
    }
    let under_graft = |p: &Path| grafted.iter().any(|g| is_under(p, g));
    whole_dirs.sort();
    whole_dirs.dedup();
    for d in &whole_dirs {
        if under_graft(d) {
            continue;
        }
        let mut list = Vec::new();
        crate::store::walk_files(d, "", &mut list);
        files.extend(list.iter().map(|f| d.join(f)));
    }
    // Response files get rewritten contents.
    let task_rsp = task.rspfile.as_ref().map(|(p, c)| (scanner::normalize(&build_dir.join(p)), c.clone()));
    let mut rsp_rewritten: Vec<(PathBuf, String)> = Vec::new();
    for r in &rsp_files {
        let content = match &task_rsp {
            Some((p, c)) if p == r => c.clone(),
            _ => std::fs::read(r).map(|b| decode_rsp(&b)).map_err(|e| format!("reading {}: {e}", r.display()))?,
        };
        rsp_rewritten.push((r.clone(), content));
    }

    files.sort();
    files.dedup();
    let mut seen_keys = std::collections::HashSet::new();
    let mut input_files = 0;
    let mut inputs: Vec<(String, PathBuf)> = Vec::new();
    for f in &files {
        if under_graft(f) || rsp_files.contains(f) {
            continue;
        }
        let key = if windows { to_s(f).to_ascii_lowercase().replace('\\', "/") } else { to_s(f) };
        if !seen_keys.insert(key) {
            continue;
        }
        let rel = mapper.tree_path(&to_s(f)).ok_or_else(|| format!("{} is outside the exec root", f.display()))?;
        inputs.push((rel, f.clone()));
    }
    // A definition keeps its absolute path (see `PathMapper::rewrite_args`):
    // one naming an input (a header included through the macro) names it
    // where a remote worker doesn't have it.
    {
        let programs: std::collections::HashSet<String> = exes.iter().map(|e| path_key(&scanner::normalize(e))).collect();
        let read: std::collections::HashSet<String> = inputs.iter().map(|(_, f)| path_key(f)).filter(|k| !programs.contains(k)).collect();
        let mut rsp_args: Vec<(Vec<String>, bool)> = rsp_rewritten.iter().map(|(p, c)| (split_rsp(c, windows), cl_rsp_files.contains(p))).collect();
        // And those `CL` adds.
        rsp_args.push((env_before.iter().chain(&env_after).cloned().collect(), true));
        let args = steps.iter().zip(cl_steps.iter().copied()).chain(rsp_args.iter().map(|(a, cl)| (a, *cl)));
        for (argv, cl) in args {
            if let Some(p) =
                crate::paths::defined_paths(argv, cl, windows).into_iter().find(|p| read.contains(&path_key(Path::new(&normalize_abs(p, windows)))))
            {
                return Err(format!("a definition names the input {p} by its absolute path, which a remote worker doesn't have"));
            }
        }
    }
    // Digests: the daemon's, which knows what didn't change, or hashed here
    // (through the digest cache).
    let known = shared.daemon().and_then(|d| {
        let paths: Vec<PathBuf> = inputs.iter().map(|(_, f)| f.clone()).collect();
        d.files(&paths).map_err(|e| shared.daemon_failed(&e)).ok()
    });
    let mut local_inputs = Vec::with_capacity(inputs.len());
    for (i, (rel, f)) in inputs.into_iter().enumerate() {
        let (digest, executable, stamp) = match known.as_ref().map(|k| &k[i]) {
            Some(Ok(info)) => {
                let digest = re::Digest { hash: info.hash.clone(), size_bytes: info.size as i64 };
                // Saved with the build's digests, for a daemon that restarts.
                store.digests.put(&f, info.stamp(), digest.clone());
                (digest, info.executable, info.stamp())
            }
            _ => {
                let (meta, stamp) = ninja_os::stat(&f).map_err(|e| format!("stat {}: {e}", f.display()))?;
                (digest_for(&f, stamp, &store.digests).map_err(|e| format!("{e:#}"))?, ninja_os::is_executable(&meta), stamp)
            }
        };
        tree.add_file_with_digest(&rel, &f, digest.clone(), executable).map_err(|e| format!("{e:#}"))?;
        local_inputs.push((f, stamp));
        tree_files.push(rel);
        input_files += 1;
    }
    for (p, e, _) in &remote_inputs {
        let rel = mapper.tree_path(&to_s(p)).ok_or_else(|| format!("{} is outside the exec root", p.display()))?;
        tree.add_remote_file(&rel, e.digest.clone(), e.executable, p).map_err(|e| format!("{e:#}"))?;
        tree_files.push(rel);
        input_files += 1;
    }
    // On Windows hosts, relative paths naming inputs are sent with forward
    // slashes, which Windows tools accept: an archive's member names (the
    // paths it was given) then don't depend on the host.
    let input_keys: std::collections::HashSet<String> =
        if windows { task.inputs.iter().map(|i| i.replace('\\', "/").to_ascii_lowercase()).collect() } else { Default::default() };
    let slashed = |a: String| -> String {
        if windows && a.contains('\\') && input_keys.contains(&a.replace('\\', "/").to_ascii_lowercase()) { a.replace('\\', "/") } else { a }
    };
    // Rewrite response files now that the tree's top-level entries are known.
    let rsp_bytes = |mapper: &mut PathMapper, path: &PathBuf, content: &str| {
        let args: Vec<String> = mapper.rewrite_args(&split_rsp(content, windows), cl_rsp_files.contains(path)).into_iter().map(slashed).collect();
        join_rsp(&args, windows).into_bytes()
    };
    let mut rsp_files = Vec::new();
    for (path, content) in &rsp_rewritten {
        let rel = mapper.tree_path(&to_s(path)).ok_or_else(|| format!("{} is outside the exec root", path.display()))?;
        let bytes = rsp_bytes(&mut mapper, path, content);
        tree.add_bytes(&rel, bytes.clone(), false).map_err(|e| format!("{e:#}"))?;
        rsp_files.push((path.clone(), bytes));
        tree_files.push(rel);
    }
    let work_dir = mapper.work_dir.clone();
    tree.ensure_dir(&work_dir).map_err(|e| format!("{e:#}"))?;

    // Command line: one command, or a chain run by the shell.
    let command_line = |mapper: &mut PathMapper| {
        let mut steps_run: Vec<Vec<String>> = Vec::new();
        for ((argv, exe), cl) in steps.iter().zip(&exes).zip(&cl_steps) {
            let mut arguments: Vec<String> = mapper.rewrite_args(argv, *cl).into_iter().map(slashed).collect();
            if let Some(p) = program_arg(mapper, exe, windows) {
                arguments[0] = p;
            }
            steps_run.push(arguments);
        }
        if steps_run.len() == 1 { steps_run.pop().unwrap() } else { vec!["/bin/sh".to_string(), "-c".to_string(), join_posix_chain(&steps_run)] }
    };
    let arguments = command_line(&mut mapper);
    // The variables its tools read (see `env`).
    let tools: Vec<Tool> = steps
        .iter()
        .zip(&exes)
        .enumerate()
        .map(|(i, (argv, exe))| {
            let style = scanner::compiler_style(&to_s(exe), argv);
            let argv_read = if i == 0 { &effective } else { argv };
            // A driver's link: the linker its dry run named.
            let linker = link.as_ref().filter(|_| i == 0).and_then(|l| l.linker.as_deref());
            Tool::of_step(exe, argv, argv_read, style, windows, &present, linker)
        })
        .collect();
    let (env, env_refused): (BTreeMap<String, String>, Option<String>) = crate::env::action_env(&tools, &mut mapper, &host_var, &shared.action_env);
    // A local run (a disk cache miss, racing, a fallback) runs from the build
    // directory, which can't reach another volume's files by relative
    // paths: those keep the host's.
    let local = if mapper.spans_volumes() {
        let mut host = mapper.for_host();
        let arguments = command_line(&mut host);
        let rsp_files = rsp_rewritten.iter().map(|(path, content)| (path.clone(), rsp_bytes(&mut host, path, content))).collect();
        let env = crate::env::action_env(&tools, &mut host, &host_var, &shared.action_env).0.into_iter().collect();
        Some(LocalForm { arguments, rsp_files, env })
    } else {
        None
    };

    // Outputs.
    let mut outputs = Vec::new();
    for o in &task.outputs {
        let rel = output_rel(&mut mapper, o)?;
        outputs.push((rel, scanner::normalize(&build_dir.join(o))));
    }
    let depfile = match &task.depfile {
        Some(d) => Some((output_rel(&mut mapper, d)?, scanner::normalize(&build_dir.join(d)))),
        None => None,
    };
    let mut side_outputs = Vec::new();
    for o in link.iter().flat_map(|l| &l.outputs) {
        // Outside the build directory: not returned.
        if let Ok(rel) = output_rel(&mut mapper, o)
            && !outputs.iter().any(|(r, _)| *r == rel)
        {
            side_outputs.push((rel, scanner::normalize(&build_dir.join(o))));
        }
    }
    for o in &compile_outputs {
        // Outside the build directory: the action can't return it.
        let rel = output_rel(&mut mapper, o)?;
        if !outputs.iter().chain(&side_outputs).chain(&depfile).any(|(r, _)| *r == rel) {
            side_outputs.push((rel, scanner::normalize(&build_dir.join(o))));
        }
    }
    let mut output_paths: Vec<String> = outputs.iter().chain(&side_outputs).map(|(r, _)| r.clone()).collect();
    if let Some((r, _)) = &depfile {
        output_paths.push(r.clone());
    }
    output_paths.sort();
    output_paths.dedup();

    let props: Vec<(String, String)> = toolchain.as_ref().map(|t| t.platform.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
    let platform = rbe_client::platform(&props);
    let env_vec: Vec<(String, String)> = env.into_iter().collect();
    #[allow(deprecated)]
    let command = re::Command {
        arguments,
        environment_variables: rbe_client::env_vars(&env_vec),
        output_files: output_paths.clone(),
        output_directories: vec![],
        output_paths,
        platform: Some(platform.clone()),
        working_directory: work_dir.clone(),
        output_node_properties: vec![],
        output_directory_format: 0,
    };
    let command_digest = blobs.insert_message(&command);
    let input_root_digest = tree.build(&mut blobs);
    let input_digests = if shared.log_inputs { tree.digests() } else { Vec::new() };
    let action = re::Action {
        command_digest: Some(command_digest),
        input_root_digest: Some(input_root_digest.clone()),
        timeout: None,
        do_not_cache: false,
        salt: vec![],
        platform: Some(platform),
    };
    let action_digest = blobs.insert_message(&action);
    let tree_index = TreeIndex::new(tree_files, windows);
    Ok(Prepared {
        action_digest,
        blobs,
        mapper,
        tree_index,
        outputs,
        depfile,
        warnings,
        input_files,
        local_before,
        local_after,
        side_outputs,
        remote_inputs: remote_inputs.into_iter().map(|(p, e, i)| (p, i, e.digest)).collect(),
        command,
        rsp_files,
        input_root_digest,
        input_digests,
        toolchain_declared,
        local_inputs,
        sources,
        env_refused,
        local,
    })
}

/// A file's digest for the stamp it has, through the digest cache, as
/// `rbe_client::tree::file_digest` gives it (keyed by the target of a
/// symlink): the stamp taken once, for the key and the check after a local
/// run.
fn digest_for(path: &Path, stamp: ninja_os::FileStamp, cache: &dyn DigestCache) -> anyhow::Result<re::Digest> {
    let key = digest_key(path);
    if let Some(d) = cache.get(&key, &stamp) {
        return Ok(d);
    }
    let read_at = std::time::SystemTime::now();
    let d = rbe_client::tree::hash_file(path)?;
    cache.put_read_at(&key, stamp, d.clone(), read_at);
    Ok(d)
}

/// What the digest cache keys a file by: the target of a symlink.
fn digest_key(path: &Path) -> PathBuf {
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if is_link { std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()) } else { path.to_path_buf() }
}

/// Hashes a file found changed since its digest was taken (an upload's
/// check) again, for the digest cache: its stamp may not show the change
/// (within its file system's time granularity).
pub fn rehash(path: &Path, cache: &dyn DigestCache) -> anyhow::Result<()> {
    let read_at = std::time::SystemTime::now();
    let (_, stamp) = ninja_os::stat(path)?;
    let d = rbe_client::tree::hash_file(path)?;
    cache.put_read_at(&digest_key(path), stamp, d, read_at);
    Ok(())
}

fn output_rel(mapper: &mut PathMapper, o: &str) -> Result<String, String> {
    let tree = mapper.tree_path(o).ok_or_else(|| format!("output {o} is outside the exec root"))?;
    let rel = mapper.rel_from_work_dir(&tree);
    if rel.starts_with("..") {
        return Err(format!("output {o} is outside the build directory"));
    }
    Ok(rel)
}

/// Bytes of a blob source (for tests and the fake server).
pub fn load_blob(src: &BlobSource) -> anyhow::Result<Vec<u8>> {
    src.load()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What actions of a build in `build_dir` share, without a toolchains
    /// file, a remote nor a daemon.
    fn shared(build_dir: &Path) -> Shared {
        Shared {
            build_dir: build_dir.to_path_buf(),
            toolchains: Toolchains::default(),
            exec_root: None,
            store: Store::new(),
            windows: cfg!(windows),
            remote_outputs: None,
            log_inputs: false,
            include_scanner: IncludeScanner::default(),
            verbose: false,
            daemon: None,
            daemon_error: Default::default(),
            profile_dir: None,
            volatile: vec![build_dir.to_path_buf()],
            written: crate::scanner::WrittenDirs::new(std::iter::once(build_dir.to_path_buf()), cfg!(windows)),
            action_env: Vec::new(),
            compiler_scans: CompilerScans::default(),
        }
    }

    /// A linker run directly with a response file (CMake puts the link's
    /// libraries and their directories there): what it names there (a
    /// version script, a library directory) is the link's, as the linker
    /// reads it.
    #[test]
    fn linker_response_files() {
        let dir = test_dirs::TestDir::new("ninja-rs-action-link-rsp");
        let build = dir.join("build");
        let linker = dir.join("tc").join(if cfg!(windows) { "ld.lld.exe" } else { "ld.lld" });
        for (path, contents) in [(&linker, ""), (&build.join("a.o"), "A"), (&dir.join("src/v.map"), "{};"), (&dir.join("libs/libz.a"), "Z")] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let rsp = "a.o --version-script=../src/v.map -L../libs -lz";
        std::fs::write(build.join("link.rsp"), rsp).unwrap();
        let task = Task {
            command: format!("{} @link.rsp -o app", linker.display()),
            inputs: vec!["a.o".into()],
            outputs: vec!["app".into()],
            bindings: vec![("rbe_mnemonic".into(), "CLink".into())],
            ..Default::default()
        };
        let p = prepare(&task, &shared(&build)).unwrap();
        for input in ["build/a.o", "src/v.map", "libs/libz.a"] {
            assert!(p.tree_index.contains(input, cfg!(windows)), "{input} isn't an input");
        }
    }

    /// A compile of `src/main.c` in `dir/build` by a fake GCC-style
    /// compiler whose `-dM` prints `macros`, with `flags` (in a response
    /// file, as written there), as the conservative scanner prepares it.
    fn compile(dir: &Path, compiler: &str, macros: &[&str], flags: &str) -> Result<Prepared, String> {
        let build = dir.join("build");
        std::fs::create_dir_all(&build).unwrap();
        let cc = crate::scanner::tests::fake_compiler(dir, compiler, macros);
        std::fs::write(build.join("flags.rsp"), flags).unwrap();
        let task = Task {
            command: format!("\"{}\" @flags.rsp -c ../src/main.c -o main.o", cc.display()),
            outputs: vec!["main.o".into()],
            bindings: vec![("rbe_mnemonic".into(), "CCompile".into())],
            ..Default::default()
        };
        let mut shared = shared(&build);
        shared.include_scanner = IncludeScanner::Conservative;
        prepare(&task, &shared)
    }

    fn tree(name: &str, files: &[(&str, &str)]) -> test_dirs::TestDir {
        let dir = test_dirs::TestDir::new(name);
        for (f, text) in files {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), text).unwrap();
        }
        dir
    }

    /// A computed include of a macro the command line defines as a header
    /// name (mbedTLS's `MBEDTLS_CONFIG_FILE`) sends it, also where the
    /// files define a default; one it defines otherwise runs locally.
    #[test]
    fn command_line_header_macros() {
        let build_info = "#if defined(MBEDTLS_CONFIG_FILE)\n#include MBEDTLS_CONFIG_FILE\n#else\n#include \"mbedtls/mbedtls_config.h\"\n#endif\n\
                          #ifndef USER_FILE\n#define USER_FILE \"default.h\"\n#endif\n#include USER_FILE\n";
        let dir = tree(
            "ninja-rs-action-cmdline-macros",
            &[
                ("src/main.c", "#include \"mbedtls/build_info.h\"\n"),
                ("include/mbedtls/build_info.h", build_info),
                ("include/mbedtls/mbedtls_config.h", ""),
                ("include/mbedtls/default.h", ""),
                ("config/my_config.h", ""),
                ("config/user.h", ""),
            ],
        );
        let flags = "-I../include -I../config -DMBEDTLS_CONFIG_FILE=\\\"my_config.h\\\" \"-DUSER_FILE=<user.h>\"";
        let p = compile(&dir, "gcc", &[], flags).unwrap();
        for input in ["config/my_config.h", "config/user.h", "include/mbedtls/build_info.h", "src/main.c"] {
            assert!(p.tree_index.contains(input, cfg!(windows)), "{input} isn't an input");
        }
        let e = compile(&dir, "gcc", &[], "-I../include -DMBEDTLS_CONFIG_FILE=XSTR(my_config.h)").err().unwrap();
        assert!(e.contains("MBEDTLS_CONFIG_FILE"), "{e}");
    }

    /// The compiler says it searches includers' directories (Clang for the
    /// MSVC ABI, by its name) where the flags don't: a forced include next
    /// to the source is found there.
    #[test]
    fn forced_includes_where_the_compiler_says() {
        let dir = tree("ninja-rs-action-forced", &[("src/main.c", ""), ("src/pre.h", "/*src*/\n"), ("inc/pre.h", "/*inc*/\n")]);
        let p = compile(&dir, "x86_64-pc-windows-msvc-clang", &["__clang__", "_MSC_VER"], "--target=x86_64-linux-gnu -include pre.h -I../inc").unwrap();
        for input in ["src/pre.h", "inc/pre.h"] {
            assert!(p.tree_index.contains(input, cfg!(windows)), "{input} isn't an input");
        }
    }

    /// A fake Clang in `dir`: it predefines `__has_include`, searches
    /// nothing built in, and reads `src/main.c` and `inc/a.h` (`-M`), unless
    /// `fail-deps` is next to it. On Unix, run once before it's returned
    /// (see `cpp_deps::test_tool`).
    fn fake_clang(dir: &Path) -> PathBuf {
        if cfg!(windows) {
            let c = dir.join("clang.cmd");
            let batch = "@echo off\r\nset \"A= %* \"\r\n\
                         if not \"%A: -v =%\"==\"%A%\" (echo #include ^<...^> search starts here: 1>&2& echo End of search list. 1>&2)\r\n\
                         if not \"%A: -dM =%\"==\"%A%\" (echo #define __GNUC__ 4& echo #define NINJA_RS_OPERATOR___has_include 1)\r\n\
                         if not \"%A: -MD =%\"==\"%A%\" if exist \"%~dp0fail-deps\" exit /b 1\r\n\
                         if not \"%A: -MD =%\"==\"%A%\" (echo main.o: ../src/main.c ../inc/a.h& exit /b 0)\r\n\
                         if not \"%A: -M =%\"==\"%A%\" echo -:\r\nexit /b 0\r\n";
            std::fs::write(&c, batch).unwrap();
            return c;
        }
        let c = dir.join("clang");
        let sh = "#!/bin/sh\ncase \" $* \" in *\" -v \"*) printf '#include <...> search starts here:\\nEnd of search list.\\n' >&2;; esac\n\
                  case \" $* \" in *\" -dM \"*) printf '#define __GNUC__ 4\\n#define NINJA_RS_OPERATOR___has_include 1\\n';; esac\n\
                  case \" $* \" in *\" -MD \"*) [ -e \"$(dirname \"$0\")/fail-deps\" ] && exit 1; echo 'main.o: ../src/main.c ../inc/a.h'; exit 0;; esac\n\
                  case \" $* \" in *\" -M \"*) echo '-:';; esac\nexit 0\n";
        std::fs::write(&c, sh).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&c, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // (Another test's fork may hold it open for writing a moment.)
        for _ in 0..1000 {
            match std::process::Command::new(&c).arg("-Q").status() {
                Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(std::time::Duration::from_millis(5)),
                r => {
                    r.unwrap();
                    return c;
                }
            }
        }
        panic!("{} stays busy", c.display());
    }

    /// The preprocessor's scanner leaves what it can't tell exactly to the
    /// compiler (`-M`), with what the compile looks for that the compiler
    /// doesn't list (`__has_include`); that one, when the compiler fails,
    /// to the conservative scanner (which takes both branches).
    #[test]
    fn scanner_fallbacks() {
        let dir = test_dirs::TestDir::new("ninja-rs-action-scanner-fallbacks");
        let build = dir.join("build");
        let compiler = fake_clang(&dir);
        let main = "#if __has_include(\"probe.h\")\n#endif\n#if __COUNTER__ == 0\n#include \"a.h\"\n#else\n#include \"b.h\"\n#endif\n";
        for (path, contents) in [("src/main.c", main), ("src/probe.h", ""), ("inc/a.h", ""), ("inc/b.h", "")] {
            std::fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
            std::fs::write(dir.join(path), contents).unwrap();
        }
        std::fs::create_dir_all(&build).unwrap();
        let task = Task {
            command: format!("{} -I../inc -c ../src/main.c -o main.o", compiler.display()),
            inputs: vec!["../src/main.c".into()],
            outputs: vec!["main.o".into()],
            bindings: vec![("rbe_mnemonic".into(), "CCompile".into())],
            ..Default::default()
        };
        let shared = shared(&build);
        let inputs = |p: &Prepared| ["src/main.c", "src/probe.h", "inc/a.h", "inc/b.h"].map(|f| p.tree_index.contains(f, cfg!(windows)));
        let p = prepare(&task, &shared).unwrap();
        assert_eq!(inputs(&p), [true, true, true, false]);
        let summary = shared.compiler_scans.summary().unwrap_or_default();
        assert!(summary.starts_with("1 compiles'") && summary.contains("__COUNTER__"), "{summary}");
        assert_eq!(shared.store.scan.fallback_summary(), None);
        std::fs::write(dir.join("fail-deps"), "").unwrap();
        let p = prepare(&task, &shared).unwrap();
        assert_eq!(inputs(&p), [true, true, true, true]);
        assert!(shared.store.scan.fallback_summary().is_some());
        // (Not one the compiler listed.)
        assert_eq!(shared.compiler_scans.summary(), Some(summary));
    }
}
