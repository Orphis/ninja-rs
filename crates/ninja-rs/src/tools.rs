//! `-t` subtools (a subset of ninja's).

use std::collections::BTreeSet;

use ninja_core::build_log::BuildLog;
use ninja_core::canon::canonicalize_path;
use ninja_core::deps_log::DepsLog;
use ninja_core::disk::{Disk, RealDisk};
use ninja_core::graph::{EdgeId, NodeId, ROOT_SCOPE, State};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// Run after parsing the command line.
    AfterFlags,
    /// Run after loading build.ninja.
    AfterLoad,
    /// Run after loading the build and deps logs.
    AfterLogs,
}

pub struct ToolInfo {
    pub name: &'static str,
    pub desc: &'static str,
    pub when: When,
}

pub const TOOLS: &[ToolInfo] = &[
    ToolInfo { name: "clean", desc: "clean built files", when: When::AfterLoad },
    ToolInfo { name: "cleandead", desc: "clean built files that are no longer produced by the manifest", when: When::AfterLogs },
    ToolInfo { name: "commands", desc: "list all commands required to rebuild given targets", when: When::AfterLoad },
    ToolInfo { name: "deps", desc: "show dependencies stored in the deps log", when: When::AfterLogs },
    ToolInfo { name: "inputs", desc: "list all inputs required to rebuild given targets", when: When::AfterLoad },
    ToolInfo { name: "query", desc: "show inputs/outputs for a path", when: When::AfterLogs },
    ToolInfo { name: "recompact", desc: "recompacts ninja-internal data structures", when: When::AfterLogs },
    ToolInfo { name: "restat", desc: "restats all outputs in the build log", when: When::AfterFlags },
    ToolInfo { name: "rules", desc: "list all rules", when: When::AfterLoad },
    ToolInfo { name: "targets", desc: "list targets by their rule or depth in the DAG", when: When::AfterLoad },
];

pub fn find_tool(name: &str) -> Option<&'static ToolInfo> {
    TOOLS.iter().find(|t| t.name == name)
}

pub fn list_tools() -> String {
    let mut s = String::from("ninja-rs subtools:\n");
    for t in TOOLS {
        s.push_str(&format!("{:>11}  {}\n", t.name, t.desc));
    }
    s
}

pub struct ToolCtx<'a> {
    pub state: &'a mut State,
    pub build_log: &'a mut BuildLog,
    pub deps_log: &'a mut DepsLog,
    pub build_log_path: String,
    pub dry_run: bool,
    pub verbose: bool,
    pub quiet: bool,
}

fn error(msg: &str) {
    eprintln!("ninja: error: {msg}");
}

/// Resolves a command-line target like ninja's `CollectTarget`.
pub fn collect_target(state: &State, deps_log: Option<&DepsLog>, cpath: &str) -> Result<NodeId, String> {
    if cpath.is_empty() {
        return Err("empty path".into());
    }
    let (mut path, _) = canonicalize_path(cpath);
    let first_dependent = path.ends_with('^');
    if first_dependent {
        path.pop();
    }
    let mut node = state.lookup_node(&path);
    let build_dir = state.lookup_variable(ROOT_SCOPE, "builddir");
    if node.is_none() && !build_dir.is_empty() {
        let (p, _) = canonicalize_path(&format!("{build_dir}/{path}"));
        node = state.lookup_node(&p);
        if node.is_some() {
            path = p;
        }
    }
    match node {
        Some(n) => {
            if first_dependent {
                if let Some(&e) = state.node(n).out_edges.first() {
                    return Ok(state.edge(e).outputs[0]);
                }
                // Look for the first output whose deps mention the node.
                if let Some(log) = deps_log {
                    let mut outs: Vec<_> = log.outputs().collect();
                    outs.sort_by(|a, b| a.0.cmp(b.0));
                    for (out, d) in outs {
                        if d.paths().any(|p| p == path)
                            && let Some(o) = state.lookup_node(out)
                        {
                            return Ok(o);
                        }
                    }
                }
                return Err(format!("'{path}' has no out edge"));
            }
            Ok(n)
        }
        None => {
            let mut err = format!("unknown target '{path}'");
            if path == "clean" {
                err.push_str(", did you mean 'ninja -t clean'?");
            } else if path == "help" {
                err.push_str(", did you mean 'ninja -h'?");
            } else if let Some(s) = state.spellcheck_node(&path) {
                err.push_str(&format!(", did you mean '{}'?", state.path(s)));
            }
            Err(err)
        }
    }
}

pub fn collect_targets(state: &State, deps_log: Option<&DepsLog>, args: &[String]) -> Result<Vec<NodeId>, String> {
    if args.is_empty() {
        return state.default_nodes();
    }
    args.iter().map(|a| collect_target(state, deps_log, a)).collect()
}

pub fn run_tool(name: &str, args: &[String], ctx: &mut ToolCtx) -> i32 {
    match name {
        "targets" => tool_targets(ctx.state, args),
        "commands" => tool_commands(ctx.state, args),
        "inputs" => tool_inputs(ctx.state, args),
        "rules" => tool_rules(ctx.state),
        "clean" => tool_clean(ctx, args),
        "cleandead" => tool_cleandead(ctx),
        "deps" => tool_deps(ctx, args),
        "query" => tool_query(ctx, args),
        "recompact" => tool_recompact(ctx),
        _ => {
            error(&format!("unknown tool '{name}'"));
            1
        }
    }
}

fn targets_list(state: &State, nodes: &[NodeId], depth: i32, indent: usize, out: &mut String) {
    for &n in nodes {
        out.push_str(&"  ".repeat(indent));
        let node = state.node(n);
        if let Some(e) = node.in_edge {
            out.push_str(&format!("{}: {}\n", node.path, state.rule_of(e).name));
            if depth > 1 || depth <= 0 {
                targets_list(state, &state.edge(e).inputs, depth - 1, indent + 1, out);
            }
        } else {
            out.push_str(&format!("{}\n", node.path));
        }
    }
}

fn tool_targets(state: &State, args: &[String]) -> i32 {
    let mut depth = 1;
    let mut out = String::new();
    if let Some(mode) = args.first() {
        match mode.as_str() {
            "rule" => {
                match args.get(1) {
                    None => {
                        for e in &state.edges {
                            for &i in &e.inputs {
                                if state.node(i).in_edge.is_none() {
                                    out.push_str(&format!("{}\n", state.path(i)));
                                }
                            }
                        }
                    }
                    Some(rule) => {
                        let mut set = BTreeSet::new();
                        for e in &state.edges {
                            if state.rule_of(e.id).name == *rule {
                                for &o in &e.outputs {
                                    set.insert(state.path(o).to_string());
                                }
                            }
                        }
                        for s in set {
                            out.push_str(&format!("{s}\n"));
                        }
                    }
                }
                print!("{out}");
                return 0;
            }
            "depth" => depth = args.get(1).and_then(|d| d.parse().ok()).unwrap_or(0),
            "all" => {
                for e in &state.edges {
                    for &o in &e.outputs {
                        out.push_str(&format!("{}: {}\n", state.path(o), state.rule_of(e.id).name));
                    }
                }
                print!("{out}");
                return 0;
            }
            other => {
                error(&format!("unknown target tool mode '{other}'"));
                return 1;
            }
        }
    }
    match state.root_nodes() {
        Ok(roots) => {
            targets_list(state, &roots, depth, 0, &mut out);
            print!("{out}");
            0
        }
        Err(e) => {
            error(&e);
            1
        }
    }
}

fn print_commands(state: &State, edge: Option<EdgeId>, seen: &mut BTreeSet<EdgeId>, all: bool, out: &mut String) {
    let Some(e) = edge else { return };
    if !seen.insert(e) {
        return;
    }
    if all {
        for &i in &state.edge(e).inputs {
            print_commands(state, state.node(i).in_edge, seen, all, out);
        }
    }
    if !state.is_phony(e) {
        out.push_str(&state.evaluate_command(e, false).unwrap_or_default());
        out.push('\n');
    }
}

fn tool_commands(state: &State, args: &[String]) -> i32 {
    let mut all = true;
    let mut rest = Vec::new();
    for a in args {
        match a.as_str() {
            "-s" => all = false,
            "-h" => {
                println!("usage: ninja -t commands [options] [targets]\n\noptions:\n  -s     only print the final command to build [target], not the whole chain");
                return 1;
            }
            _ => rest.push(a.clone()),
        }
    }
    let nodes = match collect_targets(state, None, &rest) {
        Ok(n) => n,
        Err(e) => {
            error(&e);
            return 1;
        }
    };
    let mut seen = BTreeSet::new();
    let mut out = String::new();
    for n in nodes {
        print_commands(state, state.node(n).in_edge, &mut seen, all, &mut out);
    }
    print!("{out}");
    0
}

fn tool_inputs(state: &State, args: &[String]) -> i32 {
    let nodes = match collect_targets(state, None, args) {
        Ok(n) => n,
        Err(e) => {
            error(&e);
            return 1;
        }
    };
    let mut seen = BTreeSet::new();
    let mut inputs = Vec::new();
    fn visit(state: &State, n: NodeId, seen: &mut BTreeSet<NodeId>, inputs: &mut Vec<String>) {
        let Some(e) = state.node(n).in_edge else { return };
        for &i in &state.edge(e).inputs {
            if !seen.insert(i) {
                continue;
            }
            visit(state, i, seen, inputs);
            if !state.node(i).in_edge.is_some_and(|ie| state.is_phony(ie)) {
                inputs.push(state.node(i).path_decanonicalized());
            }
        }
    }
    for n in nodes {
        visit(state, n, &mut seen, &mut inputs);
    }
    inputs.sort();
    for i in inputs {
        println!("{i}");
    }
    0
}

fn tool_rules(state: &State) -> i32 {
    let mut names: Vec<&str> = state.rules.iter().filter(|r| !r.phony).map(|r| r.name.as_str()).collect();
    names.sort();
    names.dedup();
    for n in names {
        println!("{n}");
    }
    0
}

struct Cleaner<'a> {
    disk: RealDisk,
    removed: BTreeSet<String>,
    count: usize,
    status: i32,
    dry_run: bool,
    verbose: bool,
    quiet: bool,
    _p: std::marker::PhantomData<&'a ()>,
}

impl Cleaner<'_> {
    fn remove(&mut self, path: &str) {
        if !self.removed.insert(path.to_string()) {
            return;
        }
        if self.dry_run {
            if self.disk.stat(path).unwrap_or(0) > 0 {
                self.report(path);
            }
        } else {
            match self.disk.remove_file(path) {
                Ok(true) => self.report(path),
                Ok(false) => {}
                Err(e) => {
                    error(&e);
                    self.status = 1;
                }
            }
        }
    }
    fn report(&mut self, path: &str) {
        self.count += 1;
        if self.verbose {
            println!("Remove {path}");
        }
    }
    fn header(&self) {
        if self.quiet {
            return;
        }
        if self.verbose {
            println!("Cleaning...");
        } else {
            print!("Cleaning... ");
        }
    }
    fn footer(&self) {
        if !self.quiet {
            println!("{} files.", self.count);
        }
    }
    fn remove_edge_files(&mut self, state: &State, e: EdgeId) {
        let d = state.unescaped_depfile(e);
        if !d.is_empty() {
            self.remove(&d);
        }
        let r = state.unescaped_rspfile(e);
        if !r.is_empty() {
            self.remove(&r);
        }
    }
}

fn load_all_dyndeps(state: &mut State) {
    let disk = RealDisk::default();
    let pending: Vec<NodeId> = state
        .edges
        .iter()
        .filter_map(|e| e.dyndep)
        .filter(|&d| state.node(d).dyndep_pending)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for d in pending {
        if !state.node(d).dyndep_pending {
            continue;
        }
        let mut scanner = ninja_core::build::Scanner { state: &mut *state, disk: &disk, build_log: None, deps_log: None, explain: None, deps_node_cache: None };
        // Ignore errors: the dyndep file may not exist yet.
        let _ = scanner.load_dyndeps(d);
    }
}

fn tool_clean(ctx: &mut ToolCtx, args: &[String]) -> i32 {
    let mut generator = false;
    let mut clean_rules = false;
    let mut targets = Vec::new();
    for a in args {
        match a.as_str() {
            "-g" => generator = true,
            "-r" => clean_rules = true,
            "-h" => {
                println!("usage: ninja -t clean [options] [targets]\n\noptions:\n  -g     also clean files marked as ninja generator output\n  -r     interpret targets as a list of rules to clean instead");
                return 1;
            }
            _ => targets.push(a.clone()),
        }
    }
    if clean_rules && targets.is_empty() {
        error("expected a rule to clean");
        return 1;
    }
    let mut c = Cleaner {
        disk: RealDisk::default(),
        removed: BTreeSet::new(),
        count: 0,
        status: 0,
        dry_run: ctx.dry_run,
        verbose: ctx.verbose || ctx.dry_run,
        quiet: ctx.quiet,
        _p: Default::default(),
    };
    c.header();
    load_all_dyndeps(ctx.state);
    let state = &*ctx.state;
    if targets.is_empty() {
        for e in &state.edges {
            // Do not try to remove phony targets.
            if state.is_phony(e.id) {
                continue;
            }
            // Do not remove generator's files unless generator specified.
            if !generator && state.edge_binding_bool(e.id, "generator") {
                continue;
            }
            for &o in &e.outputs {
                c.remove(state.path(o));
            }
            c.remove_edge_files(state, e.id);
        }
    } else if clean_rules {
        for rule in &targets {
            if !state.rules.iter().any(|r| r.name == *rule) {
                error(&format!("unknown rule '{rule}'"));
                c.status = 1;
                continue;
            }
            for e in &state.edges {
                if state.rule_of(e.id).name == *rule {
                    for &o in &e.outputs {
                        c.remove(state.path(o));
                    }
                    c.remove_edge_files(state, e.id);
                }
            }
        }
    } else {
        for t in &targets {
            match collect_target(state, None, t) {
                Ok(n) => {
                    // Clean the target and everything it depends on.
                    let mut stack = vec![n];
                    let mut seen = BTreeSet::new();
                    while let Some(n) = stack.pop() {
                        if !seen.insert(n) {
                            continue;
                        }
                        if let Some(e) = state.node(n).in_edge {
                            if !state.is_phony(e) {
                                c.remove(state.path(n));
                                c.remove_edge_files(state, e);
                            }
                            stack.extend(state.edge(e).inputs.iter().copied());
                        }
                    }
                }
                Err(e) => {
                    error(&e);
                    c.status = 1;
                }
            }
        }
    }
    c.footer();
    c.status
}

fn tool_cleandead(ctx: &mut ToolCtx) -> i32 {
    let mut c = Cleaner {
        disk: RealDisk::default(),
        removed: BTreeSet::new(),
        count: 0,
        status: 0,
        dry_run: ctx.dry_run,
        verbose: ctx.verbose || ctx.dry_run,
        quiet: ctx.quiet,
        _p: Default::default(),
    };
    c.header();
    load_all_dyndeps(ctx.state);
    let mut outs: Vec<String> = ctx.build_log.entries().keys().cloned().collect();
    outs.sort();
    for o in outs {
        let dead = match ctx.state.lookup_node(&o) {
            None => true,
            Some(n) => {
                let node = ctx.state.node(n);
                node.in_edge.is_none() && node.out_edges.is_empty()
            }
        };
        if dead {
            c.remove(&o);
        }
    }
    c.footer();
    c.status
}

fn tool_deps(ctx: &mut ToolCtx, args: &[String]) -> i32 {
    let disk = RealDisk::default();
    let outputs: Vec<String> = if args.is_empty() {
        let mut v: Vec<String> = ctx
            .deps_log
            .outputs()
            .map(|(k, _)| k)
            .filter(|k| ctx.state.lookup_node(k).is_some_and(|n| ctx.state.node(n).in_edge.is_some()))
            .map(str::to_string)
            .collect();
        v.sort();
        v
    } else {
        match collect_targets(ctx.state, Some(ctx.deps_log), args) {
            Ok(n) => n.into_iter().map(|n| ctx.state.path(n).to_string()).collect(),
            Err(e) => {
                error(&e);
                return 1;
            }
        }
    };
    for o in outputs {
        let Some(deps) = ctx.deps_log.get(&o) else {
            println!("{o}: deps not found");
            continue;
        };
        let mtime = disk.stat(&o).unwrap_or(0);
        println!(
            "{o}: #deps {}, deps mtime {} ({})",
            deps.len(),
            deps.mtime,
            if mtime == 0 || mtime > deps.mtime { "STALE" } else { "VALID" }
        );
        for d in deps.paths() {
            println!("    {d}");
        }
        println!();
    }
    0
}

fn tool_query(ctx: &mut ToolCtx, args: &[String]) -> i32 {
    if args.is_empty() {
        error("expected a target to query");
        return 1;
    }
    for a in args {
        let n = match collect_target(ctx.state, Some(ctx.deps_log), a) {
            Ok(n) => n,
            Err(e) => {
                error(&e);
                return 1;
            }
        };
        println!("{}:", ctx.state.path(n));
        if let Some(e) = ctx.state.node(n).in_edge {
            if let Some(dd) = ctx.state.edge(e).dyndep
                && ctx.state.node(dd).dyndep_pending
            {
                let disk = RealDisk::default();
                let mut scanner =
                    ninja_core::build::Scanner { state: &mut *ctx.state, disk: &disk, build_log: None, deps_log: None, explain: None, deps_node_cache: None };
                if let Err(err) = scanner.load_dyndeps(dd) {
                    eprintln!("ninja: warning: {err}");
                }
            }
            let state = &*ctx.state;
            let edge = state.edge(e);
            println!("  input: {}", state.rule_of(e).name);
            for (idx, &i) in edge.inputs.iter().enumerate() {
                let label = if edge.is_implicit(idx) {
                    "| "
                } else if edge.is_order_only(idx) {
                    "|| "
                } else {
                    ""
                };
                println!("    {label}{}", state.path(i));
            }
            if !edge.validations.is_empty() {
                println!("  validations:");
                for &v in &edge.validations {
                    println!("    {}", state.path(v));
                }
            }
        }
        let state = &*ctx.state;
        println!("  outputs:");
        for &e in &state.node(n).out_edges {
            for &o in &state.edge(e).outputs {
                println!("    {}", state.path(o));
            }
        }
        let ve = &state.node(n).validation_out_edges;
        if !ve.is_empty() {
            println!("  validation for:");
            for &e in ve {
                for &o in &state.edge(e).outputs {
                    println!("    {}", state.path(o));
                }
            }
        }
    }
    0
}

fn tool_recompact(ctx: &mut ToolCtx) -> i32 {
    let state = &*ctx.state;
    let live = |p: &str| state.lookup_node(p).is_some();
    let path = ctx.build_log_path.clone();
    if let Err(e) = ctx.build_log.recompact(&path, &live) {
        error(&e);
        return 1;
    }
    let deps_path = path.replace(ninja_core::build_log::BUILD_LOG_FILE, ninja_core::deps_log::DEPS_LOG_FILE);
    if std::path::Path::new(&deps_path).exists()
        && let Err(e) = ctx.deps_log.recompact(&deps_path, &|p: &str| state.lookup_node(p).is_some_and(|n| state.node(n).in_edge.is_some()))
    {
        error(&e);
        return 1;
    }
    0
}

/// `-t restat [--builddir=DIR] [outputs]`: runs before the manifest is
/// loaded, as in ninja.
pub fn tool_restat_after_flags(args: &[String]) -> i32 {
    let mut build_dir = String::new();
    let mut outputs = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(v) = a.strip_prefix("--builddir=") {
            build_dir = v.to_string();
        } else if a == "--builddir" {
            i += 1;
            build_dir = args.get(i).cloned().unwrap_or_default();
        } else if a == "-h" || a == "--help" {
            println!("usage: ninja -t restat [--builddir=DIR] [outputs]");
            return 1;
        } else {
            outputs.push(a.clone());
        }
        i += 1;
    }
    let path = if build_dir.is_empty() {
        ninja_core::build_log::BUILD_LOG_FILE.to_string()
    } else {
        format!("{build_dir}/{}", ninja_core::build_log::BUILD_LOG_FILE)
    };
    if !std::path::Path::new(&path).exists() {
        // Nothing to restat, ignore this.
        return 0;
    }
    let mut log = BuildLog::new();
    match log.load(&path) {
        Ok(Some(w)) => eprintln!("ninja: warning: {w}"),
        Ok(None) => {}
        Err(e) => {
            error(&format!("loading build log {path}: {e}"));
            return 1;
        }
    }
    if let Err(e) = log.restat(&path, &RealDisk::default(), &outputs) {
        error(&format!("failed recompaction: {e}"));
        return 1;
    }
    0
}
