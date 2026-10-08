//! Turns a ninja task into a REAPI `Action`: command unwrapping, include
//! scanning, exec root selection, path rewriting and the Merkle input root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ninja_core::runner::Task;
use rbe_client::proto as re;
use rbe_client::{BlobSet, BlobSource, InputTree};

use crate::cmdline::{join_rsp, split_rsp, unwrap_command};
use crate::paths::{ExecRoot, PathMapper, TreeIndex, normalize_abs};
use crate::scanner::{self, Scanner, Style, decode_rsp, expand_response_files, parse_compile, search_path};
use crate::store::Store;
use crate::toolchains::{Toolchains, long_path, resolve_program};

/// Per-build settings shared by all actions.
pub struct Shared {
    pub build_dir: PathBuf,
    pub toolchains: Toolchains,
    pub exec_root: Option<String>,
    pub store: Arc<Store>,
    pub windows: bool,
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
}

fn to_s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Prepares the REAPI action for `task`. Errors mean "run it locally".
pub fn prepare(task: &Task, shared: &Shared) -> Result<Prepared, String> {
    let windows = shared.windows;
    let build_dir = &shared.build_dir;
    let build_dir_s = to_s(build_dir);
    let unwrapped = unwrap_command(&task.command, windows, &task.outputs, Some(&build_dir_s))?;
    let argv = unwrapped.argv;
    let exe = resolve_program(&argv[0], build_dir, windows).ok_or_else(|| format!("cannot find program '{}'", argv[0]))?;
    let exe = long_path(&exe);
    let toolchain = shared.toolchains.find(&exe).cloned();
    let mut warnings = Vec::new();

    let style = scanner::compiler_style(&exe.to_string_lossy(), &argv);
    let is_compile = task.binding("mnemonic").is_some_and(|m| m.ends_with("Compile"));
    if is_compile && style.is_none() {
        return Err(format!("unrecognized compiler '{}'", argv[0]));
    }

    // Files and directories the action reads.
    let mut files: Vec<PathBuf> = Vec::new();
    let mut whole_dirs: Vec<PathBuf> = Vec::new();
    let mut rsp_files: Vec<PathBuf> = Vec::new();
    let (expanded, rsps) = expand_response_files(&argv, build_dir, windows);
    rsp_files.extend(rsps);
    if style.is_some() {
        let info = parse_compile(&expanded, build_dir);
        let scanner = Scanner { cache: &shared.store.scan, windows, volatile_roots: vec![build_dir.clone()] };
        let builtin = match scanner.builtin_dirs(&exe, &info) {
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
    for i in &task.inputs {
        let p = scanner::normalize(&build_dir.join(i));
        if p.is_file() {
            files.push(p);
        }
    }

    // Toolchain inputs are sent whole; without a declaration, just the
    // executable.
    let mut toolchain_dirs: Vec<PathBuf> = Vec::new();
    match &toolchain {
        Some(tc) if !tc.inputs.is_empty() => {
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
            files.push(scanner::normalize(&exe));
        }
        _ => files.push(scanner::normalize(&exe)),
    }

    // Exec root.
    let exec_root = match &shared.exec_root {
        Some(r) => ExecRoot::fixed(r, windows),
        None => {
            let mut all: Vec<String> = vec![normalize_abs(&build_dir_s, windows)];
            for f in files.iter().chain(&toolchain_dirs).chain(&whole_dirs).chain(&rsp_files) {
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
        let t = store.toolchain_tree(d).map_err(|e| format!("toolchain input {}: {e:#}", d.display()))?;
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
    for f in &files {
        if under_graft(f) || rsp_files.contains(f) {
            continue;
        }
        let key = if windows { to_s(f).to_ascii_lowercase().replace('\\', "/") } else { to_s(f) };
        if !seen_keys.insert(key) {
            continue;
        }
        let rel = mapper.tree_path(&to_s(f)).ok_or_else(|| format!("{} is outside the exec root", f.display()))?;
        tree.add_file_cached(&rel, f, Some(&store.digests)).map_err(|e| format!("{e:#}"))?;
        tree_files.push(rel);
        input_files += 1;
    }
    // Rewrite response files now that the tree's top-level entries are known.
    for (path, content) in rsp_rewritten {
        let rel = mapper.tree_path(&to_s(&path)).ok_or_else(|| format!("{} is outside the exec root", path.display()))?;
        let args: Vec<String> = split_rsp(&content, windows).iter().map(|a| mapper.rewrite_arg(a)).collect();
        tree.add_bytes(&rel, join_rsp(&args, windows).into_bytes(), false).map_err(|e| format!("{e:#}"))?;
        tree_files.push(rel);
    }
    let work_dir = mapper.work_dir.clone();
    tree.ensure_dir(&work_dir).map_err(|e| format!("{e:#}"))?;

    // Command line.
    let mut arguments: Vec<String> = argv.iter().map(|a| mapper.rewrite_arg(a)).collect();
    if let Some(rel) = mapper.tree_path(&to_s(&exe)) {
        let mut p = mapper.rel_from_work_dir(&rel);
        if !p.contains('/') {
            p = format!("./{p}");
        }
        if windows {
            p = p.replace('/', "\\");
        }
        arguments[0] = p;
    }
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
    let mut output_paths: Vec<String> = outputs.iter().map(|(r, _)| r.clone()).collect();
    if let Some((r, _)) = &depfile {
        output_paths.push(r.clone());
    }
    output_paths.sort();
    output_paths.dedup();

    let props: Vec<(String, String)> = toolchain
        .as_ref()
        .map(|t| t.platform.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
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
    let action = re::Action {
        command_digest: Some(command_digest),
        input_root_digest: Some(input_root_digest),
        timeout: None,
        do_not_cache: false,
        salt: vec![],
        platform: Some(platform),
    };
    let action_digest = blobs.insert_message(&action);
    let tree_index = TreeIndex::new(tree_files, windows);
    Ok(Prepared { action_digest, blobs, mapper, tree_index, outputs, depfile, warnings, input_files })
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
