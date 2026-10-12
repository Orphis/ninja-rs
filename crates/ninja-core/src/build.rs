//! Dirty checking (ninja's `DependencyScan`), the build plan (`Plan`) and the
//! build loop (`Builder`), ported from ninja's graph.cc and build.cc.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};

use crate::hash::HashMap;
use std::time::Instant;

use crate::build_log::{BuildLog, LogEntry, hash_command};
use crate::canon::canonicalize_path;
use crate::clparser::parse_cl_output;
use crate::depfile::parse_depfile;
use crate::deps_log::DepsLog;
use crate::disk::Disk;
use crate::dyndep::{DyndepFile, apply_dyndeps, parse_dyndep};
use crate::graph::{EdgeId, Existence, NodeId, State, TimeStamp, VisitMark};
use crate::runner::{CommandRunner, ExecKind, ExitStatus, Task, TaskResult};
use crate::status::{EdgeInfo, Status, Verbosity};

// ---------------------------------------------------------------------------
// Dependency scan
// ---------------------------------------------------------------------------

pub struct Scanner<'a> {
    pub state: &'a mut State,
    pub disk: &'a dyn Disk,
    pub build_log: Option<&'a BuildLog>,
    pub deps_log: Option<&'a DepsLog>,
    pub explain: Option<&'a mut dyn Status>,
    /// Nodes of the deps log's paths, by path id, resolved once per build
    /// instead of looking each path up again for every edge that uses it.
    pub deps_node_cache: Option<&'a mut Vec<Option<NodeId>>>,
}

impl Scanner<'_> {
    fn explain(&mut self, msg: impl FnOnce() -> String) {
        if let Some(s) = self.explain.as_deref_mut() {
            s.explain(&msg());
        }
    }

    pub fn stat_if_necessary(&mut self, n: NodeId) -> Result<(), String> {
        if self.state.node(n).status_known() {
            return Ok(());
        }
        self.stat(n)
    }

    pub fn stat(&mut self, n: NodeId) -> Result<(), String> {
        let mtime = self.disk.stat(self.state.path(n))?;
        let node = self.state.node_mut(n);
        node.mtime = mtime;
        node.exists = if mtime != 0 { Existence::Exists } else { Existence::Missing };
        Ok(())
    }

    /// Updates the dirty state of `node` and everything it depends on.
    /// Validation nodes found along the way are appended to `validation_nodes`.
    pub fn recompute_dirty(&mut self, node: NodeId, validation_nodes: &mut Vec<NodeId>) -> Result<(), String> {
        let mut nodes = std::collections::VecDeque::from([node]);
        let mut stack = Vec::new();
        while let Some(n) = nodes.pop_front() {
            stack.clear();
            let mut new_validations = Vec::new();
            self.recompute_node_dirty(n, &mut stack, &mut new_validations)?;
            nodes.extend(new_validations.iter().copied());
            validation_nodes.extend(new_validations);
        }
        Ok(())
    }

    fn recompute_node_dirty(&mut self, node: NodeId, stack: &mut Vec<NodeId>, validation_nodes: &mut Vec<NodeId>) -> Result<(), String> {
        let Some(edge) = self.state.node(node).in_edge else {
            // If we already visited this leaf node then we are done.
            if self.state.node(node).status_known() {
                return Ok(());
            }
            // This node has no in-edge; it is dirty if it is missing.
            self.stat_if_necessary(node)?;
            if !self.state.node(node).exists() {
                let p = self.state.path(node).to_string();
                self.explain(|| format!("{p} has no in-edge and is missing"));
            }
            let missing = !self.state.node(node).exists();
            self.state.node_mut(node).dirty = missing;
            return Ok(());
        };

        // If we already finished this edge then we are done.
        if self.state.edge(edge).mark == VisitMark::Done {
            return Ok(());
        }
        // If we encountered this edge earlier in the call stack we have a cycle.
        self.verify_dag(node, stack)?;

        // Store any validation nodes from the edge for adding to the initial
        // nodes. Don't recurse into them, that would trigger the dependency
        // cycle detector if the validation node depends on this node.
        validation_nodes.extend(self.state.edge(edge).validations.iter().copied());

        // Mark the edge temporarily while in the call stack.
        self.state.edge_mut(edge).mark = VisitMark::InStack;
        stack.push(node);

        {
            let e = self.state.edge_mut(edge);
            e.outputs_ready = true;
            e.deps_missing = false;
        }

        let first_visit = !self.state.edge(edge).deps_loaded;
        if first_visit {
            // This is our first encounter with this edge.
            self.state.edge_mut(edge).deps_loaded = true;
            // If there is a pending dyndep file, visit it now.
            if let Some(dd) = self.state.edge(edge).dyndep
                && self.state.node(dd).dyndep_pending
            {
                self.recompute_node_dirty(dd, stack, validation_nodes)?;
                let ready = match self.state.node(dd).in_edge {
                    None => true,
                    Some(e) => self.state.edge(e).outputs_ready,
                };
                if ready {
                    // The dyndep file is ready, so load it now.
                    let ddf = self.load_dyndeps(dd)?;
                    self.invalidate_consumers(&ddf);
                }
            }
        }

        // Load output mtimes so we can compare them to the most recent input below.
        let outputs = self.state.edge(edge).outputs.clone();
        for &o in &outputs {
            self.stat_if_necessary(o)?;
        }

        self.visit_inputs(edge, stack, validation_nodes)?;
        let (mut dirty, most_recent_input) = self.check_inputs(edge);

        // We may also be dirty due to output state: missing outputs, out of
        // date outputs, etc.
        if !dirty {
            dirty = self.recompute_outputs_dirty(edge, most_recent_input)?;
        }

        if first_visit {
            // Discovered deps (depfile or deps log) only matter to an edge
            // that is clean so far. A dirty edge's are stale: in the graph
            // they could order the build around files it no longer reads, or
            // form cycles (ninja 1.14). Then only whether they exist counts.
            if !dirty {
                self.state.edge_mut(edge).deps_added = true;
                if self.load_deps(edge)?.is_none() {
                    // Failed to load dependency info: rebuild to regenerate it.
                    dirty = true;
                    self.state.edge_mut(edge).deps_missing = true;
                } else {
                    // Visit what they added. Only a newer input can make the
                    // edge dirty now, so only times are compared again.
                    self.visit_inputs(edge, stack, validation_nodes)?;
                    let (inputs_dirty, newest) = self.check_inputs(edge);
                    dirty = inputs_dirty || (newest != most_recent_input && self.outputs_older_than(edge, newest));
                }
            } else if !self.deps_available(edge)? {
                self.state.edge_mut(edge).deps_missing = true;
            }
        }

        // Finally, visit each output and update their dirty state if necessary.
        if dirty {
            for &o in &outputs {
                self.state.node_mut(o).dirty = true;
            }
        }

        // If an edge is dirty, its outputs are normally not ready. (It's
        // possible to be clean but still not be ready in the presence of
        // order-only inputs.) But phony edges with no inputs have nothing to
        // do, so are always ready.
        if dirty && !(self.state.is_phony(edge) && self.state.edge(edge).inputs.is_empty()) {
            self.state.edge_mut(edge).outputs_ready = false;
        }

        // Mark the edge as finished during this walk now that it will no
        // longer be in the call stack.
        self.state.edge_mut(edge).mark = VisitMark::Done;
        debug_assert_eq!(stack.last(), Some(&node));
        stack.pop();
        Ok(())
    }

    /// Visits all of `edge`'s inputs before checking if any of them is
    /// ready: newly encountered edges may load dyndep files and gain outputs
    /// that correspond to some of our inputs. Such a load may also send
    /// inputs already visited back to unvisited (see
    /// `invalidate_consumers`), or give one a producer not visited yet:
    /// those are visited again.
    fn visit_inputs(&mut self, edge: EdgeId, stack: &mut Vec<NodeId>, validation_nodes: &mut Vec<NodeId>) -> Result<(), String> {
        loop {
            let inputs = self.state.edge(edge).inputs.clone();
            for &i in &inputs {
                self.recompute_node_dirty(i, stack, validation_nodes)?;
            }
            let unvisited = |i: &NodeId| self.state.node(*i).in_edge.is_some_and(|e| self.state.edge(e).mark == VisitMark::None);
            if !inputs.iter().any(unvisited) {
                return Ok(());
            }
        }
    }

    /// Whether one of `edge`'s regular inputs is dirty (or missing), and the
    /// most recent of the others. An input that isn't ready makes the
    /// edge's outputs not ready.
    fn check_inputs(&mut self, edge: EdgeId) -> (bool, Option<NodeId>) {
        let inputs = self.state.edge(edge).inputs.clone();
        let mut dirty = false;
        let mut most_recent_input: Option<NodeId> = None;
        for (idx, &i) in inputs.iter().enumerate() {
            // If an input is not ready, neither are our outputs.
            if let Some(ie) = self.state.node(i).in_edge
                && !self.state.edge(ie).outputs_ready
            {
                self.state.edge_mut(edge).outputs_ready = false;
            }
            if !self.state.edge(edge).is_order_only(idx) {
                // If a regular input is dirty (or missing), we're dirty.
                // Otherwise consider mtime.
                if self.state.node(i).dirty {
                    let ip = self.state.path(i).to_string();
                    self.explain(|| format!("{ip} is dirty"));
                    dirty = true;
                } else if most_recent_input.is_none_or(|m| self.state.node(i).mtime > self.state.node(m).mtime) {
                    most_recent_input = Some(i);
                }
            }
        }
        (dirty, most_recent_input)
    }

    /// A dyndep file loaded during the scan gave producers to its implicit
    /// outputs. Edges the scan already visited may read them, taking them
    /// for sources (ninja issues 2653 and 2743): send those edges, and the
    /// ones downstream, back to unvisited so their dirty state is computed
    /// again. Edges still on the stack visit their inputs again instead.
    fn invalidate_consumers(&mut self, ddf: &DyndepFile) {
        let mut nodes: Vec<NodeId> = ddf.values().flat_map(|d| d.implicit_outputs.iter().copied()).collect();
        while let Some(n) = nodes.pop() {
            for e in self.state.node(n).out_edges.clone() {
                if self.state.edge(e).mark == VisitMark::Done {
                    self.state.edge_mut(e).mark = VisitMark::None;
                    self.state.scan_invalidations += 1;
                    nodes.extend(self.state.edge(e).outputs.iter().copied());
                }
            }
        }
    }

    fn verify_dag(&mut self, node: NodeId, stack: &mut [NodeId]) -> Result<(), String> {
        let edge = self.state.node(node).in_edge.expect("verify_dag on a node with an in-edge");
        // If we have no temporary mark on the edge then we do not yet have a cycle.
        if self.state.edge(edge).mark != VisitMark::InStack {
            return Ok(());
        }
        // We have this edge earlier in the call stack. Find it.
        let start = stack.iter().position(|&n| self.state.node(n).in_edge == Some(edge)).expect("edge in stack");
        // Make the cycle clear by reporting its start as the node at its end
        // instead of some other output of the starting edge.
        stack[start] = node;
        let mut err = String::from("dependency cycle: ");
        for &n in &stack[start..] {
            err.push_str(self.state.path(n));
            err.push_str(" -> ");
        }
        err.push_str(self.state.path(stack[start]));
        if start + 1 == stack.len() && self.state.maybe_phonycycle_diagnostic(edge) {
            // The manifest parser would have filtered out the self-referencing
            // input if it were not configured to allow the error.
            err.push_str(" [-w phonycycle=err]");
        }
        Err(err)
    }

    /// Whether any output of `edge` is dirty given its most recent input.
    pub fn recompute_outputs_dirty(&mut self, edge: EdgeId, most_recent_input: Option<NodeId>) -> Result<bool, String> {
        let command = if self.state.is_phony(edge) { String::new() } else { self.state.evaluate_command(edge, true)? };
        let outputs = self.state.edge(edge).outputs.clone();
        for o in outputs {
            if self.recompute_output_dirty(edge, most_recent_input, Some(&command), o) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `recompute_outputs_dirty` for outputs already found clean, with a
    /// newer `most_recent_input`: compares times only.
    fn outputs_older_than(&mut self, edge: EdgeId, most_recent_input: Option<NodeId>) -> bool {
        let outputs = self.state.edge(edge).outputs.clone();
        outputs.into_iter().any(|o| self.recompute_output_dirty(edge, most_recent_input, None, o))
    }

    /// Whether `output` is dirty. Without `command`, the checks that don't
    /// depend on `most_recent_input` are skipped.
    fn recompute_output_dirty(&mut self, edge: EdgeId, most_recent_input: Option<NodeId>, command: Option<&str>, output: NodeId) -> bool {
        let mri_mtime = most_recent_input.map(|m| self.state.node(m).mtime);
        let out_path = self.state.path(output).to_string();
        if self.state.is_phony(edge) {
            // Phony edges don't write any output. Outputs are only dirty if
            // there are no inputs or validations and we're missing the output.
            let e = self.state.edge(edge);
            if e.inputs.is_empty() && e.validations.is_empty() && !self.state.node(output).exists() {
                self.explain(|| format!("output {out_path} of phony edge with no inputs doesn't exist"));
                return true;
            }
            // Update the mtime with the newest input. Dependents can thus call
            // mtime() on the fake node and get the latest mtime of the dependencies.
            if let Some(m) = mri_mtime {
                let node = self.state.node_mut(output);
                if !node.exists() {
                    node.mtime = node.mtime.max(m);
                }
            }
            return false;
        }

        // Dirty if we're missing the output.
        if !self.state.node(output).exists() {
            self.explain(|| format!("output {out_path} doesn't exist"));
            return true;
        }

        let entry = self.build_log.and_then(|l| l.lookup_by_output(&out_path)).cloned();
        let restat = self.state.edge_binding_bool(edge, "restat");
        let output_mtime = self.state.node(output).mtime;

        // If this is a restat rule, we may have cleaned the output in a
        // previous run and stored the command start time in the build log.
        // We don't want to consider a restat rule's outputs as dirty unless an
        // input changed since the last run, so we'll skip checking the output
        // file's actual mtime and simply check the recorded mtime from the
        // log against the most recent input's mtime (see below).
        let used_restat = restat && entry.is_some();
        if !used_restat
            && let Some(m) = mri_mtime
            && output_mtime < m
        {
            let mp = self.state.path(most_recent_input.unwrap()).to_string();
            self.explain(|| format!("output {out_path} older than most recent input {mp} ({output_mtime} vs {m})"));
            return true;
        }

        if self.build_log.is_some() {
            let generator = self.state.edge_binding_bool(edge, "generator");
            if let Some(entry) = &entry {
                if let Some(command) = command
                    && !generator
                    && hash_command(command) != entry.command_hash
                {
                    // May also be dirty due to the command changing since the
                    // last build. But if this is a generator rule, the command
                    // changing does not make us dirty.
                    self.explain(|| format!("command line changed for {out_path}"));
                    return true;
                }
                if let Some(m) = mri_mtime
                    && entry.mtime < m
                {
                    // May also be dirty due to the mtime in the log being older
                    // than the mtime of the most recent input. This can occur
                    // even when the mtime on disk is newer if a previous run
                    // wrote to the output file but exited with an error or was
                    // interrupted.
                    let mp = self.state.path(most_recent_input.unwrap()).to_string();
                    let em = entry.mtime;
                    self.explain(|| format!("recorded mtime of {out_path} older than most recent input {mp} ({em} vs {m})"));
                    return true;
                }
            }
            if entry.is_none() && !generator && command.is_some() {
                self.explain(|| format!("command line not found in log for {out_path}"));
                return true;
            }
        }
        false
    }

    /// Adds implicit deps from the deps log or depfile to `edge`'s inputs,
    /// and returns them. `None` if they are missing or stale (the edge must
    /// be rebuilt).
    pub fn load_deps(&mut self, edge: EdgeId) -> Result<Option<Vec<NodeId>>, String> {
        let deps_type = self.state.edge_binding(edge, "deps")?;
        if !deps_type.is_empty() {
            return self.load_deps_from_log(edge);
        }
        let depfile = self.state.unescaped_depfile(edge);
        if !depfile.is_empty() {
            return self.load_dep_file(edge, &depfile);
        }
        Ok(Some(Vec::new()))
    }

    /// ninja's `LoadDepsTry`: whether `load_deps` would find deps for
    /// `edge`, without adding them to the graph.
    fn deps_available(&mut self, edge: EdgeId) -> Result<bool, String> {
        let deps_type = self.state.edge_binding(edge, "deps")?;
        if !deps_type.is_empty() {
            let output = self.state.edge(edge).outputs[0];
            let out_path = self.state.path(output).to_string();
            let Some(deps_mtime) = self.deps_log.and_then(|l| l.get(&out_path)).map(|d| d.mtime) else {
                self.explain(|| format!("deps for '{out_path}' are missing"));
                return Ok(false);
            };
            self.stat_if_necessary(output)?;
            let omtime = self.state.node(output).mtime;
            if omtime > deps_mtime {
                self.explain(|| format!("stored deps info out of date for '{out_path}' ({deps_mtime} vs {omtime})"));
                return Ok(false);
            }
            return Ok(true);
        }
        let depfile = self.state.unescaped_depfile(edge);
        if !depfile.is_empty() {
            let mtime = self.disk.stat(&depfile).map_err(|e| format!("loading '{depfile}': {e}"))?;
            return Ok(mtime > 0);
        }
        Ok(true)
    }

    /// The paths of the deps earlier builds discovered for `edge`, without
    /// adding them to the graph or checking that they are up to date. Empty
    /// when there are none or they can't be read.
    pub fn discovered_dep_paths(&mut self, edge: EdgeId) -> Vec<String> {
        let Ok(deps_type) = self.state.edge_binding(edge, "deps") else { return Vec::new() };
        if !deps_type.is_empty() {
            let out_path = self.state.path(self.state.edge(edge).outputs[0]);
            return self.deps_log.and_then(|l| l.get(out_path)).map(|d| d.paths().map(str::to_string).collect()).unwrap_or_default();
        }
        let depfile = self.state.unescaped_depfile(edge);
        if depfile.is_empty() {
            return Vec::new();
        }
        match self.disk.read_file(&depfile) {
            Ok(Some(content)) => parse_depfile(&content).map(|d| d.ins.iter().map(|i| canonicalize_path(i).0).collect()).unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn load_dep_file(&mut self, edge: EdgeId, path: &str) -> Result<Option<Vec<NodeId>>, String> {
        let content = match self.disk.read_file(path) {
            Ok(Some(c)) => c,
            Ok(None) => Vec::new(),
            Err(e) => return Err(format!("loading '{path}': {e}")),
        };
        let first_output = self.state.edge(edge).outputs[0];
        if content.is_empty() {
            self.explain(|| format!("depfile '{path}' is missing"));
            return Ok(None);
        }
        let depfile = parse_depfile(&content).map_err(|e| format!("{path}: {e}"))?;
        if depfile.outs.is_empty() {
            return Err(format!("{path}: no outputs declared"));
        }
        // Check that this depfile matches the edge's output; if not, return
        // false to mark the edge as dirty.
        let (primary, _) = canonicalize_path(&depfile.outs[0]);
        let opath = self.state.path(first_output).to_string();
        if primary != opath {
            self.explain(|| format!("expected depfile '{path}' to mention '{opath}', got '{primary}'"));
            return Ok(None);
        }
        // Ensure that all mentioned outputs are outputs of the edge.
        for o in &depfile.outs {
            let (o, _) = canonicalize_path(o);
            if !self.state.edge(edge).outputs.iter().any(|&n| self.state.path(n) == o) {
                return Err(format!("{path}: depfile mentions '{o}' as an output, but no such output was declared"));
            }
        }
        let mut nodes = Vec::with_capacity(depfile.ins.len());
        for i in &depfile.ins {
            let (p, bits) = canonicalize_path(i);
            nodes.push(self.state.get_node(&p, bits));
        }
        self.add_implicit_deps(edge, &nodes);
        Ok(Some(nodes))
    }

    fn add_implicit_deps(&mut self, edge: EdgeId, nodes: &[NodeId]) {
        {
            let e = self.state.edge_mut(edge);
            let pos = e.inputs.len() - e.order_only_deps;
            e.inputs.splice(pos..pos, nodes.iter().copied());
            e.implicit_deps += nodes.len();
        }
        for &n in nodes {
            self.state.node_mut(n).out_edges.push(edge);
        }
    }

    fn load_deps_from_log(&mut self, edge: EdgeId) -> Result<Option<Vec<NodeId>>, String> {
        // Deps are only supported for single-target edges.
        let output = self.state.edge(edge).outputs[0];
        let out_path = self.state.path(output).to_string();
        // Copy the log reference out, so the deps borrow the log, not `self`.
        let Some(log) = self.deps_log else {
            self.explain(|| format!("deps for '{out_path}' are missing"));
            return Ok(None);
        };
        let Some(deps) = log.get(&out_path) else {
            self.explain(|| format!("deps for '{out_path}' are missing"));
            return Ok(None);
        };
        // Deps are invalid if the output is newer than the deps.
        self.stat_if_necessary(output)?;
        let omtime = self.state.node(output).mtime;
        if omtime > deps.mtime {
            let dm = deps.mtime;
            self.explain(|| format!("stored deps info out of date for '{out_path}' ({dm} vs {omtime})"));
            return Ok(None);
        }
        let mut nodes = Vec::with_capacity(deps.len());
        match self.deps_node_cache.as_deref_mut() {
            Some(cache) => {
                for &id in deps.ids() {
                    let i = id as usize;
                    if i >= cache.len() {
                        cache.resize(log.path_count(), None);
                    }
                    // Paths in the log are canonical already.
                    let node = *cache[i].get_or_insert_with(|| self.state.get_node(log.path(id), 0));
                    nodes.push(node);
                }
            }
            None => {
                for p in deps.paths() {
                    nodes.push(self.state.get_node(p, 0));
                }
            }
        }
        self.add_implicit_deps(edge, &nodes);
        Ok(Some(nodes))
    }

    /// After a restat left `edge`'s inputs clean: loads the discovered deps
    /// the scan skipped because the edge was dirty, visits them, and tells
    /// whether they make it dirty. `None` if they are missing (the edge
    /// stays dirty; ninja PR 2842).
    pub fn load_deps_after_restat(&mut self, edge: EdgeId, validation_nodes: &mut Vec<NodeId>) -> Result<Option<(Vec<NodeId>, bool)>, String> {
        self.state.edge_mut(edge).deps_added = true;
        let Some(deps) = self.load_deps(edge)? else {
            self.state.edge_mut(edge).deps_missing = true;
            return Ok(None);
        };
        let before = self.most_recent_input_except(edge, &deps);
        // Visit them with the edge on the stack, to catch cycles through it.
        let mark = std::mem::replace(&mut self.state.edge_mut(edge).mark, VisitMark::InStack);
        let mut stack = vec![self.state.edge(edge).outputs[0]];
        let visited = deps.iter().try_for_each(|&i| self.recompute_node_dirty(i, &mut stack, validation_nodes));
        self.state.edge_mut(edge).mark = mark;
        visited?;
        let (dirty, newest) = self.check_inputs(edge);
        Ok(Some((deps, dirty || (newest != before && self.outputs_older_than(edge, newest)))))
    }

    /// The most recent of `edge`'s regular inputs, ignoring `skip`.
    fn most_recent_input_except(&self, edge: EdgeId, skip: &[NodeId]) -> Option<NodeId> {
        let mut most_recent_input: Option<NodeId> = None;
        for &i in self.state.edge(edge).non_order_only_inputs() {
            if !skip.contains(&i) && most_recent_input.is_none_or(|m| self.state.node(i).mtime > self.state.node(m).mtime) {
                most_recent_input = Some(i);
            }
        }
        most_recent_input
    }

    /// Loads a dyndep file and applies it to the edges that use it.
    pub fn load_dyndeps(&mut self, node: NodeId) -> Result<DyndepFile, String> {
        let path = self.state.path(node).to_string();
        self.explain(|| format!("loading dyndep file '{path}'"));
        let content = match self.disk.read_file(&path) {
            Ok(Some(c)) => c,
            Ok(None) => return Err(format!("loading '{path}': No such file or directory")),
            Err(e) => return Err(format!("loading '{path}': {e}")),
        };
        let mut ddf = DyndepFile::default();
        parse_dyndep(self.state, &path, &content, &mut ddf)?;
        apply_dyndeps(self.state, node, &mut ddf)?;
        Ok(ddf)
    }
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// We do not want to build the edge, but we might want to build one of
    /// its dependents.
    Nothing,
    /// We want to build the edge, but have not yet scheduled it.
    ToStart,
    /// We want to build the edge, have scheduled it, and are waiting for it
    /// to complete.
    ToFinish,
}

#[derive(Default)]
pub struct Plan {
    pub want: HashMap<EdgeId, Want>,
    ready: BinaryHeap<(i64, Reverse<EdgeId>)>,
    targets: Vec<NodeId>,
    command_edges: usize,
    wanted_edges: usize,
}

impl Plan {
    /// Whether there is more work to be done.
    pub fn more_to_do(&self) -> bool {
        self.wanted_edges > 0 && self.command_edges > 0
    }
    pub fn command_edge_count(&self) -> usize {
        self.command_edges
    }
    pub fn work_ready(&self) -> bool {
        !self.ready.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EdgeResult {
    Failed,
    Succeeded,
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct BuildConfig {
    pub verbosity: Verbosity,
    pub dry_run: bool,
    pub parallelism: usize,
    /// Number of failures before stopping; `usize::MAX` for `-k 0`.
    pub failures_allowed: usize,
    pub explain: bool,
    pub keep_rsp: bool,
    pub keep_depfile: bool,
    /// Extra edge bindings evaluated into `Task::bindings`.
    pub task_bindings: Vec<String>,
    /// Directory used to relativize `/showIncludes` paths (Windows).
    pub cwd: String,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            verbosity: Verbosity::Normal,
            dry_run: false,
            parallelism: 1,
            failures_allowed: 1,
            explain: false,
            keep_rsp: false,
            keep_depfile: false,
            task_bindings: Vec::new(),
            cwd: ".".into(),
        }
    }
}

#[derive(Debug)]
pub struct BuildError {
    pub message: String,
    pub interrupted: bool,
}

impl BuildError {
    fn new(m: impl Into<String>) -> Self {
        Self { message: m.into(), interrupted: false }
    }
}

pub struct Builder<'a> {
    pub state: &'a mut State,
    pub disk: &'a dyn Disk,
    pub build_log: Option<&'a mut BuildLog>,
    pub deps_log: Option<&'a mut DepsLog>,
    pub config: &'a BuildConfig,
    pub status: &'a mut dyn Status,
    pub plan: Plan,
    running: HashMap<EdgeId, i64>,
    start: Instant,
    lock_file_path: String,
    deps_node_cache: Vec<Option<NodeId>>,
    /// How many times each edge was rewound (see `rewind`).
    rewinds: HashMap<EdgeId, u32>,
    /// Targets and validation nodes given to the dirty scan so far.
    scanned: Vec<NodeId>,
}

/// How many times an edge may wait again for lost inputs before it fails.
const MAX_REWINDS: u32 = 3;

impl<'a> Builder<'a> {
    pub fn new(
        state: &'a mut State,
        disk: &'a dyn Disk,
        build_log: Option<&'a mut BuildLog>,
        deps_log: Option<&'a mut DepsLog>,
        config: &'a BuildConfig,
        status: &'a mut dyn Status,
    ) -> Self {
        let build_dir = state.lookup_variable(crate::graph::ROOT_SCOPE, "builddir");
        let lock_file_path = if build_dir.is_empty() { ".ninja_lock".to_string() } else { format!("{build_dir}/.ninja_lock") };
        Self {
            state,
            disk,
            build_log,
            deps_log,
            config,
            status,
            plan: Plan::default(),
            running: HashMap::default(),
            start: Instant::now(),
            lock_file_path,
            deps_node_cache: Vec::new(),
            rewinds: HashMap::default(),
            scanned: Vec::new(),
        }
    }

    fn scanner(&mut self) -> Scanner<'_> {
        Scanner {
            state: &mut *self.state,
            disk: self.disk,
            build_log: self.build_log.as_deref(),
            deps_log: self.deps_log.as_deref(),
            explain: if self.config.explain { Some(&mut *self.status) } else { None },
            deps_node_cache: Some(&mut self.deps_node_cache),
        }
    }

    pub fn add_target_by_name(&mut self, name: &str) -> Result<NodeId, String> {
        let node = self.state.lookup_node(name).ok_or_else(|| format!("unknown target: '{name}'"))?;
        self.add_target(node)?;
        Ok(node)
    }

    pub fn add_target(&mut self, target: NodeId) -> Result<(), String> {
        let invalidations = self.state.scan_invalidations;
        let mut validation_nodes = Vec::new();
        self.scanner().recompute_dirty(target, &mut validation_nodes)?;
        self.scanned.push(target);
        self.scanned.extend(validation_nodes.iter().copied());
        if self.state.scan_invalidations != invalidations {
            // A dyndep file loaded by this scan changed what earlier scans
            // found clean.
            return self.rescan_targets();
        }
        let in_edge = self.state.node(target).in_edge;
        if in_edge.is_none_or(|e| !self.state.edge(e).outputs_ready) {
            self.plan_add_target(target)?;
        }
        // Also add any validation nodes found during RecomputeDirty as top
        // level targets.
        for v in validation_nodes {
            if let Some(ve) = self.state.node(v).in_edge
                && !self.state.edge(ve).outputs_ready
            {
                self.plan_add_target(v)?;
            }
        }
        Ok(())
    }

    /// Scans every target again, after dyndep files sent edges earlier
    /// scans had visited back to unvisited, and adds what is now dirty to
    /// the plan.
    fn rescan_targets(&mut self) -> Result<(), String> {
        loop {
            let invalidations = self.state.scan_invalidations;
            for t in self.scanned.clone() {
                let mut validation_nodes = Vec::new();
                self.scanner().recompute_dirty(t, &mut validation_nodes)?;
                for v in validation_nodes {
                    if !self.scanned.contains(&v) {
                        self.scanned.push(v);
                    }
                }
            }
            if self.state.scan_invalidations == invalidations {
                break;
            }
        }
        let mut seen = BTreeSet::new();
        for t in self.scanned.clone() {
            if self.state.node(t).in_edge.is_some_and(|e| self.state.edge(e).outputs_ready) {
                continue;
            }
            if !self.plan.targets.contains(&t) {
                self.plan.targets.push(t);
            }
            self.replan(t, None, &mut seen)?;
        }
        Ok(())
    }

    /// `add_sub_target` for a graph whose dirty state grew after edges were
    /// added to the plan: walks every edge that isn't ready, also below
    /// edges already in the plan.
    fn replan(&mut self, node: NodeId, dependent: Option<NodeId>, seen: &mut BTreeSet<EdgeId>) -> Result<(), String> {
        let Some(edge) = self.state.node(node).in_edge else {
            // A leaf: let `add_sub_target` report missing sources.
            self.add_sub_target(node, dependent, None)?;
            return Ok(());
        };
        if self.state.edge(edge).outputs_ready || !seen.insert(edge) {
            return Ok(());
        }
        let want = *self.plan.want.entry(edge).or_insert(Want::Nothing);
        if self.state.node(node).dirty && want == Want::Nothing {
            self.plan.want.insert(edge, Want::ToStart);
            self.edge_wanted(edge);
        }
        for i in self.state.edge(edge).inputs.clone() {
            self.replan(i, Some(node), seen)?;
        }
        Ok(())
    }

    pub fn already_up_to_date(&self) -> bool {
        !self.plan.more_to_do()
    }

    // ---- Plan operations -------------------------------------------------

    fn plan_add_target(&mut self, target: NodeId) -> Result<(), String> {
        self.plan.targets.push(target);
        self.add_sub_target(target, None, None)?;
        Ok(())
    }

    /// Returns `Ok(true)` if the node's edge was added or already present.
    fn add_sub_target(&mut self, node: NodeId, dependent: Option<NodeId>, mut dyndep_walk: Option<&mut BTreeSet<EdgeId>>) -> Result<bool, String> {
        let Some(edge) = self.state.node(node).in_edge else {
            // Leaf node: either a regular input from the manifest (a dirty
            // flag means the file is missing and the build should stop) or
            // an implicit input from a depfile or dyndep file.
            let n = self.state.node(node);
            if n.dirty && !n.generated_by_dep_loader {
                let referenced = match dependent {
                    Some(d) => format!(", needed by '{}',", self.state.path(d)),
                    None => String::new(),
                };
                return Err(format!("'{}'{} missing and no known rule to make it", n.path, referenced));
            }
            return Ok(false);
        };
        if self.state.edge(edge).outputs_ready {
            return Ok(false); // Don't need to do anything.
        }
        let inserted = !self.plan.want.contains_key(&edge);
        let want = *self.plan.want.entry(edge).or_insert(Want::Nothing);
        if dyndep_walk.is_some() && want == Want::ToFinish {
            return Ok(false); // Don't need to do anything with already-scheduled edge.
        }
        // If we do need to build edge and we haven't already marked it as
        // wanted, mark it now.
        if self.state.node(node).dirty && want == Want::Nothing {
            self.plan.want.insert(edge, Want::ToStart);
            self.edge_wanted(edge);
        }
        if let Some(w) = dyndep_walk.as_deref_mut() {
            w.insert(edge);
        }
        if !inserted {
            return Ok(true); // We've already processed the inputs.
        }
        let inputs = self.state.edge(edge).inputs.clone();
        for i in inputs {
            self.add_sub_target(i, Some(node), dyndep_walk.as_deref_mut())?;
        }
        Ok(true)
    }

    fn edge_wanted(&mut self, edge: EdgeId) {
        self.plan.wanted_edges += 1;
        if !self.state.is_phony(edge) {
            self.plan.command_edges += 1;
            self.status.edge_added_to_plan();
        }
    }

    fn find_work(&mut self) -> Option<EdgeId> {
        self.plan.ready.pop().map(|(_, Reverse(e))| e)
    }

    fn edge_weight(&self, e: EdgeId) -> usize {
        if self.state.is_phony(e) { 0 } else { 1 }
    }

    fn push_ready(&mut self, e: EdgeId) {
        let w = self.state.edge(e).critical_path_weight;
        self.plan.ready.push((w, Reverse(e)));
    }

    fn pool_retrieve_ready_edges(&mut self, pool: crate::graph::PoolId) {
        loop {
            let p = &self.state.pools[pool.0 as usize];
            let Some(&first) = p.delayed.iter().next() else { break };
            let w = self.edge_weight(first.1);
            let p = &self.state.pools[pool.0 as usize];
            if p.current_use + w > p.depth {
                break;
            }
            self.state.pools[pool.0 as usize].delayed.remove(&first);
            self.push_ready(first.1);
            self.pool_edge_scheduled(first.1);
        }
    }

    fn pool_edge_scheduled(&mut self, e: EdgeId) {
        let w = self.edge_weight(e);
        let pool = self.state.edge(e).pool;
        let p = &mut self.state.pools[pool.0 as usize];
        if p.depth != 0 {
            p.current_use += w;
        }
    }

    fn pool_edge_finished(&mut self, e: EdgeId) {
        let w = self.edge_weight(e);
        let pool = self.state.edge(e).pool;
        let p = &mut self.state.pools[pool.0 as usize];
        if p.depth != 0 {
            p.current_use = p.current_use.saturating_sub(w);
        }
    }

    fn pool_delay_edge(&mut self, e: EdgeId) {
        let w = self.state.edge(e).critical_path_weight;
        let pool = self.state.edge(e).pool;
        self.state.pools[pool.0 as usize].delayed.insert((Reverse(w), e));
    }

    fn schedule_work(&mut self, edge: EdgeId) {
        match self.plan.want.get(&edge) {
            // This edge has already been scheduled. We can get here again if
            // an edge and one of its dependencies share an order-only input,
            // or if a node duplicates an out edge.
            Some(Want::ToFinish) => return,
            Some(Want::ToStart) => {}
            other => debug_assert!(false, "schedule_work on {other:?}"),
        }
        self.plan.want.insert(edge, Want::ToFinish);
        let pool = self.state.edge(edge).pool;
        if self.state.pools[pool.0 as usize].should_delay_edge() {
            self.pool_delay_edge(edge);
            self.pool_retrieve_ready_edges(pool);
        } else {
            self.pool_edge_scheduled(edge);
            self.push_ready(edge);
        }
    }

    fn plan_edge_finished(&mut self, edge: EdgeId, result: EdgeResult) -> Result<(), String> {
        let want = *self.plan.want.get(&edge).expect("finished edge is in the plan");
        let directly_wanted = want != Want::Nothing;
        // See if this job frees up any delayed jobs.
        if directly_wanted {
            self.pool_edge_finished(edge);
        }
        let pool = self.state.edge(edge).pool;
        self.pool_retrieve_ready_edges(pool);

        // The rest of this function only applies to successful commands.
        if result != EdgeResult::Succeeded {
            return Ok(());
        }
        if directly_wanted {
            self.plan.wanted_edges -= 1;
        }
        self.plan.want.remove(&edge);
        self.state.edge_mut(edge).outputs_ready = true;

        // Load dyndep info provided by this edge's outputs.
        self.load_dyndeps_for(edge)?;

        // Check off any nodes we were waiting for with this edge.
        let outputs = self.state.edge(edge).outputs.clone();
        for o in outputs {
            self.node_finished(o)?;
        }
        Ok(())
    }

    fn node_finished(&mut self, node: NodeId) -> Result<(), String> {
        // See if we we want any edges from this node.
        let out_edges = self.state.node(node).out_edges.clone();
        for oe in out_edges {
            if !self.plan.want.contains_key(&oe) {
                continue;
            }
            // See if the edge is now ready.
            self.edge_maybe_ready(oe)?;
        }
        Ok(())
    }

    fn edge_maybe_ready(&mut self, edge: EdgeId) -> Result<(), String> {
        if self.state.all_inputs_ready(edge) {
            if self.plan.want[&edge] != Want::Nothing {
                self.schedule_work(edge);
            } else {
                // We do not need to build this edge, but we might need to
                // build one of its dependents.
                self.plan_edge_finished(edge, EdgeResult::Succeeded)?;
            }
        }
        Ok(())
    }

    /// After a restat found an output unchanged: propagates the clean state
    /// to dependents that no longer need to run.
    fn clean_node(&mut self, node: NodeId) -> Result<(), String> {
        self.state.node_mut(node).dirty = false;
        let out_edges = self.state.node(node).out_edges.clone();
        for oe in out_edges {
            // Don't process edges that we don't actually want.
            match self.plan.want.get(&oe) {
                None | Some(Want::Nothing) => continue,
                _ => {}
            }
            // Don't attempt to clean an edge if it failed to load deps.
            if self.state.edge(oe).deps_missing {
                continue;
            }
            // If all non-order-only inputs for this edge are now clean, we
            // might have changed the dirty state of the outputs.
            let inputs: Vec<NodeId> = self.state.edge(oe).non_order_only_inputs().to_vec();
            if inputs.iter().any(|&i| self.state.node(i).dirty) {
                continue;
            }
            // Recompute most_recent_input.
            let mut most_recent_input: Option<NodeId> = None;
            for &i in &inputs {
                if most_recent_input.is_none_or(|m| self.state.node(i).mtime > self.state.node(m).mtime) {
                    most_recent_input = Some(i);
                }
            }
            // Now, this edge is dirty if any of the outputs are dirty. If the
            // edge isn't dirty, clean the outputs and mark the edge as not
            // wanted.
            let mut outputs_dirty = self.scanner().recompute_outputs_dirty(oe, most_recent_input)?;
            if !outputs_dirty && !self.state.edge(oe).deps_added {
                // The scan found the edge dirty, so it didn't read its
                // discovered deps: they decide now.
                let mut validation_nodes = Vec::new();
                let Some((deps, deps_dirty)) = self.scanner().load_deps_after_restat(oe, &mut validation_nodes)? else {
                    // Don't attempt to clean an edge if it failed to load deps.
                    continue;
                };
                for v in validation_nodes {
                    if self.state.node(v).in_edge.is_some_and(|ve| !self.state.edge(ve).outputs_ready) {
                        self.plan_add_target(v)?;
                    }
                }
                if deps_dirty {
                    // Build what the edge's new inputs need first.
                    let out0 = self.state.edge(oe).outputs[0];
                    let mut walk = BTreeSet::new();
                    for d in deps {
                        self.add_sub_target(d, Some(out0), Some(&mut walk))?;
                    }
                    for e in walk {
                        if self.plan.want.contains_key(&e) {
                            self.edge_maybe_ready(e)?;
                        }
                    }
                }
                outputs_dirty = deps_dirty;
            }
            if !outputs_dirty {
                let outputs = self.state.edge(oe).outputs.clone();
                for o in outputs {
                    self.clean_node(o)?;
                }
                self.plan.want.insert(oe, Want::Nothing);
                self.plan.wanted_edges -= 1;
                if !self.state.is_phony(oe) {
                    self.plan.command_edges -= 1;
                    self.status.edge_removed_from_plan();
                }
            }
        }
        Ok(())
    }

    fn load_dyndeps_for(&mut self, edge: EdgeId) -> Result<(), String> {
        // Load the dyndep information provided by this edge's outputs.
        let mut dyndep_nodes = Vec::new();
        let mut dyndep_edges: HashMap<EdgeId, crate::dyndep::Dyndeps> = HashMap::default();
        let outputs = self.state.edge(edge).outputs.clone();
        for node in outputs {
            if self.state.node(node).dyndep_pending {
                let ddf = self.scanner().load_dyndeps(node)?;
                dyndep_nodes.push(node);
                dyndep_edges.extend(ddf);
            }
        }
        if dyndep_nodes.is_empty() {
            return Ok(());
        }
        self.dyndeps_loaded(&dyndep_nodes, &dyndep_edges)
    }

    fn dyndeps_loaded(&mut self, dyndep_nodes: &[NodeId], dyndep_edges: &HashMap<EdgeId, crate::dyndep::Dyndeps>) -> Result<(), String> {
        // Recompute the dirty state of all our direct and indirect dependents
        // now that our dyndep information has been loaded.
        self.refresh_dyndep_dependents(dyndep_nodes)?;

        // We loaded dyndep information for those out_edges of the dyndep nodes
        // that specify one of them in a dyndep binding, but they may not be in
        // the plan. Starting with those already in the plan, walk
        // newly-reachable portion of the graph through the dyndep-discovered
        // dependencies.
        let mut roots: Vec<(EdgeId, Vec<NodeId>)> = Vec::new();
        let mut sorted: Vec<&EdgeId> = dyndep_edges.keys().collect();
        sorted.sort();
        for e in sorted {
            // If the edge outputs are ready we do not need to consider it here.
            if self.state.edge(*e).outputs_ready {
                continue;
            }
            // If the edge has not been encountered before then nothing already
            // in the plan depends on it so we do not need to consider the edge
            // yet either.
            if !self.plan.want.contains_key(e) {
                continue;
            }
            roots.push((*e, dyndep_edges[e].implicit_inputs.clone()));
        }

        // Walk dyndep-discovered portion of the graph to add it to the build plan.
        let mut dyndep_walk = BTreeSet::new();
        for (e, inputs) in roots {
            let out0 = self.state.edge(e).outputs[0];
            for i in inputs {
                self.add_sub_target(i, Some(out0), Some(&mut dyndep_walk))?;
            }
        }

        // Add dyndep node out edges that are in the plan (just as
        // Plan::NodeFinished would have without taking the dyndep code path).
        for &node in dyndep_nodes {
            for &oe in &self.state.node(node).out_edges {
                if self.plan.want.contains_key(&oe) {
                    dyndep_walk.insert(oe);
                }
            }
        }

        // See if any encountered edges are now ready.
        for e in dyndep_walk {
            if !self.plan.want.contains_key(&e) {
                continue;
            }
            self.edge_maybe_ready(e)?;
        }
        Ok(())
    }

    fn refresh_dyndep_dependents(&mut self, dyndep_nodes: &[NodeId]) -> Result<(), String> {
        // Collect the transitive closure of dependents and mark their edges as
        // not yet visited by RecomputeDirty.
        let mut dependents = BTreeSet::new();
        for &n in dyndep_nodes {
            self.unmark_dependents(n, &mut dependents);
        }
        // Update the dirty state of all dependents and check if their edges
        // have become wanted.
        for n in dependents {
            // Check if this dependent node is now dirty. Also checks for new cycles.
            let mut validation_nodes = Vec::new();
            self.scanner().recompute_dirty(n, &mut validation_nodes)?;
            // Add any validation nodes found during RecomputeDirty as new top
            // level targets.
            for v in validation_nodes {
                if let Some(ve) = self.state.node(v).in_edge
                    && !self.state.edge(ve).outputs_ready
                {
                    self.plan_add_target(v)?;
                }
            }
            if !self.state.node(n).dirty {
                continue;
            }
            // This edge was encountered before. However, we may not have
            // wanted to build it if the outputs were not known to be dirty.
            // With dyndep information an output is now known to be dirty, so
            // we want the edge.
            let edge = self.state.node(n).in_edge.expect("dependent has an in-edge");
            if self.plan.want.get(&edge) == Some(&Want::Nothing) {
                self.plan.want.insert(edge, Want::ToStart);
                self.edge_wanted(edge);
            }
        }
        Ok(())
    }

    fn unmark_dependents(&mut self, node: NodeId, dependents: &mut BTreeSet<NodeId>) {
        let out_edges = self.state.node(node).out_edges.clone();
        for edge in out_edges {
            if !self.plan.want.contains_key(&edge) {
                continue;
            }
            if self.state.edge(edge).mark != VisitMark::None {
                self.state.edge_mut(edge).mark = VisitMark::None;
                let outputs = self.state.edge(edge).outputs.clone();
                for o in outputs {
                    if dependents.insert(o) {
                        self.unmark_dependents(o, dependents);
                    }
                }
            }
        }
    }

    fn compute_critical_path(&mut self) {
        // Topological sort of all edges reachable from the targets: each
        // edge appears after the edges producing its inputs.
        let mut visited = std::collections::HashSet::new();
        let mut sorted: Vec<EdgeId> = Vec::new();
        let targets = self.plan.targets.clone();
        for t in targets {
            if let Some(e) = self.state.node(t).in_edge {
                // Iterative DFS (post-order).
                let mut stack: Vec<(EdgeId, usize)> = vec![(e, 0)];
                if !visited.insert(e) {
                    continue;
                }
                while let Some((edge, idx)) = stack.pop() {
                    let inputs = &self.state.edge(edge).inputs;
                    if idx < inputs.len() {
                        let input = inputs[idx];
                        stack.push((edge, idx + 1));
                        if let Some(p) = self.state.node(input).in_edge
                            && visited.insert(p)
                        {
                            stack.push((p, 0));
                        }
                    } else {
                        sorted.push(edge);
                    }
                }
            }
        }
        // Previous elapsed times from the build log.
        for &e in &sorted {
            let out0 = self.state.edge(e).outputs[0];
            let prev = self.build_log.as_deref().and_then(|log| log.lookup_by_output(self.state.path(out0))).map(|l| l.end_time - l.start_time).unwrap_or(-1);
            self.state.edge_mut(e).prev_elapsed_time_millis = prev;
        }
        let heuristic = |s: &State, e: EdgeId| -> i64 {
            if s.is_phony(e) {
                0
            } else {
                let p = s.edge(e).prev_elapsed_time_millis;
                if p < 0 { 1 } else { p }
            }
        };
        for &e in &sorted {
            let h = heuristic(self.state, e);
            self.state.edge_mut(e).critical_path_weight = h;
        }
        for &e in sorted.iter().rev() {
            let ew = self.state.edge(e).critical_path_weight;
            let inputs = self.state.edge(e).inputs.clone();
            for i in inputs {
                if let Some(p) = self.state.node(i).in_edge {
                    let cand = ew + heuristic(self.state, p);
                    if cand > self.state.edge(p).critical_path_weight {
                        self.state.edge_mut(p).critical_path_weight = cand;
                    }
                }
            }
        }
    }

    fn schedule_initial_edges(&mut self) {
        let mut wanted: Vec<EdgeId> = self.plan.want.iter().filter(|(_, w)| **w == Want::ToStart).map(|(e, _)| *e).collect();
        wanted.sort();
        let mut pools = BTreeSet::new();
        for edge in wanted {
            if !self.state.all_inputs_ready(edge) {
                continue;
            }
            let pool = self.state.edge(edge).pool;
            if self.state.pools[pool.0 as usize].should_delay_edge() {
                self.plan.want.insert(edge, Want::ToFinish);
                self.pool_delay_edge(edge);
                pools.insert(pool.0);
            } else {
                self.schedule_work(edge);
            }
        }
        // Retrieve once at the end so higher priority edges are retrieved first.
        for p in pools {
            self.pool_retrieve_ready_edges(crate::graph::PoolId(p));
        }
    }

    // ---- Build loop ------------------------------------------------------

    fn elapsed_ms(&self) -> i64 {
        self.start.elapsed().as_millis() as i64
    }

    fn edge_info(&self, edge: EdgeId) -> EdgeInfo {
        EdgeInfo {
            edge,
            description: self.state.edge_binding(edge, "description").unwrap_or_default(),
            command: self.state.evaluate_command(edge, false).unwrap_or_default(),
            outputs: self.state.edge(edge).outputs.iter().map(|&o| self.state.path(o).to_string()).collect(),
            use_console: self.state.use_console(edge),
            bindings: self.task_bindings(edge).unwrap_or_default(),
            cas_outputs: Vec::new(),
        }
    }

    /// The edge's `BuildConfig::task_bindings`, evaluated; empty ones left out.
    fn task_bindings(&self, edge: EdgeId) -> Result<Vec<(String, String)>, String> {
        let mut bindings = Vec::new();
        for k in &self.config.task_bindings {
            let v = self.state.edge_binding(edge, k)?;
            if !v.is_empty() {
                bindings.push((k.clone(), v));
            }
        }
        Ok(bindings)
    }

    /// Runs the build. `runner` executes commands.
    pub async fn build(&mut self, runner: &mut dyn CommandRunner) -> Result<(), BuildError> {
        debug_assert!(!self.already_up_to_date());
        self.compute_critical_path();
        self.schedule_initial_edges();
        self.start = Instant::now();

        let mut pending_commands = 0usize;
        let mut failures_allowed = self.config.failures_allowed;
        self.status.build_started();

        while self.plan.more_to_do() {
            // See if we can start any more commands.
            if failures_allowed > 0 {
                let mut capacity = runner.can_run_more();
                while capacity > 0 {
                    let Some(edge) = self.find_work() else { break };
                    if let Err(e) = self.start_edge(edge, runner) {
                        self.cleanup(runner);
                        self.status.build_finished();
                        return Err(BuildError::new(e));
                    }
                    if self.state.is_phony(edge) {
                        if let Err(e) = self.plan_edge_finished(edge, EdgeResult::Succeeded) {
                            self.cleanup(runner);
                            self.status.build_finished();
                            return Err(BuildError::new(e));
                        }
                    } else {
                        pending_commands += 1;
                        capacity -= 1;
                        capacity = capacity.min(runner.can_run_more());
                    }
                }
                // We are finished with all work items and have no pending
                // commands. Therefore, break out of the main loop.
                if pending_commands == 0 && !self.plan.more_to_do() {
                    break;
                }
            }

            // See if we can reap any finished commands.
            if pending_commands > 0 {
                let Some(result) = runner.wait_for_command().await else {
                    self.cleanup(runner);
                    self.status.build_finished();
                    return Err(BuildError::new("internal error: runner lost a command"));
                };
                pending_commands -= 1;
                if result.status == ExitStatus::Interrupted {
                    self.cleanup(runner);
                    self.status.build_finished();
                    return Err(BuildError { message: "interrupted by user".into(), interrupted: true });
                }
                let success = match self.finish_command(result) {
                    Ok(success) => success,
                    Err(e) => {
                        self.cleanup(runner);
                        self.status.build_finished();
                        return Err(BuildError::new(e));
                    }
                };
                if !success && failures_allowed > 0 {
                    failures_allowed -= 1;
                }
                // We made some progress; start the main loop over.
                continue;
            }

            // If we get here, we cannot make any more progress.
            self.status.build_finished();
            let msg = if failures_allowed == 0 {
                if self.config.failures_allowed > 1 { "subcommands failed" } else { "subcommand failed" }
            } else if failures_allowed < self.config.failures_allowed {
                "cannot make progress due to previous errors"
            } else {
                "stuck [this is a bug]"
            };
            return Err(BuildError::new(msg));
        }
        self.status.build_finished();
        let _ = self.disk.remove_file(&self.lock_file_path);
        Ok(())
    }

    fn start_edge(&mut self, edge: EdgeId, runner: &mut dyn CommandRunner) -> Result<(), String> {
        if self.state.is_phony(edge) {
            return Ok(());
        }
        let start_ms = self.elapsed_ms();
        self.running.insert(edge, start_ms);
        let info = self.edge_info(edge);
        self.status.build_edge_started(&info, start_ms);

        let mut build_start: TimeStamp = if self.config.dry_run { 0 } else { -1 };
        // Create directories necessary for outputs and remember the current
        // filesystem mtime to record later.
        let outputs = self.state.edge(edge).outputs.clone();
        for &o in &outputs {
            if !self.config.dry_run {
                self.disk.make_dirs(self.state.path(o))?;
            }
            if build_start == -1 {
                let _ = self.disk.make_dirs(&self.lock_file_path);
                let _ = self.disk.write_file(&self.lock_file_path, b".");
                build_start = self.disk.stat(&self.lock_file_path).unwrap_or(0);
                if build_start == -1 {
                    build_start = 0;
                }
            }
        }
        self.state.edge_mut(edge).command_start_time = build_start;

        // Create depfile directory if needed.
        let depfile = self.state.unescaped_depfile(edge);
        if !depfile.is_empty() && !self.config.dry_run {
            self.disk.make_dirs(&depfile)?;
        }
        // Create response file, if needed.
        let rspfile = self.state.unescaped_rspfile(edge);
        let mut rsp = None;
        if !rspfile.is_empty() {
            let content = self.state.edge_binding(edge, "rspfile_content")?;
            if !self.config.dry_run {
                self.disk.make_dirs(&rspfile)?;
                self.disk.write_file(&rspfile, content.as_bytes())?;
            }
            rsp = Some((rspfile, content));
        }

        // A phony input stands for its inputs (CMake's `install` depends on
        // `all`): those are what the command reads. A phony with none names
        // a file (CMake's rules for headers that may go away).
        let mut inputs: Vec<String> = Vec::new();
        let mut stack: Vec<NodeId> = self.state.edge(edge).non_order_only_inputs().iter().rev().copied().collect();
        let mut seen = std::collections::HashSet::new();
        while let Some(n) = stack.pop() {
            if !seen.insert(n) {
                continue;
            }
            match self.state.node(n).in_edge {
                Some(e) if self.state.is_phony(e) && !self.state.edge(e).non_order_only_inputs().is_empty() => {
                    stack.extend(self.state.edge(e).non_order_only_inputs().iter().rev().copied());
                }
                _ => inputs.push(self.state.path(n).to_string()),
            }
        }
        if !self.state.edge(edge).deps_added && !self.config.dry_run {
            // Deps an earlier build discovered, as hints: remote actions
            // send them, local ones download them first.
            for p in self.scanner().discovered_dep_paths(edge) {
                if !inputs.contains(&p) {
                    inputs.push(p);
                }
            }
        }
        let bindings = self.task_bindings(edge)?;
        let deps = self.state.edge_binding(edge, "deps")?;
        let task = Task {
            edge,
            rule: self.state.rule_of(edge).name.clone(),
            command: info.command.clone(),
            description: info.description.clone(),
            use_console: info.use_console,
            rspfile: rsp,
            depfile: if depfile.is_empty() { None } else { Some(depfile) },
            deps: if deps.is_empty() { None } else { Some(deps) },
            msvc_deps_prefix: self.state.edge_binding(edge, "msvc_deps_prefix")?,
            outputs: outputs.iter().map(|&o| self.state.path(o).to_string()).collect(),
            inputs,
            restat: self.state.edge_binding_bool(edge, "restat"),
            generator: self.state.edge_binding_bool(edge, "generator"),
            bindings,
        };
        runner.start_command(task).map_err(|e| format!("command '{}' failed: {e}", info.command))
    }

    /// Action rewinding: `edge` couldn't read `lost` inputs, outputs of
    /// earlier edges that only existed in a remote store and are gone. Those
    /// edges run again, and `edge` waits for them like any edge for its
    /// inputs. Returns false (the edge fails) when an input has no edge to
    /// make it, or after `MAX_REWINDS` attempts.
    fn rewind(&mut self, edge: EdgeId, lost: &[String]) -> Result<bool, String> {
        let attempts = self.rewinds.entry(edge).or_default();
        if *attempts >= MAX_REWINDS {
            return Ok(false);
        }
        *attempts += 1;
        let mut producers = Vec::new();
        for path in lost {
            let (canonical, _) = canonicalize_path(path);
            let Some(node) = self.state.lookup_node(&canonical) else { return Ok(false) };
            let Some(producer) = self.state.node(node).in_edge else { return Ok(false) };
            producers.push((node, producer));
        }
        // The edge was started: it frees its pool slot and waits again.
        self.running.remove(&edge);
        let pool = self.state.edge(edge).pool;
        self.pool_edge_finished(edge);
        self.pool_retrieve_ready_edges(pool);
        self.plan.want.insert(edge, Want::ToStart);
        let info = self.edge_info(edge);
        self.status.build_edge_rewound(&info, lost);
        for (node, producer) in producers {
            let n = self.state.node_mut(node);
            n.dirty = true;
            n.mtime = 0;
            n.exists = Existence::Missing;
            self.state.edge_mut(producer).outputs_ready = false;
            // Already planned to run (another consumer rewound it): wait.
            if matches!(self.plan.want.get(&producer), Some(Want::ToStart | Want::ToFinish)) {
                continue;
            }
            self.plan.want.insert(producer, Want::ToStart);
            self.edge_wanted(producer);
            if self.state.all_inputs_ready(producer) {
                self.schedule_work(producer);
            }
        }
        Ok(true)
    }

    /// Returns whether the command (including deps extraction) succeeded.
    fn finish_command(&mut self, mut result: TaskResult) -> Result<bool, String> {
        let edge = result.edge;
        // Inputs lost from the remote store: their edges run again, then
        // this one.
        if !result.lost_inputs.is_empty() && self.rewind(edge, &result.lost_inputs)? {
            return Ok(true);
        }
        // First try to extract dependencies from the result, if any. This must
        // happen first as it filters the command output (we want to filter
        // /showIncludes output, even on compile failure) and extraction itself
        // can fail, which makes the command fail from a build perspective.
        let mut deps_nodes: Vec<NodeId> = Vec::new();
        let deps_type = self.state.edge_binding(edge, "deps")?;
        if !deps_type.is_empty() {
            let prefix = self.state.edge_binding(edge, "msvc_deps_prefix")?;
            match self.extract_deps(&mut result, &deps_type, &prefix) {
                Ok(nodes) => deps_nodes = nodes,
                Err(e) => {
                    if result.status.success() {
                        if !result.output.is_empty() {
                            result.output.push(b'\n');
                        }
                        result.output.extend_from_slice(e.as_bytes());
                        result.status = ExitStatus::Failure(1);
                    }
                }
            }
        }

        let start_ms = self.running.remove(&edge).unwrap_or(0);
        let end_ms = self.elapsed_ms();
        let mut info = self.edge_info(edge);
        info.cas_outputs = std::mem::take(&mut result.cas_outputs);
        let code = match result.status {
            ExitStatus::Success => None,
            ExitStatus::Failure(c) => Some(c),
            ExitStatus::Interrupted => Some(130),
        };
        self.status.build_edge_finished(&info, end_ms, code, &result.kind, &result.output);

        // The rest of this function only applies to successful commands.
        if !result.status.success() {
            self.plan_edge_finished(edge, EdgeResult::Failed)?;
            return Ok(false);
        }

        // Restat the edge outputs.
        let mut record_mtime: TimeStamp = 0;
        if !self.config.dry_run {
            let restat = self.state.edge_binding_bool(edge, "restat");
            let generator = self.state.edge_binding_bool(edge, "generator");
            let mut node_cleaned = false;
            record_mtime = self.state.edge(edge).command_start_time;
            // restat and generator rules must restat the outputs after the
            // build has finished. If record_mtime == 0, then there was an error
            // while attempting to touch/stat the temp file when the edge
            // started and we should fall back to recording the outputs'
            // current mtime in the log.
            if record_mtime == 0 || restat || generator {
                let outputs = self.state.edge(edge).outputs.clone();
                for o in outputs {
                    let new_mtime = self.disk.stat(self.state.path(o))?;
                    if new_mtime > record_mtime {
                        record_mtime = new_mtime;
                    }
                    if self.state.node(o).mtime == new_mtime && restat {
                        // The rule command did not change the output. Propagate
                        // the clean state through the build graph. Note that
                        // this also applies to nonexistent outputs (mtime == 0).
                        self.clean_node(o)?;
                        node_cleaned = true;
                    }
                }
            }
            if node_cleaned {
                record_mtime = self.state.edge(edge).command_start_time;
            }
        }

        self.plan_edge_finished(edge, EdgeResult::Succeeded)?;

        // Delete any left over response file.
        let rspfile = self.state.unescaped_rspfile(edge);
        if !rspfile.is_empty() && !self.config.keep_rsp && !self.config.dry_run {
            let _ = self.disk.remove_file(&rspfile);
        }

        if !self.config.dry_run {
            let command = self.state.evaluate_command(edge, true)?;
            let hash = hash_command(&command);
            let outputs = self.state.edge(edge).outputs.clone();
            if let Some(log) = self.build_log.as_deref_mut() {
                for o in &outputs {
                    log.record(LogEntry {
                        output: self.state.path(*o).to_string(),
                        command_hash: hash,
                        start_time: start_ms,
                        end_time: end_ms,
                        mtime: record_mtime,
                    })?;
                }
            }
            if !deps_type.is_empty()
                && let Some(log) = self.deps_log.as_deref_mut()
            {
                let deps: Vec<String> = deps_nodes.iter().map(|&n| self.state.path(n).to_string()).collect();
                for o in outputs {
                    let deps_mtime = self.disk.stat(self.state.path(o))?;
                    log.record_deps(self.state.path(o), deps_mtime, &deps)?;
                }
            }
        }
        Ok(true)
    }

    fn extract_deps(&mut self, result: &mut TaskResult, deps_type: &str, prefix: &str) -> Result<Vec<NodeId>, String> {
        let mut nodes = Vec::new();
        match deps_type {
            "msvc" => {
                let output = String::from_utf8_lossy(&result.output).into_owned();
                let parsed = parse_cl_output(&output, prefix, &self.config.cwd, cfg!(windows));
                result.output = parsed.filtered_output.into_bytes();
                for inc in parsed.includes {
                    // Assume all separators may be backslashes, as ninja does.
                    nodes.push(self.state.get_node(&inc, if cfg!(windows) { u64::MAX } else { 0 }));
                }
            }
            "gcc" | "zero" => {
                let depfile = self.state.unescaped_depfile(result.edge);
                if depfile.is_empty() {
                    return Err(format!("edge with deps={deps_type} but no depfile makes no sense"));
                }
                // Read depfile content. Treat a missing depfile as empty.
                let content = self.disk.read_file(&depfile)?.unwrap_or_default();
                if content.is_empty() {
                    return Ok(nodes);
                }
                if deps_type == "gcc" {
                    let d = parse_depfile(&content)?;
                    for i in d.ins {
                        let (p, bits) = canonicalize_path(&i);
                        nodes.push(self.state.get_node(&p, bits));
                    }
                } else {
                    for piece in content.split(|&b| b == 0).filter(|p| !p.is_empty()) {
                        let (p, bits) = canonicalize_path(&String::from_utf8_lossy(piece));
                        nodes.push(self.state.get_node(&p, bits));
                    }
                }
                if !self.config.keep_depfile {
                    self.disk.remove_file(&depfile).map_err(|e| format!("deleting depfile: {e}\n"))?;
                }
            }
            other => return Err(format!("unknown deps type '{other}'")),
        }
        Ok(nodes)
    }

    /// On failure or interruption: kills running commands and deletes their
    /// possibly half-written outputs.
    fn cleanup(&mut self, runner: &mut dyn CommandRunner) {
        let active = runner.active_edges();
        runner.abort();
        for e in active {
            let depfile = self.state.unescaped_depfile(e);
            let outputs = self.state.edge(e).outputs.clone();
            for o in outputs {
                // Only delete this output if it was actually modified. This is
                // important for things like the generator where we don't want
                // to delete the manifest file if we can avoid it. But if the
                // rule uses a depfile, always delete.
                let path = self.state.path(o).to_string();
                match self.disk.stat(&path) {
                    Ok(new_mtime) => {
                        if !depfile.is_empty() || self.state.node(o).mtime != new_mtime {
                            let _ = self.disk.remove_file(&path);
                        }
                    }
                    Err(e) => self.status.error(&e),
                }
            }
            if !depfile.is_empty() {
                let _ = self.disk.remove_file(&depfile);
            }
        }
        if self.disk.stat(&self.lock_file_path).unwrap_or(0) > 0 {
            let _ = self.disk.remove_file(&self.lock_file_path);
        }
    }
}

/// Convenience used by tools: the evaluated command of an edge.
pub fn edge_command(state: &State, e: EdgeId) -> String {
    state.evaluate_command(e, false).unwrap_or_default()
}

/// Result helper for tests and the CLI.
pub fn exec_kind_name(k: &ExecKind) -> &'static str {
    match k {
        ExecKind::Local => "local",
        ExecKind::Remote => "remote",
        ExecKind::RemoteCacheHit => "remote-cache-hit",
        ExecKind::DiskCacheHit => "disk-cache-hit",
        ExecKind::DryRun => "dry-run",
    }
}
