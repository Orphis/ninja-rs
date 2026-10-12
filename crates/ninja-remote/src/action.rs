//! Turns a ninja task into a REAPI `Action`: command unwrapping, include
//! scanning, exec root selection, path rewriting and the Merkle input root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use ninja_core::runner::Task;
use rbe_client::proto as re;
use rbe_client::{BlobSet, BlobSource, DigestCache, InputTree};

use crate::cmdline::{join_posix_chain, join_rsp, split_rsp, unwrap_command};
use crate::paths::{ExecRoot, PathMapper, TreeIndex, normalize_abs};
use crate::scanner::{self, Scanner, Style, decode_rsp, expand_response_files, parse_compile, search_path};
use crate::store::Store;
use crate::toolchains::{Toolchains, long_path, resolve_program};

/// How compiles' inputs are found (`--remote_include_scanner`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IncludeScanner {
    /// Following every `#include`, whatever the conditionals (`scanner`).
    #[default]
    Conservative,
    /// The `cpp-deps` preprocessor: exact for GCC and Clang commands.
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
}

fn to_s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
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
    rsp_files.extend(rsps);
    for later in &steps[1..] {
        rsp_files.extend(expand_response_files(later, build_dir, windows).1);
    }
    // A driver that compiles nothing links: its linker's inputs are found
    // from its dry run, like a linker's from its arguments.
    let driver_link = style.is_some() && !crate::link::driver_compiles(&expanded);
    // A driver without a dry run sends its declared inputs only.
    let link = if steps.len() == 1 && (driver_link || crate::link::linker_flavor(&exe.to_string_lossy()).is_some()) {
        match crate::link::analyze(exe, argv, build_dir, driver_link) {
            Ok(info) => Some(info),
            Err(e) => {
                warnings.push(format!("link inputs: {e}"));
                None
            }
        }
    } else {
        None
    };
    // The chosen scanner first; the conservative one when it can't tell.
    let mut scanned = false;
    if style.is_some() && !driver_link && steps.len() == 1 && shared.include_scanner != IncludeScanner::Conservative {
        let mut argv = expanded.clone();
        argv[0] = to_s(exe);
        let found: Result<Vec<PathBuf>, String> = match shared.include_scanner {
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
                match scanned {
                    Ok(out) if out.exact() => Ok(out.files.into_iter().chain(out.found).collect()),
                    Ok(out) => {
                        let why = out.inexact.first().cloned().or_else(|| out.unknown.first().map(|(o, a)| format!("unknown {o}({a})"))).unwrap_or_default();
                        Err(format!("not exact ({why})"))
                    }
                    Err(e) => Err(e),
                }
            }
            IncludeScanner::ClangScanDeps => cpp_deps::external::clang_scan_deps(&argv, build_dir),
            IncludeScanner::Compiler => {
                let prefix = (!task.msvc_deps_prefix.is_empty()).then_some(task.msvc_deps_prefix.as_str());
                cpp_deps::external::compiler_deps(&argv, build_dir, prefix)
            }
            IncludeScanner::Conservative => unreachable!(),
        };
        match found {
            Ok(found) => {
                files.extend(found);
                files.extend(parse_compile(&expanded, build_dir).extra_files);
                scanned = true;
            }
            Err(e) => {
                if shared.verbose {
                    warnings.push(format!("{:?} include scanner: {e}; scanning conservatively", shared.include_scanner));
                }
            }
        }
    }
    if style.is_some() && !driver_link && !scanned {
        let info = parse_compile(&expanded, build_dir);
        let scanner = Scanner { cache: &shared.store.scan, windows, volatile_roots: vec![build_dir.clone()] };
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
        if info.style == Some(Style::Msvc)
            && let Some(inc) = std::env::var_os("INCLUDE")
        {
            sp.angled.extend(std::env::split_paths(&inc).map(|p| scanner::normalize(&p)));
        }
        let mut roots = info.sources.clone();
        roots.extend(info.forced_includes.iter().cloned());
        let r = scanner.scan(&roots, &sp);
        files.extend(r.files);
        whole_dirs.extend(r.dirs);
        warnings.extend(r.warnings);
        files.extend(info.extra_files);
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
        let members = crate::archive::expand(&task.inputs, build_dir, shared.remote_outputs.as_deref(), &shared.store)?;
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
    // Digests: the daemon's, which knows what didn't change, or hashed here
    // (through the digest cache).
    let known = shared.daemon().and_then(|d| {
        let paths: Vec<PathBuf> = inputs.iter().map(|(_, f)| f.clone()).collect();
        d.files(&paths).map_err(|e| shared.daemon_failed(&e)).ok()
    });
    for (i, (rel, f)) in inputs.into_iter().enumerate() {
        match known.as_ref().map(|k| &k[i]) {
            Some(Ok(info)) => {
                let digest = re::Digest { hash: info.hash.clone(), size_bytes: info.size as i64 };
                // Saved with the build's digests, for a daemon that restarts.
                store.digests.put(&f, info.size, UNIX_EPOCH + Duration::from_nanos(info.mtime_ns), digest.clone());
                tree.add_file_with_digest(&rel, &f, digest, info.executable)
            }
            _ => tree.add_file_cached(&rel, &f, Some(&store.digests)),
        }
        .map_err(|e| format!("{e:#}"))?;
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
    let mut rsp_files = Vec::new();
    for (path, content) in rsp_rewritten {
        let rel = mapper.tree_path(&to_s(&path)).ok_or_else(|| format!("{} is outside the exec root", path.display()))?;
        let args: Vec<String> = split_rsp(&content, windows).iter().map(|a| slashed(mapper.rewrite_arg(a))).collect();
        let bytes = join_rsp(&args, windows).into_bytes();
        tree.add_bytes(&rel, bytes.clone(), false).map_err(|e| format!("{e:#}"))?;
        rsp_files.push((path, bytes));
        tree_files.push(rel);
    }
    let work_dir = mapper.work_dir.clone();
    tree.ensure_dir(&work_dir).map_err(|e| format!("{e:#}"))?;

    // Command line: one command, or a chain run by the shell.
    let mut remote_steps: Vec<Vec<String>> = Vec::new();
    for (argv, exe) in steps.iter().zip(&exes) {
        let mut arguments: Vec<String> = argv.iter().map(|a| slashed(mapper.rewrite_arg(a))).collect();
        if let Some(rel) = mapper.tree_path(&to_s(exe)) {
            let mut p = mapper.rel_from_work_dir(&rel);
            if !p.contains('/') {
                p = format!("./{p}");
            }
            if windows {
                p = p.replace('/', "\\");
            }
            arguments[0] = p;
        }
        remote_steps.push(arguments);
    }
    let arguments =
        if remote_steps.len() == 1 { remote_steps.pop().unwrap() } else { vec!["/bin/sh".to_string(), "-c".to_string(), join_posix_chain(&remote_steps)] };
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    if style == Some(Style::Msvc)
        && let Ok(inc) = std::env::var("INCLUDE")
    {
        let mapped: Vec<String> = inc.split(';').filter(|s| !s.is_empty()).map(|p| mapper.rewrite_arg(p)).collect();
        env.insert("INCLUDE".into(), mapped.join(";"));
    }

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
    })
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
