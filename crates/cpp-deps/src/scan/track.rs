//! Tracking unknown values, and replaying the macros text pushes and pops.
//!
//! What the scan can't know (an unanswered query, `__COUNTER__`, an
//! expansion of cl's traditional preprocessor the model can't follow, a
//! macro text may push or pop) is an unknown value rather than a reason to
//! give up on the whole scan. It matters only where it may change which
//! files the compiler reads:
//!
//! - A macro is unknown (`Tracking::unknown`) when it changes in an
//!   uncertain region, when text may push or pop it, or when it's popped
//!   from a stack that is unknown. It may then be any definition it had in
//!   this scan (`Tracking::alts`), or none. An expansion reading it is
//!   unknown as a whole (`Expander::tainted`).
//! - A condition of unknown value (one that `1 || x` and `0 && x` don't
//!   decide) makes its branches an uncertain region: every one is processed
//!   (the list is the union of what each reads, a superset), and every macro
//!   defined, undefined, pushed or popped there is unknown afterwards. So
//!   are files that may be skipped: a file with `#pragma once` run in an
//!   uncertain region ("maybe once"), a copy of a `#pragma once` file, and
//!   the two files Clang may pick for an `#include_next`.
//! - The scan still gives up where the file read can't be named: an
//!   `#include` operand reading an unknown value, an unknown condition from
//!   which `__has_include`, `__has_include_next` or `__has_embed` may run,
//!   a `_Pragma` operand or a macro's pragmas reading one, and a union of
//!   more than `UNION_BOUND` files first read in uncertain regions.
//!
//! `CPP_DEPS_TRACKING=off` turns tracking and the replay off, for
//! diagnosis: the scan is then inexact wherever a value is unknown.

use super::*;

/// The most files first read (or found) inside uncertain regions: past it,
/// the union is too loose to be worth sending, and the compiler's list is
/// used.
pub const UNION_BOUND: usize = 64;

/// Whether scans track unknown values and replay the macros text pushes
/// and pops: unless `CPP_DEPS_TRACKING` is `off` (or `0`), for diagnosis,
/// or this thread said otherwise (`set_tracking`).
pub fn tracking() -> bool {
    if let Some(v) = TRACKING.with(|o| o.get()) {
        return v;
    }
    static ENV: OnceLock<bool> = OnceLock::new();
    *ENV.get_or_init(|| !matches!(std::env::var("CPP_DEPS_TRACKING").as_deref(), Ok("off" | "0")))
}

thread_local! {
    static TRACKING: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// This thread's scans track unknown values or not (None: as
/// `CPP_DEPS_TRACKING` says). For tests and measurements.
pub fn set_tracking(v: Option<bool>) {
    TRACKING.with(|o| o.set(v));
}

/// A region the compiler may or may not process: the branches of a
/// condition of unknown value, a file that may be skipped.
pub(super) struct Region {
    id: u64,
    files_before: usize,
    found_before: usize,
    /// Includes and embeds met (also in nested regions).
    includes: usize,
}

/// The state of tracking unknown values (see the module comment).
#[derive(Default)]
pub(super) struct Tracking {
    /// Macros that can change text's parenthesis nesting, and their users.
    context: Option<ContextMacros>,
    /// Macros whose definition is unknown, and why.
    unknown: SymMap<String>,
    /// The definitions of macros that are or may become unknown (changed in
    /// uncertain regions, pushed or popped in text, popped from unknown
    /// stacks): what an unknown one may be, besides undefined.
    alts: SymMap<Vec<Arc<Macro>>>,
    /// Macros changed in uncertain regions: the region of the last change,
    /// and the definition it certainly gave there (a `#define`, a pop from
    /// a known stack), if any (see `join_guard`).
    last: SymMap<(u64, Option<Arc<Macro>>)>,
    regions: Vec<Region>,
    next_region: u64,
    /// Files with `#pragma once` run in an uncertain region, by key,
    /// identity and contents: skipped or not.
    maybe_once: HashSet<String>,
    maybe_once_ids: HashSet<FileId>,
    maybe_once_contents: HashSet<(u64, u64)>,
    /// Macros whose `push_macro` stack is unknown (pushed or popped in an
    /// uncertain region).
    stack_unknown: SymSet,
    /// The depths in `State::pushed` of the definitions unknown when pushed.
    pushed_unknown: SymMap<Vec<usize>>,
    /// Files first read or found inside uncertain regions, and the regions
    /// that read or looked for files.
    union_files: usize,
    union_regions: usize,
    /// A macro defined in this scan names `__has_include`, `__has_embed`, or
    /// pastes and names a part of one (see `may_test_files`); the same of
    /// the predefined ones, once asked.
    bodies_test_files: bool,
    predefined_test_files: Option<bool>,
}

/// A conservative, monotone expansion graph. A macro with unbalanced
/// parentheses, one expanding it, or a paste that could form its name can
/// move a pusher into a discarded argument. Check the file's cached names
/// only when this graph gains a dangerous name, not for every text token.
#[derive(Default)]
struct ContextMacros {
    /// Until the first unbalanced body, there is no dangerous name to
    /// propagate. Retain earlier definitions (including overwritten ones)
    /// so a later definition can still make their users dangerous.
    pending: Vec<Arc<Macro>>,
    active: bool,
    users: SymMap<Vec<Sym>>,
    unsafe_names: SymSet,
    pastes: SymSet,
    hashes: Vec<u64>,
}

impl ContextMacros {
    fn add(&mut self, m: &Arc<Macro>) -> bool {
        if !self.active {
            if !m.context.unbalanced {
                self.pending.push(m.clone());
                return false;
            }
            self.active = true;
            for earlier in std::mem::take(&mut self.pending) {
                self.add_edges(&earlier);
            }
        }
        self.add_edges(m)
    }

    fn add_edges(&mut self, m: &Macro) -> bool {
        let mut unsafe_body = m.context.unbalanced;
        let new_paste = m.context.pastes && self.pastes.insert(m.name);
        for &n in &m.context.names {
            let users = self.users.entry(n).or_default();
            if !users.contains(&m.name) {
                users.push(m.name);
            }
            unsafe_body |= self.unsafe_names.contains(&n);
        }
        let before = self.unsafe_names.len();
        let mut todo = Vec::new();
        if unsafe_body {
            todo.push(m.name);
        }
        if before == 0 && !todo.is_empty() {
            todo.extend(self.pastes.iter().copied());
        } else if before != 0 && new_paste {
            todo.push(m.name);
        }
        while let Some(n) = todo.pop() {
            if self.unsafe_names.insert(n) {
                self.hashes.push(lex::name_hash(n.as_str().as_bytes()));
                todo.extend(self.users.get(&n).into_iter().flatten().copied());
            }
        }
        before != self.unsafe_names.len()
    }
}

/// Whether a macro's body may make `__has_include`, `__has_include_next`
/// or `__has_embed`: it names one, or pastes and names a part of one.
fn body_tests_files(m: &Macro) -> bool {
    let params = m.params.as_deref().unwrap_or(&[]);
    m.body.iter().any(|t| HAS_OPS.contains(&t.sym))
        || (m.body.iter().any(|t| t.is(P::HashHash)) && m.body.iter().filter_map(Token::ident).any(|n| !params.contains(&n) && part_of_has_op(n)))
}

/// A part starting a pusher's name, one ending it (see `spells_ends`).
const PREFIX: u8 = 1;
const SUFFIX: u8 = 2;

const HAS_OPS: [Sym; 3] = [intern::HAS_INCLUDE, intern::HAS_INCLUDE_NEXT, intern::HAS_EMBED];

fn part_of_has_op(n: Sym) -> bool {
    !n.as_str().is_empty() && HAS_OPS.iter().any(|o| o.as_str().contains(n.as_str()))
}

/// The include guard an `#ifndef G` or `#if !defined(G)` tests.
pub(super) fn guard_of(d: &Directive) -> Option<Sym> {
    match (d.kind, &d.toks[..]) {
        (DirKind::Ifndef, [g, ..]) => g.ident(),
        (DirKind::If, [n, d, g]) if n.is(P::Not) && d.sym == intern::DEFINED => g.ident(),
        (DirKind::If, [n, d, l, g, r]) if n.is(P::Not) && d.sym == intern::DEFINED && l.is(P::LParen) && r.is(P::RParen) => g.ident(),
        _ => None,
    }
}

/// Whether a file runs `#pragma once` whenever it's processed: outside any
/// conditional.
pub(super) fn once_always(fd: &FileDirectives) -> bool {
    let mut depth = 0usize;
    for d in &fd.dirs {
        match d.kind {
            DirKind::If | DirKind::Ifdef | DirKind::Ifndef => depth += 1,
            DirKind::Endif => depth = depth.saturating_sub(1),
            DirKind::Pragma if !d.from_text && depth == 0 && d.toks.first().is_some_and(|t| t.sym == intern::ONCE) => return true,
            _ => {}
        }
    }
    false
}

/// What `once_elsewhere` decided.
#[derive(PartialEq, Eq)]
pub(super) enum Elsewhere {
    /// Not a copy of a `#pragma once` file.
    No,
    /// Skipped, as the compiler does here (including it again changes
    /// nothing, or the scan is inexact).
    Skip,
    /// Skipped here, maybe not where copies differ: an uncertain region.
    Uncertain,
}

impl State<'_> {
    pub(super) fn is_unknown_macro(&self, name: Sym) -> bool {
        self.taint.as_ref().is_some_and(|t| !t.unknown.is_empty() && t.unknown.contains_key(&name))
    }

    pub(super) fn in_region(&self) -> bool {
        self.taint.as_ref().is_some_and(|t| !t.regions.is_empty())
    }

    /// Enters an uncertain region; its number.
    pub(super) fn enter_region(&mut self) -> u64 {
        let (files_before, found_before) = (self.out.files.len(), self.out.found.len());
        let Some(t) = &mut self.taint else { return 0 };
        t.next_region += 1;
        let id = t.next_region;
        t.regions.push(Region { id, files_before, found_before, includes: 0 });
        id
    }

    pub(super) fn exit_region(&mut self) {
        let (files, found) = (self.out.files.len(), self.out.found.len());
        let Some(t) = &mut self.taint else { return };
        let Some(r) = t.regions.pop() else { return };
        if r.includes > 0 || files > r.files_before || found > r.found_before {
            t.union_regions += 1;
        }
        if let Some(parent) = t.regions.last_mut() {
            parent.includes += r.includes;
        }
    }

    /// An include, import or embed: in an uncertain region, the scan's
    /// exactness rests on a union of files.
    pub(super) fn region_include(&mut self) {
        if let Some(r) = self.taint.as_mut().and_then(|t| t.regions.last_mut()) {
            r.includes += 1;
        }
    }

    /// A file read or found for the first time.
    pub(super) fn first_read(&mut self) {
        if let Some(t) = &mut self.taint
            && !t.regions.is_empty()
        {
            t.union_files += 1;
            if t.union_files > UNION_BOUND {
                self.inexact(format!("the files read where the scan couldn't tell whether the compiler reads them are too many: over {UNION_BOUND}"));
            }
        }
    }

    /// Whether a file was marked `#pragma once` in an uncertain region (by
    /// key, identity or contents): it may be skipped.
    pub(super) fn maybe_once(&self, key: &str, id: FileId, contents: (u64, u64)) -> bool {
        self.taint.as_ref().is_some_and(|t| {
            !t.maybe_once_contents.is_empty()
                && (t.maybe_once.contains(key) || (id != (0, 0) && t.maybe_once_ids.contains(&id)) || t.maybe_once_contents.contains(&contents))
        })
    }

    /// `#pragma once` (or `#import`) run in an uncertain region: true when
    /// noted so (the file may be skipped later).
    pub(super) fn mark_maybe_once(&mut self, key: &str, id: FileId, contents: (u64, u64)) -> bool {
        let Some(t) = &mut self.taint else { return false };
        if t.regions.is_empty() {
            return false;
        }
        t.maybe_once.insert(key.to_string());
        if id != (0, 0) {
            t.maybe_once_ids.insert(id);
        }
        t.maybe_once_contents.insert(contents);
        true
    }

    /// Notes a definition an unknown macro may have.
    fn alt(t: &mut Tracking, name: Sym, m: &Arc<Macro>) -> bool {
        let alts = t.alts.entry(name).or_default();
        if !alts.iter().any(|a| Arc::ptr_eq(a, m)) {
            alts.push(m.clone());
            // A saved definition can first become possible when an
            // unplaced pop restores it, after the graph was initialized.
            return t.context.as_mut().is_some_and(|c| c.add(m));
        }
        false
    }

    /// Before a macro changes: its definition is one it may have later, when
    /// it's (or will be) unknown.
    pub(super) fn track_before(&mut self, name: Sym) {
        let Some(t) = &mut self.taint else { return };
        let text = !self.pushed_in_text.is_empty() && self.pushed_in_text.contains(&name);
        let unknown = !t.unknown.is_empty() && t.unknown.contains_key(&name);
        let stack = !t.stack_unknown.is_empty() && t.stack_unknown.contains(&name);
        if t.regions.is_empty() && !text && !unknown && !stack {
            return;
        }
        if let Some(m) = self.macros.get(&name) {
            self.pushers_gen += usize::from(Self::alt(t, name, m));
        }
    }

    /// After a macro was set (defined, undefined, popped): unknown in an
    /// uncertain region, when text may push or pop it (a pop there may
    /// restore another definition), or when `unknown` (popped from an
    /// unknown stack, `why`); known otherwise.
    pub(super) fn track_after(&mut self, name: Sym, unknown: bool, why: &str) {
        let Some(t) = &mut self.taint else { return };
        let region = t.regions.last().map(|r| r.id);
        let text = !self.pushed_in_text.is_empty() && self.pushed_in_text.contains(&name);
        if region.is_none() && !text && !unknown {
            if !t.unknown.is_empty() {
                t.unknown.remove(&name);
            }
            return;
        }
        let cur = self.macros.get(&name);
        if let Some(m) = cur {
            self.pushers_gen += usize::from(Self::alt(t, name, m));
        }
        if let Some(id) = region {
            t.last.insert(name, (id, cur.filter(|_| !unknown).cloned()));
        }
        t.unknown.entry(name).or_insert_with(|| {
            let cause = if region.is_some() {
                "changed in an uncertain region"
            } else if text {
                "changed while text may push or pop it"
            } else {
                why
            };
            let here = self.stack.last().map(|f| format!("{}:{}", f.path.display(), f.line)).unwrap_or_default();
            format!("{} {cause} at {here}", name.as_str())
        });
    }

    /// Why a macro is unknown (for messages).
    pub(super) fn why_unknown(&self, name: Sym) -> String {
        self.taint.as_ref().and_then(|t| t.unknown.get(&name)).cloned().unwrap_or_else(|| name.as_str().to_string())
    }

    /// Macros text may push or pop, from now on: their definitions are
    /// tracked.
    pub(super) fn pushed_in_text_add(&mut self, names: &[Sym]) {
        for &n in names {
            let newly_unknown = !self.is_unknown_macro(n);
            self.pushed_in_text.insert(n);
            // An unplaced pop can restore a value immediately, without a
            // later #define or #undef. Its stack may also be different.
            self.stack_uncertain(n);
            self.track_after(n, true, "an unplaced push or pop in text");
            if newly_unknown {
                self.changed(n);
            }
        }
    }

    /// A macro's `push_macro` stack becomes unknown: what it holds, and its
    /// definition now, are definitions it may have.
    fn stack_uncertain(&mut self, name: Sym) {
        let Some(t) = &mut self.taint else { return };
        if !t.stack_unknown.insert(name) {
            return;
        }
        for m in self.pushed.get(&name).into_iter().flatten().flatten().chain(self.macros.get(&name)) {
            self.pushers_gen += usize::from(Self::alt(t, name, m));
        }
    }

    /// `#pragma push_macro("name")` (`push`) or `pop_macro`, in a directive,
    /// in text through a macro replayed, or in a `_Pragma`.
    pub(super) fn push_pop(&mut self, push: bool, name: Sym) {
        let region = self.in_region();
        if region {
            self.stack_uncertain(name);
        }
        if push {
            self.track_before(name);
            let cur = self.macros.get(&name).cloned();
            let unknown = self.is_unknown_macro(name);
            let stack = self.pushed.entry(name).or_default();
            if unknown && let Some(t) = &mut self.taint {
                t.pushed_unknown.entry(name).or_default().push(stack.len());
            }
            stack.push(cur);
            return;
        }
        let stack_unknown = self.taint.as_ref().is_some_and(|t| !t.stack_unknown.is_empty() && t.stack_unknown.contains(&name));
        let stack = self.pushed.get_mut(&name);
        if let Some(prev) = stack.and_then(|s| s.pop().map(|p| (p, s.len()))) {
            let (prev, depth) = prev;
            let mut unknown = stack_unknown;
            if let Some(t) = &mut self.taint
                && !t.pushed_unknown.is_empty()
                && let Some(depths) = t.pushed_unknown.get_mut(&name)
                && depths.last() == Some(&depth)
            {
                depths.pop();
                unknown = true;
            }
            self.track_before(name);
            self.changing(name);
            match prev {
                Some(m) => {
                    self.macros.insert(name, m.clone());
                    self.track_after(name, unknown, "popped from an unknown stack");
                    self.defined(&m);
                }
                None => {
                    self.macros.remove(&name);
                    self.undefined.insert(name);
                    self.track_after(name, unknown, "popped from an unknown stack");
                }
            }
            self.changed(name);
        } else if stack_unknown {
            // The stack may hold a definition: the macro may change.
            self.track_before(name);
            self.track_after(name, true, "popped from an unknown stack");
            self.changed(name);
        }
    }

    /// Whether expanding `toks` may run `__has_include`, `__has_include_next`
    /// or `__has_embed` (a file tested), with any definition the macros
    /// reached may have: those named, or made by pastes.
    pub(super) fn may_test_files(&mut self, toks: &[Token]) -> bool {
        let (direct, pastes, parts) = self.reaches_operators(toks, &HAS_OPS);
        direct || (pastes && (parts || self.some_body_tests_files()))
    }

    /// Pragmas in a condition can change the macro state used by later
    /// directives. Treat hidden/pasted operators as effects too, including
    /// operators in definitions an unknown macro may have.
    pub(super) fn may_run_pragma(&self, toks: &[Token]) -> bool {
        let ops = [intern::PRAGMA_OPERATOR, intern::MS_PRAGMA_OPERATOR];
        let (direct, pastes, parts) = self.reaches_operators(toks, &ops);
        direct
            || (pastes
                && (parts
                    || self.macros.values().chain(self.cfg.predefined.values()).chain(self.taint.iter().flat_map(|t| t.alts.values().flatten())).any(|m| {
                        let params = m.params.as_deref().unwrap_or(&[]);
                        m.body.iter().any(|t| ops.contains(&t.sym))
                            || (m.body.iter().any(|t| t.is(P::HashHash))
                                && m.body.iter().filter_map(Token::ident).any(|n| !params.contains(&n) && ops.iter().any(|o| o.as_str().contains(n.as_str()))))
                    })))
    }

    fn reaches_operators(&self, toks: &[Token], ops: &[Sym]) -> (bool, bool, bool) {
        let mut stack: Vec<Sym> = toks.iter().filter_map(Token::ident).collect();
        let mut seen = HashSet::new();
        let (mut pastes, mut parts) = (false, false);
        let empty = Vec::new();
        while let Some(n) = stack.pop() {
            if ops.contains(&n) {
                return (true, false, false);
            }
            parts |= !n.as_str().is_empty() && ops.iter().any(|o| o.as_str().contains(n.as_str()));
            if !seen.insert(n) {
                continue;
            }
            if seen.len() > 100_000 {
                return (true, false, false);
            }
            let alts = self.taint.as_ref().filter(|t| t.unknown.contains_key(&n)).and_then(|t| t.alts.get(&n)).unwrap_or(&empty);
            for m in self.macros.get(&n).into_iter().chain(alts) {
                let params = m.params.as_deref().unwrap_or(&[]);
                for t in m.body.iter() {
                    pastes |= t.is(P::HashHash);
                    if let Some(i) = t.ident()
                        && !params.contains(&i)
                    {
                        stack.push(i);
                    }
                }
            }
        }
        // A paste may make an operator of its parts, or the name of a macro
        // whose body has one.
        (false, pastes, parts)
    }

    /// Whether a macro defined in this scan, or a predefined one, may make
    /// a file test (see `body_tests_files`).
    fn some_body_tests_files(&mut self) -> bool {
        let predefined = self.cfg.predefined.clone();
        let Some(t) = &mut self.taint else { return true };
        t.bodies_test_files || *t.predefined_test_files.get_or_insert_with(|| predefined.values().any(|m| body_tests_files(m)))
    }

    /// A macro is defined: one whose body may make a file test is noted.
    pub(super) fn note_body(&mut self, m: &Macro) {
        if let Some(t) = &mut self.taint
            && !t.bodies_test_files
            && body_tests_files(m)
        {
            t.bodies_test_files = true;
        }
    }

    /// At the `#endif` of an uncertain `#ifndef G` (or `#if !defined(G)`)
    /// without other branches, `region` its region: G is defined either
    /// way. When the region certainly defined it last, as every definition
    /// G may have had before, G is known again (so that the file it guards
    /// is skipped from now on).
    pub(super) fn join_guard(&mut self, g: Sym, region: u64) {
        if self.in_region() || self.pushed_in_text.contains(&g) {
            return;
        }
        let Some(cur) = self.macros.get(&g) else { return };
        let Some(t) = &mut self.taint else { return };
        if !t.unknown.contains_key(&g) || t.stack_unknown.contains(&g) {
            return;
        }
        let certain = matches!(t.last.get(&g), Some((id, Some(m))) if *id == region && Arc::ptr_eq(m, cur));
        if certain && t.alts.get(&g).is_none_or(|alts| alts.iter().all(|a| **a == **cur)) {
            t.unknown.remove(&g);
        }
    }

    /// At the end of a scan: the union's accounting (none for an inexact
    /// scan, which stopped tracking).
    pub(super) fn finish_tracking(&mut self) {
        let Some(t) = self.taint.take() else { return };
        self.out.union = (t.union_regions > 0).then_some(t.union_files);
    }

    // --- The replay of the macros text pushes and pops -------------------

    /// An object-like macro whose body pushes or pops macros (a "pusher",
    /// libc++'s `_LIBCPP_PUSH_MACROS`) is replayed where text uses it,
    /// rather than make the macros it names unknown. Not one whose name
    /// has a digit (a paste of a number could make it unseen), nor once a
    /// paste may make a pusher's name (see `spells_ends`): false then.
    pub(super) fn add_pusher(&mut self, m: &Macro, names: &[Sym]) -> bool {
        if !self.replay || m.params.is_some() || m.name.as_str().bytes().any(|b| b.is_ascii_digit()) {
            return false;
        }
        if self.pushers.get(&m.name).is_some_and(|n| names.iter().all(|name| n.contains(name))) {
            return true;
        }
        // Unknown definitions may target different stacks. The fallback
        // must affect every target, including an earlier definition's.
        let mut targets = self.pushers.get(&m.name).map(|n| n.to_vec()).unwrap_or_default();
        for name in names {
            if !targets.contains(name) {
                targets.push(*name);
            }
        }
        self.pushers.insert(m.name, targets.into());
        if self.taint.as_ref().is_some_and(|t| t.context.is_none()) {
            let mut context = ContextMacros::default();
            for m in self.macros.values() {
                context.add(m);
            }
            for m in self.taint.as_ref().unwrap().alts.values().flatten() {
                context.add(m);
            }
            self.taint.as_mut().unwrap().context = Some(context);
        }
        self.pushers_gen += 1;
        self.pusher_hashes.push(m.name_hash);
        let name = m.name.as_str();
        let mut hashes: HashMap<u64, u8> = self.pusher_end_hashes.iter().copied().collect();
        for (i, _) in name.char_indices().skip(1) {
            for (end, kind) in [(&name[..i], PREFIX), (&name[i..], SUFFIX)] {
                *self.pusher_ends.entry(intern::intern(end)).or_default() |= kind;
                *hashes.entry(lex::name_hash(end.as_bytes())).or_default() |= kind;
            }
        }
        self.pusher_end_hashes = hashes.into_iter().collect();
        self.pusher_end_hashes.sort_unstable();
        let mut print = std::hash::DefaultHasher::new();
        std::hash::Hash::hash(&self.pusher_end_hashes, &mut print);
        self.pusher_ends_print = (std::hash::Hasher::finish(&print) & !3) | 4;
        // Macros naming it push where text uses them, which isn't replayed;
        // those spelling the ends of its name may paste it.
        // (And the definitions unknown macros may have.)
        let alts = self.taint.as_ref().map(|t| t.alts.values().flatten().cloned().collect::<Vec<_>>()).unwrap_or_default();
        let macros: Vec<Arc<Macro>> = self.macros.values().cloned().chain(alts).collect();
        for o in &macros {
            if o.name != m.name && o.body.iter().any(|t| t.ident() == Some(m.name)) {
                self.pushed_in_text_add(names);
            }
            self.pusher_ends_in(o);
        }
        self.replay
    }

    /// A macro is defined while there are pushers: one naming a pusher (a
    /// wrapper) pushes where text uses it, which isn't replayed; one
    /// spelling the ends of a pusher's name may paste it.
    pub(super) fn pusher_named(&mut self, m: &Arc<Macro>) {
        if self.pushers.is_empty() || !self.replay {
            return;
        }
        if self.taint.as_mut().and_then(|t| t.context.as_mut()).is_some_and(|c| c.add(m)) {
            self.pushers_gen += 1;
        }
        let named: Vec<Box<[Sym]>> = m.context.names.iter().filter(|&&n| n != m.name).filter_map(|n| self.pushers.get(n).cloned()).collect();
        for names in named {
            self.pushed_in_text_add(&names);
        }
        self.pusher_ends_in(m);
    }

    /// Notes the ends of pushers' names a body spells (see `spells_ends`).
    fn pusher_ends_in(&mut self, m: &Macro) {
        if !self.replay || self.pusher_ends.is_empty() {
            return;
        }
        for n in &m.context.names {
            if let Some(&kind) = self.pusher_ends.get(n) {
                self.spelled_ends |= kind;
            }
        }
        self.spells_ends();
    }

    /// A paste makes a pusher's name only of a part spelled in a body or in
    /// text that starts it, and one that ends it (numbers excepted, which
    /// its name hasn't). Once both are spelled, the replay stops: every
    /// pusher's macros are pushed or popped in text, unplaced.
    fn spells_ends(&mut self) {
        if self.spelled_ends == PREFIX | SUFFIX && self.replay {
            self.replay = false;
            let all: Vec<Box<[Sym]>> = self.pushers.values().cloned().collect();
            for names in all {
                self.pushed_in_text_add(&names);
            }
        }
    }

    /// Notes the ends of pushers' names a file's text spells (see
    /// `spells_ends`).
    fn text_ends(&mut self, fd: &FileDirectives) {
        if self.taint.as_ref().and_then(|t| t.context.as_ref()).is_some_and(|c| c.hashes.iter().any(|h| fd.names.binary_search(h).is_ok())) {
            self.replay = false;
            let all: Vec<Box<[Sym]>> = self.pushers.values().cloned().collect();
            for names in all {
                self.pushed_in_text_add(&names);
            }
            return;
        }
        use std::sync::atomic::Ordering::Relaxed;
        let known = fd.ends.load(Relaxed);
        let spelled = if known & !3 == self.pusher_ends_print {
            (known & 3) as u8
        } else {
            let spelled = self.pusher_end_hashes.iter().filter(|(h, _)| fd.names.binary_search(h).is_ok()).fold(0, |s, (_, kind)| s | kind);
            fd.ends.store(self.pusher_ends_print | spelled as u64, Relaxed);
            spelled
        };
        self.spelled_ends |= spelled;
        self.spells_ends();
    }

    /// The uses of pushers in a file's text (read when it names one); those
    /// that can't be placed after line `after` push or pop in text, unplaced.
    fn replay_uses(&mut self, fd: &FileDirectives, after: u32) -> Option<Arc<[lex::TextUse]>> {
        if !self.replay || !self.pusher_hashes.iter().any(|h| fd.names.binary_search(h).is_ok()) {
            return None;
        }
        let real = self.stack.last()?.real.clone();
        let uses = fd
            .uses
            .get_or_init(|| {
                let content = std::fs::read(&real).ok().filter(|c| lex::content_hash(c) == fd.hash)?;
                let mut uses = lex::text_uses(&content);
                // A use on a directive's line shares it with a `_Pragma` the
                // lexer made a directive: its order with it is unknown.
                let lines: HashSet<u32> = fd.dirs.iter().map(|d| d.line).collect();
                for u in &mut uses {
                    u.placeable &= !lines.contains(&u.line);
                }
                Some(uses.into())
            })
            .clone();
        let Some(uses) = uses else {
            // Changed since: every pusher may push in text.
            let all: Vec<Box<[Sym]>> = self.pushers.values().cloned().collect();
            for names in all {
                self.pushed_in_text_add(&names);
            }
            return None;
        };
        self.replay_unplaceable(&uses, after);
        Some(uses)
    }

    /// Replays the uses of pushers in a file's text before `line` (after the
    /// directive at `prev_line`), when `active`; `uses`, `next_use` and
    /// `gen_seen` are the file's (see `Replay`).
    pub(super) fn replay_until(&mut self, fd: &FileDirectives, r: &mut Replay, line: u32, active: bool) {
        if r.gen_seen != self.pushers_gen {
            r.gen_seen = self.pushers_gen;
            if !self.pushers.is_empty() {
                self.text_ends(fd);
            }
            match &r.uses {
                None => {
                    r.uses = self.replay_uses(fd, r.prev_line);
                    if let Some(u) = &r.uses {
                        r.next_use = u.iter().position(|u| u.line > r.prev_line).unwrap_or(u.len());
                    }
                }
                Some(u) => {
                    let u = u.clone();
                    self.replay_unplaceable(&u, r.prev_line);
                }
            }
        }
        r.prev_line = line;
        if !self.replay {
            return;
        }
        let Some(u) = r.uses.clone() else { return };
        while r.next_use < u.len() && u[r.next_use].line < line {
            let x = u[r.next_use];
            r.next_use += 1;
            if active && x.placeable && self.pushers.contains_key(&x.sym) {
                self.replay_use(x.sym);
            }
        }
    }

    fn replay_unplaceable(&mut self, uses: &[lex::TextUse], after: u32) {
        let unplaceable: Vec<Box<[Sym]>> = uses.iter().filter(|u| u.line > after && !u.placeable).filter_map(|u| self.pushers.get(&u.sym).cloned()).collect();
        for names in unplaceable {
            self.pushed_in_text_add(&names);
        }
    }

    /// A pusher used in text, at its place: its expansion's pushes and
    /// pops, in order; when the expansion can't be read, its macros are
    /// pushed or popped in text, unplaced.
    fn replay_use(&mut self, name: Sym) {
        let Some(names) = self.pushers.get(&name).cloned() else { return };
        if self.is_unknown_macro(name) {
            self.pushed_in_text_add(&names);
            return;
        }
        let Some(m) = self.macros.get(&name).filter(|m| m.params.is_none()).cloned() else { return };
        // A body of pragmas only (libc++'s): its pushes and pops, once.
        let cached = self.pusher_ops.get(&name).filter(|(d, _)| Arc::ptr_eq(d, &m)).map(|(_, ops)| ops.clone());
        if let Some(ops) = cached.or_else(|| {
            let ops: Arc<[(bool, Sym)]> = pragma_ops(&m.body)?.into();
            self.pusher_ops.insert(name, (m.clone(), ops.clone()));
            Some(ops)
        }) {
            for &(push, n) in ops.iter() {
                self.push_pop(push, n);
            }
            return;
        }
        let mut x = Expander::new(self, false);
        let out = x.expand_tokens(&[Token { kind: Kind::Ident, sym: name, space: false }]);
        let ok = x.error.is_none() && !x.unsure() && x.unknown_values == 0;
        match pragma_pushes(&out).filter(|_| ok) {
            Some(ops) => {
                for (push, n) in ops {
                    self.push_pop(push, n);
                }
            }
            None => self.pushed_in_text_add(&names),
        }
    }
}

/// The pushes and pops of a body that is only `_Pragma("...")` and
/// `__pragma(...)` (as libc++'s `_LIBCPP_PUSH_MACROS`): nothing a macro
/// changes. None otherwise.
fn pragma_ops(body: &[Token]) -> Option<Vec<(bool, Sym)>> {
    if body.iter().any(|t| t.kind == Kind::Ident && t.sym != intern::PRAGMA_OPERATOR && t.sym != intern::MS_PRAGMA_OPERATOR) {
        return None;
    }
    pragma_pushes(body)
}

/// The pushes and pops of the pragmas in an expansion, in order; None when
/// one does something else that matters (see `macros::PRAGMAS`) or can't be
/// read.
fn pragma_pushes(out: &[Token]) -> Option<Vec<(bool, Sym)>> {
    let mut ops = Vec::new();
    let mut i = 0;
    while i < out.len() {
        let t = out[i];
        let words = match &out[i..] {
            [p, l, s, r, ..] if p.sym == intern::PRAGMA_OPERATOR && l.is(P::LParen) && s.kind == Kind::Str && r.is(P::RParen) => {
                i += 4;
                lex::tokens(macros::destringize(s.sym.as_str()).as_bytes())
            }
            [p, l, ..] if p.sym == intern::MS_PRAGMA_OPERATOR && l.is(P::LParen) => {
                let end = macros::closing(out, i + 1);
                if end >= out.len() {
                    return None;
                }
                let w = out[i + 2..end].to_vec();
                i = end + 1;
                w
            }
            _ if t.sym == intern::PRAGMA_OPERATOR || t.sym == intern::MS_PRAGMA_OPERATOR => return None,
            _ => {
                i += 1;
                continue;
            }
        };
        match &words[..] {
            [w, l, s, r] if (w.sym == intern::PUSH_MACRO || w.sym == intern::POP_MACRO) && l.is(P::LParen) && s.kind == Kind::Str && r.is(P::RParen) => {
                ops.push((w.sym == intern::PUSH_MACRO, intern::intern(&macros::destringize(s.sym.as_str()))));
            }
            [w, ..] if macros::PRAGMAS.contains(&w.sym) => return None,
            _ => {}
        }
    }
    Some(ops)
}

/// Where a file's replay is (see `State::replay_until`).
#[derive(Default)]
pub(super) struct Replay {
    uses: Option<Arc<[lex::TextUse]>>,
    next_use: usize,
    gen_seen: usize,
    prev_line: u32,
}

impl Replay {
    pub(super) fn new() -> Self {
        Replay { gen_seen: usize::MAX, ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_context_matches_eager_graph() {
        let definitions: Vec<_> = macros::parse_defines(
            "#define ALIAS TARGET\n#define TARGET IGNORE(\n#define CAT(a,b) a##b\n#define WRAPPER ALIAS\n#define TARGET\n#define CLOSE )(\n",
        )
        .into_iter()
        .map(Arc::new)
        .collect();
        // All orders exercise forward references, redefinitions, a paste
        // before/after activation, and a body with net-zero but unsafe nesting.
        fn permutations(defs: &[Arc<Macro>], order: &mut Vec<usize>) {
            if order.len() == defs.len() {
                let mut lazy = ContextMacros::default();
                let mut eager = ContextMacros { active: true, ..Default::default() };
                for &i in order.iter() {
                    assert_eq!(lazy.add(&defs[i]), eager.add(&defs[i]), "{order:?}, step {i}");
                    assert_eq!(lazy.unsafe_names, eager.unsafe_names, "{order:?}, step {i}");
                }
                return;
            }
            for i in 0..defs.len() {
                if !order.contains(&i) {
                    order.push(i);
                    permutations(defs, order);
                    order.pop();
                }
            }
        }
        permutations(&definitions, &mut Vec::new());
    }
}
