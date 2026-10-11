//! The build graph: nodes (files), edges (build statements), rules, pools and
//! variable scopes. Mirrors ninja's `State`, `Node`, `Edge`, `Rule`, `Pool`
//! and `BindingEnv`, with arena indices instead of pointers.

use std::collections::BTreeSet;

use crate::hash::HashMap;

use crate::canon::decanonicalize;
use crate::eval::EvalString;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EdgeId(pub u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RuleId(pub u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PoolId(pub u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScopeId(pub u32);

impl NodeId {
    pub fn idx(self) -> usize {
        self.0 as usize
    }
}
impl EdgeId {
    pub fn idx(self) -> usize {
        self.0 as usize
    }
}

/// Timestamps are nanoseconds since the Unix epoch. 0 means "missing".
pub type TimeStamp = i64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existence {
    Unknown,
    Missing,
    Exists,
}

#[derive(Debug)]
pub struct Node {
    pub path: String,
    pub slash_bits: u64,
    pub in_edge: Option<EdgeId>,
    pub out_edges: Vec<EdgeId>,
    pub validation_out_edges: Vec<EdgeId>,
    /// Node only known from a depfile, deps log or dyndep file.
    pub generated_by_dep_loader: bool,
    /// The node is a dyndep file that has not been loaded yet.
    pub dyndep_pending: bool,
    // Build-time state.
    pub mtime: TimeStamp,
    pub exists: Existence,
    pub dirty: bool,
}

impl Node {
    pub fn status_known(&self) -> bool {
        self.exists != Existence::Unknown
    }
    pub fn exists(&self) -> bool {
        self.exists == Existence::Exists
    }
    pub fn path_decanonicalized(&self) -> String {
        decanonicalize(&self.path, self.slash_bits)
    }
    pub fn reset_state(&mut self) {
        self.mtime = -1;
        self.exists = Existence::Unknown;
        self.dirty = false;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisitMark {
    None,
    InStack,
    Done,
}

#[derive(Debug)]
pub struct Edge {
    pub id: EdgeId,
    pub rule: RuleId,
    pub pool: PoolId,
    /// Enclosing scope (file scope of the build statement).
    pub scope: ScopeId,
    /// The edge's own bindings, if it had any indented `key = value` lines
    /// (ninja allocates a per-edge `BindingEnv` only in that case, which
    /// affects lookup precedence).
    pub env: Option<HashMap<String, String>>,
    pub inputs: Vec<NodeId>,
    pub outputs: Vec<NodeId>,
    pub validations: Vec<NodeId>,
    pub implicit_deps: usize,
    pub order_only_deps: usize,
    pub implicit_outs: usize,
    pub dyndep: Option<NodeId>,
    // Build-time state.
    pub outputs_ready: bool,
    pub deps_loaded: bool,
    /// The discovered deps (depfile or deps log) are in `inputs`. They
    /// aren't for an edge the scan found dirty before reading them.
    pub deps_added: bool,
    pub deps_missing: bool,
    pub mark: VisitMark,
    pub command_start_time: TimeStamp,
    pub prev_elapsed_time_millis: i64,
    pub critical_path_weight: i64,
}

impl Edge {
    pub fn explicit_inputs(&self) -> &[NodeId] {
        &self.inputs[..self.inputs.len() - self.implicit_deps - self.order_only_deps]
    }
    pub fn non_order_only_inputs(&self) -> &[NodeId] {
        &self.inputs[..self.inputs.len() - self.order_only_deps]
    }
    pub fn order_only_inputs(&self) -> &[NodeId] {
        &self.inputs[self.inputs.len() - self.order_only_deps..]
    }
    pub fn explicit_outputs(&self) -> &[NodeId] {
        &self.outputs[..self.outputs.len() - self.implicit_outs]
    }
    pub fn is_implicit(&self, index: usize) -> bool {
        index >= self.inputs.len() - self.order_only_deps - self.implicit_deps && !self.is_order_only(index)
    }
    pub fn is_order_only(&self, index: usize) -> bool {
        index >= self.inputs.len() - self.order_only_deps
    }
    pub fn is_implicit_out(&self, index: usize) -> bool {
        index >= self.outputs.len() - self.implicit_outs
    }
}

#[derive(Debug)]
pub struct Rule {
    pub name: String,
    pub bindings: HashMap<String, EvalString>,
    pub phony: bool,
}

impl Rule {
    pub fn is_reserved_binding(var: &str) -> bool {
        matches!(
            var,
            "command" | "depfile" | "dyndep" | "description" | "deps" | "generator" | "pool" | "restat" | "rspfile" | "rspfile_content" | "msvc_deps_prefix"
        )
    }
}

#[derive(Debug)]
pub struct Pool {
    pub name: String,
    /// 0 means unlimited.
    pub depth: usize,
    pub current_use: usize,
    /// Delayed edges ordered by (descending weight, id).
    pub delayed: BTreeSet<(std::cmp::Reverse<i64>, EdgeId)>,
}

impl Pool {
    pub fn is_valid(&self) -> bool {
        true
    }
    pub fn should_delay_edge(&self) -> bool {
        self.depth != 0
    }
}

#[derive(Debug, Default)]
pub struct Scope {
    pub parent: Option<ScopeId>,
    pub bindings: HashMap<String, String>,
    pub rules: HashMap<String, RuleId>,
}

pub const DEFAULT_POOL: PoolId = PoolId(0);
pub const CONSOLE_POOL: PoolId = PoolId(1);
pub const ROOT_SCOPE: ScopeId = ScopeId(0);
pub const PHONY_RULE: RuleId = RuleId(0);

/// Whether `$in`/`$out` are shell-escaped when evaluated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escape {
    Shell,
    None,
}

#[derive(Debug)]
pub struct State {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub rules: Vec<Rule>,
    pub pools: Vec<Pool>,
    pub pool_names: HashMap<String, PoolId>,
    pub scopes: Vec<Scope>,
    pub paths: HashMap<String, NodeId>,
    pub defaults: Vec<NodeId>,
    /// Edges the dirty scan visited and had to visit again, because a
    /// dyndep file loaded later gave a producer to one of their inputs.
    pub scan_invalidations: u64,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        let mut s = State {
            nodes: Vec::new(),
            edges: Vec::new(),
            rules: Vec::new(),
            pools: Vec::new(),
            pool_names: HashMap::default(),
            scopes: vec![Scope::default()],
            paths: HashMap::default(),
            defaults: Vec::new(),
            scan_invalidations: 0,
        };
        s.rules.push(Rule { name: "phony".into(), bindings: HashMap::default(), phony: true });
        s.scopes[0].rules.insert("phony".into(), PHONY_RULE);
        s.add_pool("", 0);
        s.add_pool("console", 1);
        s
    }

    pub fn add_pool(&mut self, name: &str, depth: usize) -> PoolId {
        let id = PoolId(self.pools.len() as u32);
        self.pools.push(Pool { name: name.to_string(), depth, current_use: 0, delayed: BTreeSet::new() });
        self.pool_names.insert(name.to_string(), id);
        id
    }

    pub fn lookup_pool(&self, name: &str) -> Option<PoolId> {
        self.pool_names.get(name).copied()
    }

    pub fn new_scope(&mut self, parent: Option<ScopeId>) -> ScopeId {
        let id = ScopeId(self.scopes.len() as u32);
        self.scopes.push(Scope { parent, ..Default::default() });
        id
    }

    pub fn add_rule(&mut self, scope: ScopeId, rule: Rule) -> RuleId {
        let id = RuleId(self.rules.len() as u32);
        self.scopes[scope.0 as usize].rules.insert(rule.name.clone(), id);
        self.rules.push(rule);
        id
    }

    pub fn lookup_rule_current_scope(&self, scope: ScopeId, name: &str) -> Option<RuleId> {
        self.scopes[scope.0 as usize].rules.get(name).copied()
    }

    pub fn lookup_rule(&self, scope: ScopeId, name: &str) -> Option<RuleId> {
        let mut cur = Some(scope);
        while let Some(s) = cur {
            let sc = &self.scopes[s.0 as usize];
            if let Some(r) = sc.rules.get(name) {
                return Some(*r);
            }
            cur = sc.parent;
        }
        None
    }

    /// Looks up a variable in a scope chain.
    pub fn lookup_variable(&self, scope: ScopeId, var: &str) -> String {
        let mut cur = Some(scope);
        while let Some(s) = cur {
            let sc = &self.scopes[s.0 as usize];
            if let Some(v) = sc.bindings.get(var) {
                return v.clone();
            }
            cur = sc.parent;
        }
        String::new()
    }

    pub fn add_binding(&mut self, scope: ScopeId, key: &str, value: String) {
        self.scopes[scope.0 as usize].bindings.insert(key.to_string(), value);
    }

    pub fn add_edge(&mut self, rule: RuleId, scope: ScopeId) -> EdgeId {
        let id = EdgeId(self.edges.len() as u32);
        self.edges.push(Edge {
            id,
            rule,
            pool: DEFAULT_POOL,
            scope,
            env: None,
            inputs: Vec::new(),
            outputs: Vec::new(),
            validations: Vec::new(),
            implicit_deps: 0,
            order_only_deps: 0,
            implicit_outs: 0,
            dyndep: None,
            outputs_ready: false,
            deps_loaded: false,
            deps_added: false,
            deps_missing: false,
            mark: VisitMark::None,
            command_start_time: 0,
            prev_elapsed_time_millis: -1,
            critical_path_weight: -1,
        });
        id
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.idx()]
    }
    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.idx()]
    }
    pub fn edge(&self, id: EdgeId) -> &Edge {
        &self.edges[id.idx()]
    }
    pub fn edge_mut(&mut self, id: EdgeId) -> &mut Edge {
        &mut self.edges[id.idx()]
    }
    pub fn path(&self, id: NodeId) -> &str {
        &self.nodes[id.idx()].path
    }

    pub fn get_node(&mut self, path: &str, slash_bits: u64) -> NodeId {
        if let Some(id) = self.paths.get(path) {
            return *id;
        }
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            path: path.to_string(),
            slash_bits,
            in_edge: None,
            out_edges: Vec::new(),
            validation_out_edges: Vec::new(),
            generated_by_dep_loader: true,
            dyndep_pending: false,
            mtime: -1,
            exists: Existence::Unknown,
            dirty: false,
        });
        self.paths.insert(path.to_string(), id);
        id
    }

    pub fn lookup_node(&self, path: &str) -> Option<NodeId> {
        self.paths.get(path).copied()
    }

    pub fn add_in(&mut self, edge: EdgeId, path: &str, slash_bits: u64) {
        let n = self.get_node(path, slash_bits);
        self.nodes[n.idx()].generated_by_dep_loader = false;
        self.edges[edge.idx()].inputs.push(n);
        self.nodes[n.idx()].out_edges.push(edge);
    }

    pub fn add_out(&mut self, edge: EdgeId, path: &str, slash_bits: u64) -> Result<(), String> {
        let n = self.get_node(path, slash_bits);
        if let Some(other) = self.nodes[n.idx()].in_edge {
            if other == edge {
                return Err(format!("{path} is defined as an output multiple times"));
            }
            return Err(format!("multiple rules generate {path}"));
        }
        self.edges[edge.idx()].outputs.push(n);
        let node = &mut self.nodes[n.idx()];
        node.in_edge = Some(edge);
        node.generated_by_dep_loader = false;
        Ok(())
    }

    pub fn add_validation(&mut self, edge: EdgeId, path: &str, slash_bits: u64) {
        let n = self.get_node(path, slash_bits);
        self.edges[edge.idx()].validations.push(n);
        let node = &mut self.nodes[n.idx()];
        node.validation_out_edges.push(edge);
        node.generated_by_dep_loader = false;
    }

    pub fn add_default(&mut self, path: &str) -> Result<(), String> {
        match self.lookup_node(path) {
            Some(n) => {
                self.defaults.push(n);
                Ok(())
            }
            None => Err(format!("unknown target '{path}'")),
        }
    }

    /// Nodes that are not inputs of any edge.
    pub fn root_nodes(&self) -> Result<Vec<NodeId>, String> {
        let mut roots = Vec::new();
        for e in &self.edges {
            for &o in &e.outputs {
                if self.nodes[o.idx()].out_edges.is_empty() {
                    roots.push(o);
                }
            }
        }
        if !self.edges.is_empty() && roots.is_empty() {
            return Err("could not determine root nodes of build graph".into());
        }
        Ok(roots)
    }

    pub fn default_nodes(&self) -> Result<Vec<NodeId>, String> {
        if self.defaults.is_empty() { self.root_nodes() } else { Ok(self.defaults.clone()) }
    }

    /// Resets build-time state, e.g. before a second build in the same process.
    pub fn reset(&mut self) {
        for n in &mut self.nodes {
            n.reset_state();
        }
        for e in &mut self.edges {
            e.outputs_ready = false;
            e.deps_loaded = false;
            e.deps_added = false;
            e.mark = VisitMark::None;
        }
    }

    /// Finds the closest node name, for "did you mean" suggestions.
    pub fn spellcheck_node(&self, path: &str) -> Option<NodeId> {
        let mut best: Option<(usize, NodeId)> = None;
        for (p, id) in &self.paths {
            let d = edit_distance(p, path, 3);
            if d <= 3 && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, *id));
            }
        }
        best.map(|(_, id)| id)
    }

    pub fn is_phony(&self, e: EdgeId) -> bool {
        self.rules[self.edges[e.idx()].rule.0 as usize].phony
    }

    pub fn rule_of(&self, e: EdgeId) -> &Rule {
        &self.rules[self.edges[e.idx()].rule.0 as usize]
    }

    pub fn use_console(&self, e: EdgeId) -> bool {
        self.edges[e.idx()].pool == CONSOLE_POOL
    }

    pub fn all_inputs_ready(&self, e: EdgeId) -> bool {
        self.edges[e.idx()].inputs.iter().all(|&i| self.nodes[i.idx()].in_edge.is_none_or(|ie| self.edges[ie.idx()].outputs_ready))
    }

    /// CMake 2.8.12.x and 3.0.x produced self-referencing phony rules of the
    /// form "build a: phony ... a ...".
    pub fn maybe_phonycycle_diagnostic(&self, e: EdgeId) -> bool {
        let edge = &self.edges[e.idx()];
        self.is_phony(e) && edge.outputs.len() == 1 && edge.implicit_outs == 0 && edge.implicit_deps == 0 && edge.order_only_deps == 0
    }

    // ---- Edge variable evaluation (ninja's EdgeEnv) -----------------------

    /// Evaluates an edge binding with `$in`/`$out` shell-escaped.
    pub fn edge_binding(&self, e: EdgeId, key: &str) -> Result<String, String> {
        self.edge_lookup(e, key, Escape::Shell, &mut Vec::new(), false)
    }

    pub fn edge_binding_bool(&self, e: EdgeId, key: &str) -> bool {
        self.edge_binding(e, key).map(|s| !s.is_empty()).unwrap_or(false)
    }

    pub fn edge_binding_unescaped(&self, e: EdgeId, key: &str) -> Result<String, String> {
        self.edge_lookup(e, key, Escape::None, &mut Vec::new(), false)
    }

    pub fn unescaped_depfile(&self, e: EdgeId) -> String {
        self.edge_binding_unescaped(e, "depfile").unwrap_or_default()
    }
    pub fn unescaped_dyndep(&self, e: EdgeId) -> String {
        self.edge_binding_unescaped(e, "dyndep").unwrap_or_default()
    }
    pub fn unescaped_rspfile(&self, e: EdgeId) -> String {
        self.edge_binding_unescaped(e, "rspfile").unwrap_or_default()
    }

    /// The command, optionally with the rspfile content appended (used for
    /// the command hash in the build log, as ninja does).
    pub fn evaluate_command(&self, e: EdgeId, incl_rsp_file: bool) -> Result<String, String> {
        let mut command = self.edge_binding(e, "command")?;
        if incl_rsp_file {
            let rsp = self.edge_binding(e, "rspfile_content")?;
            if !rsp.is_empty() {
                command.push_str(";rspfile=");
                command.push_str(&rsp);
            }
        }
        Ok(command)
    }

    fn make_path_list(&self, nodes: &[NodeId], sep: char, escape: Escape) -> String {
        let mut out = String::new();
        for &n in nodes {
            if !out.is_empty() {
                out.push(sep);
            }
            let path = self.nodes[n.idx()].path_decanonicalized();
            if escape == Escape::Shell {
                if cfg!(windows) {
                    crate::shell::win32_escape(&path, &mut out);
                } else {
                    crate::shell::shell_escape(&path, &mut out);
                }
            } else {
                out.push_str(&path);
            }
        }
        out
    }

    fn edge_lookup(&self, e: EdgeId, var: &str, escape: Escape, lookups: &mut Vec<String>, recursive: bool) -> Result<String, String> {
        let edge = &self.edges[e.idx()];
        if var == "in" || var == "in_newline" {
            let n = edge.inputs.len() - edge.implicit_deps - edge.order_only_deps;
            return Ok(self.make_path_list(&edge.inputs[..n], if var == "in" { ' ' } else { '\n' }, escape));
        }
        if var == "out" {
            let n = edge.outputs.len() - edge.implicit_outs;
            return Ok(self.make_path_list(&edge.outputs[..n], ' ', escape));
        }
        if recursive && let Some(pos) = lookups.iter().position(|l| l == var) {
            let mut cycle = String::new();
            for l in &lookups[pos..] {
                cycle.push_str(l);
                cycle.push_str(" -> ");
            }
            cycle.push_str(var);
            return Err(format!("cycle in rule variables: {cycle}"));
        }
        let rule = &self.rules[edge.rule.0 as usize];
        let eval = rule.bindings.get(var);
        // BindingEnv::LookupWithFallback on the edge's env.
        let scope = &self.scopes[edge.scope.0 as usize];
        let (own, parent) = match &edge.env {
            Some(env) => (env.get(var), Some(edge.scope)),
            None => (scope.bindings.get(var), scope.parent),
        };
        if let Some(v) = own {
            return Ok(v.clone());
        }
        if let Some(eval) = eval {
            let record = recursive;
            if record {
                lookups.push(var.to_string());
            }
            let r = eval.evaluate_with(&mut |v| self.edge_lookup(e, v, escape, lookups, true));
            if record {
                lookups.pop();
            }
            return r;
        }
        Ok(match parent {
            Some(p) => self.lookup_variable(p, var),
            None => String::new(),
        })
    }
}

/// Levenshtein distance with an early cutoff, as ninja's `EditDistance`.
pub fn edit_distance(s1: &str, s2: &str, max: usize) -> usize {
    let a = s1.as_bytes();
    let b = s2.as_bytes();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for y in 1..=a.len() {
        let mut prev = row[0];
        row[0] = y;
        let mut best = row[0];
        for x in 1..=b.len() {
            let old = row[x];
            row[x] = if a[y - 1] == b[x - 1] { prev } else { 1 + prev.min(row[x - 1]).min(row[x]) };
            prev = old;
            best = best.min(row[x]);
        }
        if best > max {
            return max + 1;
        }
    }
    row[b.len()]
}
