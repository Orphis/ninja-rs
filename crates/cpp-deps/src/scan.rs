//! Scanning a translation unit: the preprocessor's directives evaluated
//! exactly (macros, conditionals, includes), text skipped.
//!
//! A conditional the scanner can't decide (an unanswered compiler query, a
//! malformed expression) processes every branch it can't rule out, and the
//! result is marked inexact: it's then likely a superset, but no guarantee.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::expr;
use crate::intern::{self, Sym};
use crate::lex::{self, DirKind, Directive, FileDirectives, Kind, P, Token};
use crate::macros::{self, Context, Expander, Macro, XTok};

pub type MacroTable = HashMap<Sym, Arc<Macro>>;

/// Builds a macro table from `#define` lines (e.g. `-dM -E` output).
pub fn macro_table(defines: &str) -> MacroTable {
    macros::parse_defines(defines).into_iter().map(|m| (m.name, Arc::new(m))).collect()
}

/// A `-D` or `-U` of the command line, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandMacro {
    /// `NAME`, `NAME=VALUE` or `NAME(args)=VALUE`.
    Define(String),
    Undef(String),
}

/// What a compile's preprocessor is set up with.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// `true`/`false` and the alternative operators (`and`, `or`, ...).
    pub dialect: expr::Dialect,
    /// `-iquote`: searched by `#include "..."` only.
    pub quote_dirs: Vec<PathBuf>,
    /// `-I`, `-isystem`, the built-in directories, `-idirafter`, in order.
    pub angled_dirs: Vec<PathBuf>,
    /// MSVC: `#include "..."` searches the directories of every includer,
    /// innermost first, not only the current file's.
    pub msvc_lookup: bool,
    /// The compiler's predefined macros.
    pub predefined: Arc<MacroTable>,
    pub commands: Vec<CommandMacro>,
    /// `-imacros` and `-include` files, in the order they're processed.
    pub forced: Vec<PathBuf>,
    /// The `__has_*` operators the compiler has, besides `__has_include`.
    pub operators: Arc<HashSet<Sym>>,
}

/// Answers the compiler's queries (`__has_builtin(x)` and the like).
pub trait Oracle: Sync {
    /// None when unknown.
    fn query(&self, op: &str, arg: &str) -> Option<i64>;
}

/// An oracle that knows nothing.
pub struct NoOracle;

impl Oracle for NoOracle {
    fn query(&self, _: &str, _: &str) -> Option<i64> {
        None
    }
}

#[derive(Debug, Default)]
pub struct Output {
    /// Every file the preprocessor reads, the main file first.
    pub files: Vec<PathBuf>,
    /// Files whose existence `__has_include` checked and found (an input:
    /// absent remotely, the answer would differ).
    pub found: Vec<PathBuf>,
    /// `#include`s not found (an error for the compile, or a header not
    /// generated yet).
    pub missing: Vec<String>,
    /// Queries without an answer: (operator, argument).
    pub unknown: Vec<(String, String)>,
    /// Why the result may be incomplete.
    pub inexact: Vec<String>,
}

impl Output {
    pub fn exact(&self) -> bool {
        self.inexact.is_empty() && self.unknown.is_empty()
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub files_lexed: u64,
    pub files_reused: u64,
}

/// A directory's files (None: not a directory), and the session it's from.
type Listing = (u64, Option<Arc<HashSet<String>>>);

struct Entry {
    len: u64,
    mtime: Option<SystemTime>,
    session: u64,
    fd: Arc<FileDirectives>,
}

/// Lexed files and directory listings, shared by every scan (and thread).
/// Within a session, files and directories outside the volatile ones are
/// assumed unchanged; a new session checks them again.
pub struct Cache {
    files: Mutex<HashMap<PathBuf, Entry>>,
    dirs: Mutex<HashMap<PathBuf, Listing>>,
    session: AtomicU64,
    volatile: Vec<PathBuf>,
    case_insensitive: bool,
    pub stats: Mutex<Stats>,
}

impl Cache {
    /// `volatile`: directories whose contents may change during a session
    /// (the build directory). `case_insensitive`: the file system's names.
    pub fn new(volatile: Vec<PathBuf>, case_insensitive: bool) -> Self {
        Cache { files: Mutex::default(), dirs: Mutex::default(), session: AtomicU64::new(1), volatile, case_insensitive, stats: Mutex::default() }
    }

    /// Starts a new session: files and directories are checked again.
    pub fn new_session(&self) {
        self.session.fetch_add(1, Ordering::Relaxed);
    }

    fn is_volatile(&self, p: &Path) -> bool {
        self.volatile.iter().any(|v| p.starts_with(v))
    }

    fn name_key(&self, name: &str) -> String {
        if self.case_insensitive { name.to_lowercase() } else { name.to_string() }
    }

    /// The regular files (and links to them) of a directory.
    fn listing(&self, dir: &Path) -> Option<Arc<HashSet<String>>> {
        let session = self.session.load(Ordering::Relaxed);
        if let Some((s, l)) = self.dirs.lock().unwrap().get(dir)
            && *s == session
        {
            return l.clone();
        }
        let l = std::fs::read_dir(dir).ok().map(|rd| {
            Arc::new(
                rd.flatten()
                    .filter(|e| match e.file_type() {
                        Ok(t) if t.is_symlink() => e.path().is_file(),
                        Ok(t) => t.is_file(),
                        Err(_) => false,
                    })
                    .map(|e| self.name_key(&e.file_name().to_string_lossy()))
                    .collect(),
            )
        });
        self.dirs.lock().unwrap().insert(dir.to_path_buf(), (session, l.clone()));
        l
    }

    pub fn exists(&self, p: &Path) -> bool {
        if self.is_volatile(p) {
            return p.is_file();
        }
        let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else { return false };
        self.listing(dir).is_some_and(|l| l.contains(&self.name_key(&name.to_string_lossy())))
    }

    /// A file's directives, lexed once per version.
    pub fn directives(&self, p: &Path) -> Option<Arc<FileDirectives>> {
        let session = self.session.load(Ordering::Relaxed);
        let volatile = self.is_volatile(p);
        {
            let mut files = self.files.lock().unwrap();
            if let Some(e) = files.get_mut(p)
                && e.session == session
                && !volatile
            {
                self.stats.lock().unwrap().files_reused += 1;
                return Some(e.fd.clone());
            }
        }
        let meta = std::fs::metadata(p).ok().filter(|m| m.is_file())?;
        let (len, mtime) = (meta.len(), meta.modified().ok());
        if let Some(e) = self.files.lock().unwrap().get_mut(p)
            && e.len == len
            && e.mtime == mtime
        {
            e.session = session;
            self.stats.lock().unwrap().files_reused += 1;
            return Some(e.fd.clone());
        }
        let content = std::fs::read(p).ok()?;
        let fd = Arc::new(lex::lex(&content));
        self.stats.lock().unwrap().files_lexed += 1;
        self.files.lock().unwrap().insert(p.to_path_buf(), Entry { len, mtime, session, fd: fd.clone() });
        Some(fd)
    }
}

/// `p` without `.` components and with `..` applied lexically (as
/// compilers spell the paths they report).
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

struct Frame {
    path: PathBuf,
    dir: PathBuf,
    /// Where the file was found in the search chain (for `#include_next`).
    found_at: Option<usize>,
    line: u32,
}

struct Cond {
    parent_active: bool,
    taking: bool,
    /// A branch was certainly taken: the others are skipped.
    decided: bool,
}

struct State<'a> {
    cfg: &'a Config,
    cache: &'a Cache,
    oracle: &'a dyn Oracle,
    macros: MacroTable,
    pushed: HashMap<Sym, Vec<Option<Arc<Macro>>>>,
    once: HashSet<String>,
    seen: HashSet<String>,
    stack: Vec<Frame>,
    counter: i64,
    main: PathBuf,
    out: Output,
}

fn number(v: i64) -> Token {
    Token { kind: Kind::Number, sym: intern::intern(&v.to_string()), space: true }
}

fn string(s: &str) -> Token {
    let escaped: String = s.chars().flat_map(|c| if c == '\\' || c == '"' { vec!['\\', c] } else { vec![c] }).collect();
    Token { kind: Kind::Str, sym: intern::intern(&format!("\"{escaped}\"")), space: true }
}

/// The dynamic macros, defined although not in the macro table.
const DYNAMIC: &[&str] =
    &["__LINE__", "__FILE__", "__COUNTER__", "__INCLUDE_LEVEL__", "__BASE_FILE__", "__FILE_NAME__", "__DATE__", "__TIME__", "__TIMESTAMP__"];

impl Context for State<'_> {
    fn lookup(&self, name: Sym) -> Option<Arc<Macro>> {
        self.macros.get(&name).cloned()
    }

    fn is_defined(&self, name: Sym) -> bool {
        self.macros.contains_key(&name)
            || name == intern::HAS_INCLUDE
            || name == intern::HAS_INCLUDE_NEXT
            || self.cfg.operators.contains(&name)
            || DYNAMIC.contains(&name.as_str())
    }

    fn dynamic(&mut self, name: Sym) -> Option<Token> {
        let frame = self.stack.last()?;
        Some(match name {
            intern::LINE => number(frame.line as i64),
            intern::FILE => string(&frame.path.to_string_lossy()),
            intern::COUNTER => {
                self.counter += 1;
                number(self.counter - 1)
            }
            intern::INCLUDE_LEVEL => number(self.stack.len() as i64 - 1),
            intern::BASE_FILE => string(&self.main.to_string_lossy()),
            intern::FILE_NAME => string(&frame.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            _ if DYNAMIC.contains(&name.as_str()) => string("??"),
            _ => return None,
        })
    }

    fn has_include(&mut self, name: &str, angled: bool, next: bool) -> bool {
        match self.resolve(name, angled, next) {
            Some((p, _)) => {
                if !self.out.found.contains(&p) {
                    self.out.found.push(p);
                }
                true
            }
            None => false,
        }
    }

    fn is_query(&self, op: Sym) -> bool {
        op != intern::HAS_INCLUDE && op != intern::HAS_INCLUDE_NEXT && self.cfg.operators.contains(&op)
    }

    fn query(&mut self, op: Sym, arg: &str) -> Option<i64> {
        let v = self.oracle.query(op.as_str(), arg);
        if v.is_none() {
            let q = (op.as_str().to_string(), arg.to_string());
            if !self.out.unknown.contains(&q) {
                self.out.unknown.push(q);
            }
        }
        v
    }
}

impl State<'_> {
    fn key(&self, p: &Path) -> String {
        let s = p.to_string_lossy();
        if self.cache.case_insensitive { s.to_lowercase() } else { s.into_owned() }
    }

    fn inexact(&mut self, why: String) {
        if self.out.inexact.len() < 20 && !self.out.inexact.contains(&why) {
            self.out.inexact.push(why);
        }
    }

    fn here(&self) -> String {
        self.stack.last().map(|f| format!("{}:{}", f.path.display(), f.line)).unwrap_or_default()
    }

    /// Finds an included file: (path, index in the search chain).
    fn resolve(&self, name: &str, angled: bool, next: bool) -> Option<(PathBuf, Option<usize>)> {
        let p = Path::new(name);
        if p.is_absolute() {
            let n = normalize(p);
            return self.cache.exists(&n).then_some((n, None));
        }
        if !angled && !next {
            let dirs: Vec<&PathBuf> =
                if self.cfg.msvc_lookup { self.stack.iter().rev().map(|f| &f.dir).collect() } else { self.stack.last().map(|f| &f.dir).into_iter().collect() };
            for d in dirs {
                let c = normalize(&d.join(p));
                if self.cache.exists(&c) {
                    return Some((c, None));
                }
            }
        }
        let quotes = self.cfg.quote_dirs.len();
        let mut start = if angled { quotes } else { 0 };
        if next && let Some(Some(i)) = self.stack.last().map(|f| f.found_at) {
            start = start.max(i + 1);
        }
        let chain = self.cfg.quote_dirs.iter().chain(&self.cfg.angled_dirs).enumerate().skip(start);
        for (i, d) in chain {
            let c = normalize(&d.join(p));
            if self.cache.exists(&c) {
                return Some((c, Some(i)));
            }
        }
        None
    }

    fn record(&mut self, p: &Path) {
        if self.seen.insert(self.key(p)) {
            self.out.files.push(p.to_path_buf());
        }
    }

    fn include_file(&mut self, path: PathBuf, found_at: Option<usize>, import: bool) {
        let key = self.key(&path);
        if self.once.contains(&key) {
            return;
        }
        let Some(fd) = self.cache.directives(&path) else {
            self.out.missing.push(path.to_string_lossy().into_owned());
            return;
        };
        self.record(&path);
        if let Some(g) = fd.guard
            && self.macros.contains_key(&g)
        {
            return;
        }
        if import {
            self.once.insert(key);
        }
        if self.stack.len() >= 200 {
            self.inexact(format!("{}: #include nested too deeply", self.here()));
            return;
        }
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        self.stack.push(Frame { path, dir, found_at, line: 0 });
        self.run(&fd);
        self.stack.pop();
    }

    /// `#if`-family conditions: true, false, or unknown.
    fn condition(&mut self, d: &Directive) -> Option<bool> {
        match d.kind {
            DirKind::Ifdef | DirKind::Ifndef | DirKind::Elifdef | DirKind::Elifndef => {
                let Some(name) = d.toks.first().and_then(Token::ident) else {
                    self.inexact(format!("{}: malformed #{:?}", self.here(), d.kind));
                    return None;
                };
                let defined = self.is_defined(name);
                Some(if matches!(d.kind, DirKind::Ifdef | DirKind::Elifdef) { defined } else { !defined })
            }
            _ => {
                let dialect = self.cfg.dialect;
                let mut x = Expander::new(self, true);
                let toks = x.expand_tokens(&d.toks);
                let error = x.error.take();
                let v = expr::eval(&toks, dialect);
                if let Some(e) = error {
                    self.inexact(format!("{}: {e}", self.here()));
                    return None;
                }
                match v {
                    Ok(v) => v.truth(),
                    Err(e) => {
                        self.inexact(format!("{}: {e}", self.here()));
                        None
                    }
                }
            }
        }
    }

    fn include(&mut self, d: &Directive) {
        let next = d.kind == DirKind::IncludeNext;
        let target = match &d.toks[..] {
            [t] if t.kind == Kind::HeaderName => Some((t.sym.as_str().to_string(), true)),
            [t] if t.kind == Kind::Str => t.sym.as_str().strip_prefix('"').and_then(|s| s.strip_suffix('"')).map(|s| (s.to_string(), false)),
            toks => {
                let mut x = Expander::new(self, false);
                let expanded = x.expand(toks.iter().copied().map(XTok::new).collect());
                header_name(&expanded.iter().map(|t| t.t).collect::<Vec<_>>())
            }
        };
        let Some((name, angled)) = target else {
            self.inexact(format!("{}: malformed #include", self.here()));
            return;
        };
        match self.resolve(&name, angled, next) {
            Some((path, found_at)) => self.include_file(path, found_at, d.kind == DirKind::Import),
            None => self.out.missing.push(name),
        }
    }

    fn pragma(&mut self, d: &Directive) {
        let Some(first) = d.toks.first() else { return };
        if first.sym == intern::ONCE {
            let key = self.stack.last().map(|f| self.key(&f.path));
            if let Some(k) = key {
                self.once.insert(k);
            }
            return;
        }
        if first.sym != intern::PUSH_MACRO && first.sym != intern::POP_MACRO {
            return;
        }
        let name = match &d.toks[1..] {
            [l, s, r] if l.is(P::LParen) && s.kind == Kind::Str && r.is(P::RParen) => s.sym.as_str().trim_matches('"').to_string(),
            _ => return,
        };
        let name = intern::intern(&name);
        if first.sym == intern::PUSH_MACRO {
            let cur = self.macros.get(&name).cloned();
            self.pushed.entry(name).or_default().push(cur);
        } else if let Some(prev) = self.pushed.get_mut(&name).and_then(Vec::pop) {
            match prev {
                Some(m) => {
                    self.macros.insert(name, m);
                }
                None => {
                    self.macros.remove(&name);
                }
            }
        }
    }

    fn run(&mut self, fd: &FileDirectives) {
        let mut conds: Vec<Cond> = Vec::new();
        for d in &fd.dirs {
            if let Some(f) = self.stack.last_mut() {
                f.line = d.line;
            }
            let active = conds.last().is_none_or(|c| c.parent_active && c.taking);
            match d.kind {
                DirKind::If | DirKind::Ifdef | DirKind::Ifndef => {
                    if !active {
                        conds.push(Cond { parent_active: false, taking: false, decided: true });
                        continue;
                    }
                    let v = self.condition(d);
                    conds.push(Cond { parent_active: true, taking: v != Some(false), decided: v == Some(true) });
                }
                DirKind::Elif | DirKind::Elifdef | DirKind::Elifndef => {
                    let Some(c) = conds.last() else { continue };
                    if !c.parent_active {
                        continue;
                    }
                    if c.decided {
                        conds.last_mut().unwrap().taking = false;
                        continue;
                    }
                    let v = self.condition(d);
                    let c = conds.last_mut().unwrap();
                    c.taking = v != Some(false);
                    c.decided = v == Some(true);
                }
                DirKind::Else => {
                    if let Some(c) = conds.last_mut()
                        && c.parent_active
                    {
                        c.taking = !c.decided;
                        c.decided = true;
                    }
                }
                DirKind::Endif => {
                    conds.pop();
                }
                _ if !active => {}
                DirKind::Define => {
                    if let Some(m) = &d.mac {
                        self.macros.insert(m.name, m.clone());
                    }
                }
                DirKind::Undef => {
                    if let Some(n) = d.toks.first().and_then(Token::ident) {
                        self.macros.remove(&n);
                    }
                }
                DirKind::Include | DirKind::IncludeNext | DirKind::Import => self.include(d),
                DirKind::Pragma => self.pragma(d),
                DirKind::Other => {}
            }
        }
    }
}

/// `"x"` or `<x>` from the tokens of a computed `#include`.
fn header_name(toks: &[Token]) -> Option<(String, bool)> {
    match toks {
        [t] if t.kind == Kind::Str => Some((t.sym.as_str().strip_prefix('"')?.strip_suffix('"')?.to_string(), false)),
        [t, ..] if t.is(P::Lt) => {
            let end = toks.iter().position(|t| t.is(P::Gt))?;
            let mut s = String::new();
            for (i, t) in toks[1..end].iter().enumerate() {
                if i > 0 && t.space {
                    s.push(' ');
                }
                s.push_str(t.sym.as_str());
            }
            Some((s, true))
        }
        _ => None,
    }
}

/// `-D`/`-U` as `#define`/`#undef` lines.
fn command_text(commands: &[CommandMacro]) -> String {
    let mut text = String::new();
    for c in commands {
        match c {
            CommandMacro::Define(d) => match d.split_once('=') {
                Some((name, value)) => text.push_str(&format!("#define {name} {value}\n")),
                None => text.push_str(&format!("#define {d} 1\n")),
            },
            CommandMacro::Undef(u) => text.push_str(&format!("#undef {u}\n")),
        }
    }
    text
}

/// Scans a translation unit: the files its preprocessor reads.
pub fn scan(cache: &Cache, cfg: &Config, oracle: &dyn Oracle, main: &Path) -> Output {
    let main = normalize(main);
    let mut st = State {
        cfg,
        cache,
        oracle,
        macros: (*cfg.predefined).clone(),
        pushed: HashMap::new(),
        once: HashSet::new(),
        seen: HashSet::new(),
        stack: Vec::new(),
        counter: 0,
        main: main.clone(),
        out: Output::default(),
    };
    if !cfg.commands.is_empty() {
        let fd = lex::lex(command_text(&cfg.commands).as_bytes());
        st.stack.push(Frame { path: PathBuf::from("<command line>"), dir: PathBuf::new(), found_at: None, line: 0 });
        st.run(&fd);
        st.stack.pop();
    }
    for f in &cfg.forced {
        st.include_file(normalize(f), None, false);
    }
    if cache.directives(&main).is_none() {
        st.out.missing.push(main.to_string_lossy().into_owned());
        return st.out;
    }
    st.include_file(main, None, false);
    // The main file first.
    st.out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("cpp_deps_{name}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
        fn file(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p
        }
        fn rel(&self, files: &[PathBuf]) -> Vec<String> {
            files.iter().map(|p| p.strip_prefix(&self.0).unwrap().to_string_lossy().replace('\\', "/")).collect()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Answers(HashMap<(String, String), i64>);
    impl Oracle for Answers {
        fn query(&self, op: &str, arg: &str) -> Option<i64> {
            self.0.get(&(op.to_string(), arg.to_string())).copied()
        }
    }

    fn cfg(d: &Dir) -> Config {
        Config {
            dialect: expr::Dialect::CXX,
            angled_dirs: vec![d.0.join("inc"), d.0.join("sys")],
            predefined: Arc::new(macro_table("#define __GNUC__ 4\n#define __x86_64__ 1\n")),
            operators: Arc::new(["__has_builtin", "__has_feature"].iter().map(|s| intern::intern(s)).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn conditionals_and_macros() {
        let d = Dir::new("cond");
        let main = d.file(
            "main.cc",
            "#include \"local.h\"\n#if defined(__x86_64__) && __GNUC__ >= 4\n#include <x86.h>\n#else\n#include <other.h>\n#endif\n\
             #define HDR <computed.h>\n#include HDR\n#define STR(x) #x\n#define XSTR(x) STR(x)\n#include XSTR(quoted.h)\n\
             #ifdef NOPE\n#include <nope.h>\n#elif LEVEL > 2 and not defined(NOPE)\n#include <level.h>\n#endif\n#include <missing.h>\n",
        );
        d.file("local.h", "#define LEVEL 3\n");
        for f in ["inc/x86.h", "inc/other.h", "inc/computed.h", "quoted.h", "inc/nope.h", "sys/level.h"] {
            d.file(f, "");
        }
        let cache = Cache::new(vec![], false);
        let out = scan(&cache, &cfg(&d), &NoOracle, &main);
        assert_eq!(d.rel(&out.files), vec!["main.cc", "local.h", "inc/x86.h", "inc/computed.h", "quoted.h", "sys/level.h"]);
        assert_eq!(out.missing, vec!["missing.h"]);
        assert!(out.exact(), "{:?}", out.inexact);
    }

    #[test]
    fn guards_once_and_next() {
        let d = Dir::new("guards");
        let main = d.file("main.c", "#include <a.h>\n#include <a.h>\n#include <b.h>\n#include <b.h>\n#include <wrap.h>\n#include \"c.h\"\n#include <c.h>\n");
        d.file("inc/a.h", "#ifndef A_H\n#define A_H\n#include <counted.h>\n#endif\n");
        d.file("inc/b.h", "#pragma once\n#include <counted2.h>\n");
        d.file("inc/counted.h", "#ifdef A_H\n#define SEEN_TWICE\n#endif\n");
        d.file("inc/counted2.h", "");
        // include_next reaches the next directory's wrap.h.
        d.file("inc/wrap.h", "#include_next <wrap.h>\n");
        d.file("sys/wrap.h", "");
        // c.h: "c.h" from the main file's directory, <c.h> from inc.
        d.file("c.h", "");
        d.file("inc/c.h", "");
        let cache = Cache::new(vec![], false);
        let out = scan(&cache, &cfg(&d), &NoOracle, &main);
        assert_eq!(d.rel(&out.files), vec!["main.c", "inc/a.h", "inc/counted.h", "inc/b.h", "inc/counted2.h", "inc/wrap.h", "sys/wrap.h", "c.h", "inc/c.h"]);
        assert!(out.exact());
        // The second scan reuses the lexed files.
        let before = cache.stats.lock().unwrap().files_lexed;
        scan(&cache, &cfg(&d), &NoOracle, &main);
        assert_eq!(cache.stats.lock().unwrap().files_lexed, before);
    }

    #[test]
    fn queries_and_has_include() {
        let d = Dir::new("queries");
        let main = d.file(
            "main.cc",
            "#if __has_builtin(__builtin_expect)\n#include <expect.h>\n#endif\n\
             #if __has_feature(unknown_feature)\n#include <maybe.h>\n#else\n#include <maybe_not.h>\n#endif\n\
             #if __has_include(<probe.h>) && !__has_include(<absent.h>)\n#include <has.h>\n#endif\n\
             #ifdef __has_builtin\n#include <op.h>\n#endif\n#ifdef __has_warning\n#include <no_op.h>\n#endif\n",
        );
        for f in ["inc/expect.h", "inc/maybe.h", "inc/maybe_not.h", "inc/probe.h", "inc/has.h", "inc/op.h", "inc/no_op.h"] {
            d.file(f, "");
        }
        let cache = Cache::new(vec![], false);
        let oracle = Answers([(("__has_builtin".to_string(), "__builtin_expect".to_string()), 1)].into_iter().collect());
        let out = scan(&cache, &cfg(&d), &oracle, &main);
        // The unknown feature takes both branches.
        assert_eq!(d.rel(&out.files), vec!["main.cc", "inc/expect.h", "inc/maybe.h", "inc/maybe_not.h", "inc/has.h", "inc/op.h"]);
        assert_eq!(d.rel(&out.found), vec!["inc/probe.h"]);
        assert_eq!(out.unknown, vec![("__has_feature".to_string(), "unknown_feature".to_string())]);
        assert!(!out.exact());
    }

    #[test]
    fn command_line_forced_and_push_pop() {
        let d = Dir::new("cmdline");
        let main = d.file(
            "main.c",
            "#if FOO == 2 && !defined(BAR) && defined(FORCED)\n#include <yes.h>\n#endif\n\
             #define M 1\n#pragma push_macro(\"M\")\n#undef M\n#define M 2\n#pragma pop_macro(\"M\")\n#if M == 1\n#include <popped.h>\n#endif\n",
        );
        let forced = d.file("forced.h", "#define FORCED\n");
        d.file("inc/yes.h", "");
        d.file("inc/popped.h", "");
        let mut c = cfg(&d);
        c.commands = vec![CommandMacro::Define("FOO=2".into()), CommandMacro::Define("BAR".into()), CommandMacro::Undef("BAR".into())];
        c.forced = vec![forced];
        let cache = Cache::new(vec![], false);
        let out = scan(&cache, &c, &NoOracle, &main);
        assert_eq!(d.rel(&out.files), vec!["forced.h", "main.c", "inc/yes.h", "inc/popped.h"]);
        assert!(out.exact(), "{:?}", out.inexact);
    }

    #[test]
    fn msvc_lookup() {
        let d = Dir::new("msvc");
        let main = d.file("src/main.cc", "#include <sub/a.h>\n");
        d.file("inc/sub/a.h", "#include \"b.h\"\n");
        // MSVC finds b.h next to main.cc (an includer), not next to a.h.
        d.file("src/b.h", "");
        let mut c = cfg(&d);
        let cache = Cache::new(vec![], false);
        assert_eq!(scan(&cache, &c, &NoOracle, &main).missing, vec!["b.h"]);
        c.msvc_lookup = true;
        let out = scan(&cache, &c, &NoOracle, &main);
        assert_eq!(d.rel(&out.files), vec!["src/main.cc", "inc/sub/a.h", "src/b.h"]);
    }
}
