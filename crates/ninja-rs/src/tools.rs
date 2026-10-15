//! `-t` subtools (a subset of ninja's).

use std::collections::BTreeSet;
use std::io::Write;

use ninja_core::build_log::{Access, BuildLog, LOCK_FILE, LogLock};
use ninja_core::canon::canonicalize_path;
use ninja_core::deps_log::DepsLog;
use ninja_core::disk::{Disk, RealDisk};
use ninja_core::graph::{EdgeId, NodeId, ROOT_SCOPE, State};
use ninja_remote::remote_outputs::{RemoteDisk, RemoteOutputs};

// `print!`, `println!` and `eprintln!` as ninja's `printf`: output to a pipe
// its reader closed (`-t targets all | head`) or to a terminal gone is lost,
// where std's would panic. (On Unix, tools die of SIGPIPE first, as ninja's:
// see `die_on_broken_pipe`.)
macro_rules! print {
    ($($arg:tt)*) => {
        ninja_core::status::print_out(&format!($($arg)*))
    };
}
macro_rules! println {
    () => {
        ninja_core::status::print_out("\n")
    };
    ($($arg:tt)*) => {
        ninja_core::status::print_out(&format!("{}\n", format_args!($($arg)*)))
    };
}
macro_rules! eprintln {
    ($($arg:tt)*) => {
        ninja_core::status::print_err(&format!("{}\n", format_args!($($arg)*)))
    };
}

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
    ToolInfo { name: "compdb", desc: "dump JSON compilation database to stdout", when: When::AfterLoad },
    ToolInfo { name: "compdb-targets", desc: "dump JSON compilation database for a given list of targets to stdout", when: When::AfterLoad },
    ToolInfo { name: "commands", desc: "list all commands required to rebuild given targets", when: When::AfterLoad },
    ToolInfo { name: "daemon", desc: "the daemon keeping caches between builds (status, start, stop)", when: When::AfterFlags },
    ToolInfo { name: "exec", desc: "run a command whose builds take these options and report under this invocation", when: When::AfterFlags },
    ToolInfo { name: "deps", desc: "show dependencies stored in the deps log", when: When::AfterLogs },
    ToolInfo { name: "missingdeps", desc: "check deps log dependencies on generated files", when: When::AfterLogs },
    ToolInfo { name: "inputs", desc: "list all inputs required to rebuild given targets", when: When::AfterLoad },
    ToolInfo { name: "query", desc: "show inputs/outputs for a path", when: When::AfterLogs },
    ToolInfo { name: "recompact", desc: "recompacts ninja-internal data structures", when: When::AfterLogs },
    ToolInfo { name: "restat", desc: "restats all outputs in the build log", when: When::AfterFlags },
    ToolInfo { name: "rules", desc: "list all rules", when: When::AfterLoad },
    ToolInfo { name: "targets", desc: "list targets by their rule or depth in the DAG", when: When::AfterLoad },
    ToolInfo { name: "wincodepage", desc: "print the Windows code page used by ninja-rs", when: When::AfterFlags },
];

/// `-t wincodepage`: the encoding build files are read in, which CMake asks
/// on Windows before writing them (ANSI otherwise, mangling non-ASCII
/// paths). ninja-rs reads them as UTF-8 and passes commands and paths to
/// Windows as UTF-16, on every code page.
pub fn tool_wincodepage(args: &[String]) -> i32 {
    if !args.is_empty() {
        println!("usage: ninja -t wincodepage");
        return 1;
    }
    println!("Build file encoding: UTF-8");
    0
}

pub fn find_tool(name: &str) -> Option<&'static ToolInfo> {
    TOOLS.iter().find(|t| t.name == name)
}

/// ninja's message for a tool it doesn't have, with the closest one.
pub fn unknown_tool(name: &str) -> String {
    let names: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
    crate::options::did_you_mean("unknown tool", name, &names)
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
    /// How the tool may write the logs (see `Service::lock_logs`).
    pub access: Access,
    /// The records of outputs of remote actions, for the tools removing
    /// outputs: those of the outputs removed go.
    pub remote_outputs: Option<&'a RemoteOutputs>,
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
        "rules" => tool_rules(ctx.state, args),
        "compdb" => tool_compdb(ctx.state, args),
        "compdb-targets" => tool_compdb_targets(ctx.state, args),
        "missingdeps" => tool_missingdeps(ctx.state, ctx.deps_log, args),
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

/// C's `atoi`: blanks, a sign and the digits that follow; 0 without any.
fn atoi(s: &str) -> i32 {
    let s = s.trim_start();
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let n = digits.bytes().take_while(u8::is_ascii_digit).fold(0i64, |n, d| (n * 10 + i64::from(d - b'0')).min(1 << 32));
    (if negative { -n } else { n }).clamp(i32::MIN.into(), i32::MAX.into()) as i32
}

fn tool_targets(state: &State, args: &[String]) -> i32 {
    let mut depth = 1;
    let mut out = String::new();
    if let Some(mode) = args.first() {
        match mode.as_str() {
            "rule" => {
                match args.get(1).filter(|r| !r.is_empty()) {
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
            // As ninja's `atoi`: no number keeps the default, one that
            // doesn't parse is 0 (the whole tree).
            "depth" => depth = args.get(1).map_or(1, |d| atoi(d)),
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
                error(&crate::options::did_you_mean("unknown target tool mode", other, &["rule", "depth", "all"]));
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
                println!(
                    "usage: ninja -t commands [options] [targets]\n\noptions:\n  -s     only print the final command to build [target], not the whole chain"
                );
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
    let usage = || {
        println!(
            "Usage '-t inputs [options] [targets]\n\n\
List all inputs used for a set of targets, sorted in dependency order.\n\
Note that by default, results are shell escaped, and sorted alphabetically,\n\
and never include validation target paths.\n\n\
Options:\n  \
-h, --help          Print this message.\n  \
-0, --print0            Use \\0, instead of \\n as a line terminator.\n  \
-E, --no-shell-escape   Do not shell escape the result.\n  \
-d, --dependency-order  Sort results by dependency order."
        );
        1
    };
    let (mut print0, mut shell_escape, mut dependency_order) = (false, true, false);
    let mut targets = Vec::new();
    for a in args {
        match a.as_str() {
            "--print0" => print0 = true,
            "--no-shell-escape" => shell_escape = false,
            "--dependency-order" => dependency_order = true,
            "--help" => return usage(),
            s if s.starts_with('-') && s.len() > 1 && !s.starts_with("--") => {
                // Short options, possibly combined ("-0E").
                for c in s[1..].chars() {
                    match c {
                        '0' => print0 = true,
                        'E' => shell_escape = false,
                        'd' => dependency_order = true,
                        _ => return usage(),
                    }
                }
            }
            s if s.starts_with("--") => return usage(),
            _ => targets.push(a.clone()),
        }
    }
    let nodes = match collect_targets(state, None, &targets) {
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
    if shell_escape {
        inputs = inputs
            .into_iter()
            .map(|path| {
                let mut escaped = String::new();
                if cfg!(windows) {
                    ninja_core::shell::win32_escape(&path, &mut escaped);
                } else {
                    ninja_core::shell::shell_escape(&path, &mut escaped);
                }
                escaped
            })
            .collect();
    }
    if !dependency_order {
        inputs.sort();
    }
    let mut out = std::io::stdout().lock();
    for i in inputs {
        let _ = out.write_all(i.as_bytes());
        let _ = out.write_all(if print0 { b"\0" } else { b"\n" });
    }
    let _ = out.flush();
    0
}

/// As ninja: the rules of the top-level scope (the built-in `phony`
/// included, not those of subninjas), sorted; `-d` adds their description.
fn tool_rules(state: &State, args: &[String]) -> i32 {
    let mut print_description = false;
    for a in args {
        match a.as_str() {
            "-d" => print_description = true,
            _ => {
                println!("usage: ninja -t rules [options]\n\noptions:\n  -d     also print the description of the rule\n  -h     print this message");
                return 1;
            }
        }
    }
    let root = &state.scopes[ROOT_SCOPE.0 as usize];
    let mut names: Vec<(&String, &ninja_core::graph::RuleId)> = root.rules.iter().collect();
    names.sort_by(|a, b| a.0.cmp(b.0));
    for (name, id) in names {
        match state.rules[id.0 as usize].bindings.get("description") {
            Some(d) if print_description => println!("{name}: {}", d.unparse()),
            _ => println!("{name}"),
        }
    }
    0
}

/// A string in JSON, escaped as ninja's `EncodeJSONString`.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// An edge's command for the compilation database; with `expand_rspfile`,
/// an `@rspfile` (or `-f rspfile`, `--option-file=rspfile`) argument is
/// replaced by the response file's content, as ninja's `-x`.
fn compdb_command(state: &State, e: EdgeId, expand_rspfile: bool) -> String {
    let command = state.evaluate_command(e, false).unwrap_or_default();
    if !expand_rspfile {
        return command;
    }
    let rspfile = state.unescaped_rspfile(e);
    if rspfile.is_empty() {
        return command;
    }
    let Some(index) = command.find(&rspfile).filter(|&i| i > 0) else { return command };
    let at = command.as_bytes()[index - 1] == b'@';
    let dash_f = index >= 3 && command.find("-f ") == Some(index - 3);
    let option_file = index >= 14 && command.find("--option-file=") == Some(index - 14);
    let prefix = if at {
        1
    } else if dash_f {
        3
    } else if option_file {
        14
    } else {
        return command;
    };
    let content = state.edge_binding(e, "rspfile_content").unwrap_or_default().replace('\n', " ");
    let mut out = command;
    out.replace_range(index - prefix..index + rspfile.len(), &content);
    out
}

/// Whether an edge's outputs only serve as validations of other edges.
fn is_validation_only_edge(state: &State, e: EdgeId) -> bool {
    let outputs = &state.edge(e).outputs;
    !outputs.is_empty()
        && outputs.iter().all(|&o| {
            let n = state.node(o);
            !n.validation_out_edges.is_empty() && n.out_edges.is_empty()
        })
}

/// The compilation database entries of an edge: one per input, as ninja.
fn compdb_objects(out: &mut String, directory: &str, state: &State, e: EdgeId, expand_rspfile: bool) {
    let command = json_string(&compdb_command(state, e, expand_rspfile));
    let output = json_string(state.path(state.edge(e).outputs[0]));
    for (i, &input) in state.edge(e).inputs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "\n  {{\n    \"directory\": \"{}\",\n    \"command\": \"{command}\",\n    \"file\": \"{}\",\n    \"output\": \"{output}\"\n  }}",
            json_string(directory),
            json_string(state.path(input)),
        ));
    }
}

fn working_directory() -> String {
    std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default()
}

/// `-t compdb [-x] [rules]`: every edge with inputs (of the given rules),
/// in manifest order.
fn tool_compdb(state: &State, args: &[String]) -> i32 {
    let mut expand_rspfile = false;
    let mut rules: Vec<&str> = Vec::new();
    for a in args {
        match a.as_str() {
            "-x" => expand_rspfile = true,
            s if s.starts_with('-') => {
                println!("usage: ninja -t compdb [options] [rules]\n\noptions:\n  -x     expand @rspfile style response file invocations");
                return 1;
            }
            s => rules.push(s),
        }
    }
    let directory = working_directory();
    let mut out = String::from("[");
    let mut first = true;
    for i in 0..state.edges.len() {
        let e = EdgeId(i as u32);
        if state.edge(e).inputs.is_empty() || is_validation_only_edge(state, e) {
            continue;
        }
        let rule = &state.rules[state.edge(e).rule.0 as usize].name;
        // A rule named twice prints its edges twice, as ninja.
        let times = if rules.is_empty() { 1 } else { rules.iter().filter(|r| **r == rule.as_str()).count() };
        for _ in 0..times {
            if !first {
                out.push(',');
            }
            compdb_objects(&mut out, &directory, state, e, expand_rspfile);
            first = false;
        }
    }
    out.push_str("\n]\n");
    print!("{out}");
    0
}

/// `-t compdb-targets [-x] targets...`: the edges the targets need, from
/// their requisites to their dependents.
fn tool_compdb_targets(state: &State, args: &[String]) -> i32 {
    let usage = || {
        println!(
            "usage: ninja -t compdb [-hx] target [targets]\n\noptions:\n  -h     display this help message\n  -x     expand @rspfile style response file invocations"
        );
        1
    };
    let mut expand_rspfile = false;
    let mut targets = Vec::new();
    for a in args {
        match a.as_str() {
            "-x" => expand_rspfile = true,
            s if s.starts_with('-') => return usage(),
            s => targets.push(s),
        }
    }
    if targets.is_empty() {
        error("compdb-targets expects the name of at least one target");
        return usage();
    }
    // Requisites before their dependents, phony edges left out.
    fn collect(state: &State, n: NodeId, nodes: &mut BTreeSet<NodeId>, edges: &mut BTreeSet<EdgeId>, order: &mut Vec<EdgeId>) {
        if !nodes.insert(n) {
            return;
        }
        let Some(e) = state.node(n).in_edge else { return };
        if !edges.insert(e) {
            return;
        }
        for &i in &state.edge(e).inputs {
            collect(state, i, nodes, edges, order);
        }
        if !state.is_phony(e) {
            order.push(e);
        }
    }
    let (mut nodes, mut edges, mut order) = (BTreeSet::new(), BTreeSet::new(), Vec::new());
    for t in targets {
        let node = match collect_target(state, None, t) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("ninja: fatal: {e}");
                return 1;
            }
        };
        if state.node(node).in_edge.is_none() {
            eprintln!("ninja: fatal: '{}' is not a target (i.e. it is not an output of any `build` statement)", state.path(node));
            return 1;
        }
        collect(state, node, &mut nodes, &mut edges, &mut order);
    }
    let directory = working_directory();
    let mut out = String::from("[");
    let mut first = true;
    for e in order {
        if state.edge(e).inputs.is_empty() || is_validation_only_edge(state, e) {
            continue;
        }
        if !first {
            out.push(',');
        }
        compdb_objects(&mut out, &directory, state, e, expand_rspfile);
        first = false;
    }
    out.push_str("\n]\n");
    print!("{out}");
    0
}

/// `-t missingdeps [targets]`: outputs whose recorded dependencies (deps
/// log or depfile) include a file generated by an edge they have no build
/// path from, as ninja's. Exit status 3 when there are some.
fn tool_missingdeps(state: &State, deps_log: &DepsLog, args: &[String]) -> i32 {
    let nodes = match collect_targets(state, Some(deps_log), args) {
        Ok(n) => n,
        Err(e) => {
            error(&e);
            return 1;
        }
    };
    let mut s = MissingDeps::new(state, deps_log);
    for n in nodes {
        s.process_node(n);
    }
    println!("Processed {} nodes.", s.seen.len());
    if s.nodes_missing_deps.is_empty() {
        println!("No missing dependencies on generated files found.");
        return 0;
    }
    println!("Error: There are {} missing dependency paths.", s.missing_dep_path_count);
    println!(
        "{} targets had depfile dependencies on {} distinct generated inputs (from {} rules)  without a non-depfile dep path to the generator.",
        s.nodes_missing_deps.len(),
        s.generated_nodes.len(),
        s.generator_rules.len()
    );
    println!("There might be build flakiness if any of the targets listed above are built alone, or not late enough, in a clean output directory.");
    3
}

struct MissingDeps<'a> {
    state: &'a State,
    deps_log: &'a DepsLog,
    seen: std::collections::HashSet<NodeId>,
    /// (from, to) → whether `to` depends on `from` through build edges.
    paths: std::collections::HashMap<(EdgeId, EdgeId), bool>,
    missing_dep_path_count: usize,
    nodes_missing_deps: std::collections::HashSet<NodeId>,
    generated_nodes: std::collections::HashSet<NodeId>,
    generator_rules: std::collections::HashSet<u32>,
}

impl<'a> MissingDeps<'a> {
    fn new(state: &'a State, deps_log: &'a DepsLog) -> Self {
        Self {
            state,
            deps_log,
            seen: Default::default(),
            paths: Default::default(),
            missing_dep_path_count: 0,
            nodes_missing_deps: Default::default(),
            generated_nodes: Default::default(),
            generator_rules: Default::default(),
        }
    }

    fn process_node(&mut self, node: NodeId) {
        let state = self.state;
        let Some(edge) = state.node(node).in_edge else { return };
        if !self.seen.insert(node) {
            return;
        }
        for &i in &state.edge(edge).inputs {
            self.process_node(i);
        }
        let lookup = |p: &str| state.lookup_node(p).or_else(|| state.lookup_node(&canonicalize_path(p).0));
        let deps: Vec<Option<NodeId>> = if !state.edge_binding(edge, "deps").unwrap_or_default().is_empty() {
            self.deps_log.get(state.path(node)).map(|d| d.paths().map(lookup).collect()).unwrap_or_default()
        } else {
            let depfile = state.unescaped_depfile(edge);
            match std::fs::read(&depfile).ok().filter(|_| !depfile.is_empty()).and_then(|c| ninja_core::depfile::parse_depfile(&c).ok()) {
                Some(d) => d.ins.iter().map(|p| lookup(p)).collect(),
                None => Vec::new(),
            }
        };
        self.process_node_deps(node, edge, &deps);
    }

    fn process_node_deps(&mut self, node: NodeId, edge: EdgeId, deps: &[Option<NodeId>]) {
        let state = self.state;
        let mut dep_edges = BTreeSet::new();
        for &d in deps.iter().flatten() {
            // A dependency on build.ninja means "rebuild when reconfigured":
            // the generator of the build, not a missing dependency.
            if state.path(d) == "build.ninja" {
                return;
            }
            if let Some(e) = state.node(d).in_edge {
                dep_edges.insert(e);
            }
        }
        let missing: Vec<EdgeId> = dep_edges.into_iter().filter(|&de| !self.path_exists_between(de, edge)).collect();
        if missing.is_empty() {
            return;
        }
        let mut rules = BTreeSet::new();
        for e in missing {
            for &d in deps.iter().flatten() {
                if state.node(d).in_edge == Some(e) {
                    let rule = state.edge(e).rule;
                    self.generated_nodes.insert(d);
                    self.generator_rules.insert(rule.0);
                    rules.insert(rule.0);
                    println!("Missing dep: {} uses {} (generated by {})", state.path(node), state.path(d), state.rules[rule.0 as usize].name);
                }
            }
        }
        self.missing_dep_path_count += rules.len();
        self.nodes_missing_deps.insert(node);
    }

    fn path_exists_between(&mut self, from: EdgeId, to: EdgeId) -> bool {
        if let Some(&known) = self.paths.get(&(from, to)) {
            return known;
        }
        let state = self.state;
        let mut found = false;
        for &i in &state.edge(to).inputs {
            if let Some(e) = state.node(i).in_edge
                && (e == from || self.path_exists_between(from, e))
            {
                found = true;
                break;
            }
        }
        self.paths.insert((from, to), found);
        found
    }
}

struct Cleaner<'a> {
    disk: &'a dyn Disk,
    removed: BTreeSet<String>,
    count: usize,
    status: i32,
    dry_run: bool,
    verbose: bool,
    quiet: bool,
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
    /// Cleans `n` and everything it depends on, depth first like ninja. A
    /// multi-output edge's outputs are removed together.
    fn clean_target(&mut self, state: &State, n: NodeId, cleaned: &mut BTreeSet<NodeId>) {
        if let Some(e) = state.node(n).in_edge {
            // Do not try to remove phony targets.
            if !state.is_phony(e) {
                for &o in &state.edge(e).outputs {
                    self.remove(state.path(o));
                }
                self.remove_edge_files(state, e);
            }
            for &i in &state.edge(e).inputs {
                if !cleaned.contains(&i) {
                    self.clean_target(state, i, cleaned);
                }
            }
        }
        cleaned.insert(n);
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
    let pending: Vec<NodeId> =
        state.edges.iter().filter_map(|e| e.dyndep).filter(|&d| state.node(d).dyndep_pending).collect::<BTreeSet<_>>().into_iter().collect();
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
    let mut options_done = false;
    for a in args {
        if options_done || a.len() < 2 || !a.starts_with('-') {
            targets.push(a.clone());
            continue;
        }
        if a == "--" {
            options_done = true;
            continue;
        }
        // getopt("hgr"): flags may be combined.
        for f in a[1..].chars() {
            match f {
                'g' => generator = true,
                'r' => clean_rules = true,
                _ => {
                    if f != 'h' {
                        eprintln!("ninja: invalid option -- '{f}'");
                    }
                    println!(
                        "usage: ninja -t clean [options] [targets]\n\noptions:\n  -g     also clean files marked as ninja generator output\n  -r     interpret targets as a list of rules to clean instead"
                    );
                    return 1;
                }
            }
        }
    }
    if clean_rules && targets.is_empty() {
        error("expected a rule to clean");
        return 1;
    }
    let real = RealDisk::default();
    let remote = ctx.remote_outputs.map(|outputs| RemoteDisk { inner: &real, outputs, fetch: None });
    let mut c = Cleaner {
        disk: remote.as_ref().map_or(&real as &dyn Disk, |d| d),
        removed: BTreeSet::new(),
        count: 0,
        status: 0,
        dry_run: ctx.dry_run,
        verbose: ctx.verbose || ctx.dry_run,
        quiet: ctx.quiet,
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
            if c.verbose {
                println!("Rule {rule}");
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
        let mut cleaned = BTreeSet::new();
        for t in &targets {
            if t.is_empty() {
                error("failed to canonicalize '': empty path");
                c.status = 1;
                continue;
            }
            // Plain lookups, like ninja: no suggestions, no `^`.
            let (path, _) = canonicalize_path(t);
            match state.lookup_node(&path) {
                Some(n) => {
                    if c.verbose {
                        println!("Target {path}");
                    }
                    c.clean_target(state, n, &mut cleaned);
                }
                None => {
                    error(&format!("unknown target '{path}'"));
                    c.status = 1;
                }
            }
        }
    }
    c.footer();
    c.status
}

fn tool_cleandead(ctx: &mut ToolCtx) -> i32 {
    let real = RealDisk::default();
    let remote = ctx.remote_outputs.map(|outputs| RemoteDisk { inner: &real, outputs, fetch: None });
    let mut c = Cleaner {
        disk: remote.as_ref().map_or(&real as &dyn Disk, |d| d),
        removed: BTreeSet::new(),
        count: 0,
        status: 0,
        dry_run: ctx.dry_run,
        verbose: ctx.verbose || ctx.dry_run,
        quiet: ctx.quiet,
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
        println!("{o}: #deps {}, deps mtime {} ({})", deps.len(), deps.mtime, if mtime == 0 || mtime > deps.mtime { "STALE" } else { "VALID" });
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
    // Run by a build holding the lock (a generator: CMake runs it at each
    // configure), which appends to the logs meanwhile: left as they are.
    if ctx.access == Access::Shared {
        return 0;
    }
    let state = &*ctx.state;
    let live = |p: &str| crate::is_live(state, p);
    let path = ctx.build_log_path.clone();
    if let Err(e) = ctx.build_log.recompact(&path, &live) {
        error(&e);
        return 1;
    }
    let deps_path = path.replace(ninja_core::build_log::BUILD_LOG_FILE, ninja_core::deps_log::DEPS_LOG_FILE);
    if std::path::Path::new(&deps_path).exists()
        && let Err(e) = ctx.deps_log.recompact(&deps_path, &live)
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
    let path =
        if build_dir.is_empty() { ninja_core::build_log::BUILD_LOG_FILE.to_string() } else { format!("{build_dir}/{}", ninja_core::build_log::BUILD_LOG_FILE) };
    if !std::path::Path::new(&path).exists() {
        // Nothing to restat, ignore this.
        return 0;
    }
    // A build writing the log is waited for, unless it runs this (a
    // generator, which appends then): see `Service::lock_logs`.
    let lock_file = if build_dir.is_empty() { LOCK_FILE.to_string() } else { format!("{build_dir}/{LOCK_FILE}") };
    let (_lock, access) = match crate::try_lock_logs(&lock_file) {
        Ok(crate::LogsAccess::Locked(lock)) => (Some(lock), Access::Exclusive),
        Ok(crate::LogsAccess::Busy(holder)) => {
            println!("ninja: {}", crate::waiting_message(holder.as_ref()));
            let lock = LogLock::open(&lock_file).and_then(|mut lock| lock.lock(true).map(|_| lock));
            (lock.ok(), Access::Exclusive)
        }
        Ok(crate::LogsAccess::Inherited) => (None, Access::Shared),
        Err(_) => (None, Access::Exclusive),
    };
    let mut log = BuildLog::new();
    match log.load(&path) {
        Ok(Some(w)) => eprintln!("ninja: warning: {w}"),
        Ok(None) => {}
        Err(e) => {
            error(&format!("loading build log {path}: {e}"));
            return 1;
        }
    }
    if let Err(e) = log.restat(&path, &RealDisk::default(), &outputs, access) {
        error(&format!("failed recompaction: {e}"));
        return 1;
    }
    0
}
