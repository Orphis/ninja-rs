//! Scanning a translation unit: the preprocessor's directives evaluated
//! exactly (macros, conditionals, includes), text skipped.
//!
//! A conditional the scanner can't decide (an unanswered compiler query, a
//! malformed expression) processes every branch it can't rule out, and the
//! result is marked inexact: it's then likely a superset, but no guarantee.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::SystemTime;

use crate::expr;
use crate::hmap::HeaderMap;
use crate::intern::{self, Sym};
use crate::lex::{self, DirKind, Directive, FileDirectives, Kind, P, Token};
use crate::macros::{self, Context, Expander, Macro, Pragmas, XTok};
use crate::vfs::{Overlay, Redirect};

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

/// An entry of the include search chain.
#[derive(Clone, Debug)]
pub enum SearchDir {
    Dir(PathBuf),
    /// Apple frameworks (`-F`): `<Foo/Bar.h>` is
    /// `Foo.framework/Headers/Bar.h` (or `PrivateHeaders`).
    Framework(PathBuf),
    /// A Clang header map (`-I x.hmap`).
    HeaderMap(Arc<HeaderMap>),
}

impl From<PathBuf> for SearchDir {
    fn from(p: PathBuf) -> Self {
        SearchDir::Dir(p)
    }
}

/// The compiler, for what its preprocessors do differently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Family {
    /// `#include_next` from a file found next to its includer starts at
    /// the head of the `#include "..."` chain.
    #[default]
    Gcc,
    /// A file found next to an includer has the includer's place in the
    /// chains (`#include_next` goes on after it); one found otherwise
    /// outside them searches as `#include` does.
    Clang,
    /// cl: a lone carriage return doesn't end a line; `#import` reads a
    /// type library.
    Msvc,
}

/// What a compile's preprocessor is set up with.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub family: Family,
    /// `true`/`false` and the alternative operators (`and`, `or`, ...).
    pub dialect: expr::Dialect,
    /// Trigraphs are replaced (which the lexer doesn't do: files with any
    /// make the result inexact).
    pub trigraphs: bool,
    /// `-iquote`: searched by `#include "..."` only.
    pub quote_dirs: Vec<SearchDir>,
    /// `-I`/`-F`, `-isystem`/`-iframework`, the built-in directories,
    /// `-idirafter`, in order.
    pub angled_dirs: Vec<SearchDir>,
    /// `--embed-dir`: searched by `#embed` (after the current file's
    /// directory for `#embed "..."`).
    pub embed_dirs: Vec<PathBuf>,
    /// VFS overlays (`-ivfsoverlay`), in the order given: later ones are
    /// consulted first.
    pub overlays: Vec<Arc<Overlay>>,
    /// MSVC (cl, and Clang's MSVC compatibility): `#include "..."` searches
    /// the directories of every includer, innermost first, not only the
    /// current file's; forced includes count the main file as one.
    pub msvc_lookup: bool,
    /// The compiler's predefined macros.
    pub predefined: Arc<MacroTable>,
    pub commands: Vec<CommandMacro>,
    /// `-imacros`, `-include` and `/FI` files, in the order they're
    /// processed (every `-imacros` first).
    pub forced: Vec<Forced>,
    /// The `__has_*` operators the compiler has, besides `__has_include`.
    pub operators: Arc<HashSet<Sym>>,
    /// The command line names the source, a search directory or a forced
    /// include by a relative path: `__FILE__` is then spelled relative,
    /// unlike the scan's paths.
    pub relative_paths: bool,
}

/// A file processed before the main one, and how the compiler finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Forced {
    /// A path (absolute, or one the compiler itself reported).
    Path(PathBuf),
    /// GCC and Clang (`-include`, `-imacros`): in the working directory,
    /// then along the `#include "..."` chain.
    Gcc { cwd: PathBuf, name: String },
    /// `cl.exe` (`/FI`): as `#include "name"` in the main file, its
    /// directory first.
    Msvc(String),
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

/// What tells a file's versions apart: its size and modification time,
/// and its change time and inode on Unix (a file replaced by one of the
/// same size with its modification time restored, as `cp -p` or an
/// archive extraction leaves it), its creation time on Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Stamp {
    len: u64,
    mtime: Option<SystemTime>,
    change: u64,
    inode: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Stamp {
        #[cfg(unix)]
        let (change, inode) = {
            use std::os::unix::fs::MetadataExt;
            ((meta.ctime() as u64).wrapping_mul(1_000_000_000).wrapping_add(meta.ctime_nsec() as u64), meta.ino())
        };
        #[cfg(windows)]
        let (change, inode) = {
            use std::os::windows::fs::MetadataExt;
            (meta.creation_time(), 0)
        };
        #[cfg(not(any(unix, windows)))]
        let (change, inode) = (0, 0);
        Stamp { len: meta.len(), mtime: meta.modified().ok(), change, inode }
    }
}

/// A file's key and stamp.
type Version = (PathBuf, Stamp);
type Lexing = Arc<OnceLock<Option<Arc<FileDirectives>>>>;

/// The session of entries a watch vouches for: valid until it reports a
/// change (`Cache::changed`).
const WATCHED: u64 = u64::MAX;

struct Entry {
    stamp: Stamp,
    /// The session it was last checked in, or `WATCHED`.
    session: AtomicU64,
    fd: Arc<FileDirectives>,
}

/// Tells which files need no checking: a file watcher. What the cache
/// checks under a watch stays valid, across sessions, until the watcher
/// reports a change with `Cache::changed`.
pub trait Watch: Send + Sync {
    /// Taken before checking `path` (a file, or a directory's names): a
    /// token when the watch covers it.
    fn token(&self, path: &Path) -> Option<u64>;
    /// Whether no change was seen since `token` was taken (a change seen
    /// meanwhile may have been missed by the check).
    fn unchanged_since(&self, token: u64) -> bool;
    /// Whether `path` may change during a session (in a build directory
    /// the watch knows): checked every time.
    fn volatile(&self, _path: &Path) -> bool {
        false
    }
}

/// What a watcher saw change (see `Cache::changed`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// A file's contents, or a directory's names.
    Content,
    /// A directory's names (files created, removed or renamed).
    Names,
    /// Everything under a directory (it was replaced or removed, or the
    /// watcher lost track).
    Tree,
}

/// Lexed files and directory listings, shared by every scan (and thread).
/// Within a session, files and directories outside the volatile ones are
/// assumed unchanged; a new session checks them again, except for those a
/// watch vouches for (`set_watch`). Lookups only take read locks, so that
/// concurrent scans don't wait for each other.
pub struct Cache {
    files: RwLock<HashMap<PathBuf, Arc<Entry>>>,
    /// Files being lexed (key, size, modification time): concurrent scans
    /// wait for the one lexing.
    lexing: Mutex<HashMap<Version, Lexing>>,
    dirs: RwLock<HashMap<PathBuf, Listing>>,
    session: AtomicU64,
    /// As keys (see `key`), with when a build last named each: looked up
    /// by ancestor, however many there are (CMake's `try_compile` names
    /// hundreds a day).
    volatile: RwLock<HashMap<PathBuf, std::time::Instant>>,
    case_insensitive: bool,
    watch: OnceLock<Arc<dyn Watch>>,
    files_lexed: AtomicU64,
    files_reused: AtomicU64,
}

impl Cache {
    /// `volatile`: directories whose contents may change during a session
    /// (the build directory). `case_insensitive`: the file system's names.
    pub fn new(volatile: Vec<PathBuf>, case_insensitive: bool) -> Self {
        let cache = Cache {
            files: RwLock::default(),
            lexing: Mutex::default(),
            dirs: RwLock::default(),
            session: AtomicU64::new(1),
            volatile: RwLock::default(),
            case_insensitive,
            watch: OnceLock::new(),
            files_lexed: AtomicU64::new(0),
            files_reused: AtomicU64::new(0),
        };
        for v in volatile {
            cache.add_volatile(&v);
        }
        cache
    }

    /// Another directory whose contents may change during a session, or
    /// one named again.
    pub fn add_volatile(&self, dir: &Path) {
        let key = self.key(dir).into_owned();
        self.volatile.write().unwrap().insert(key, std::time::Instant::now());
    }

    /// Forgets the volatile directories not named since `before`.
    pub fn expire_volatile(&self, before: std::time::Instant) {
        self.volatile.write().unwrap().retain(|_, named| *named >= before);
    }

    /// Lets `watch` vouch for files (once).
    pub fn set_watch(&self, watch: Arc<dyn Watch>) {
        let _ = self.watch.set(watch);
    }

    /// What a watcher saw change: entries it concerns are checked again.
    pub fn changed(&self, path: &Path, change: Change) {
        let key = self.key(path);
        match change {
            Change::Content => {
                self.files.write().unwrap().remove(key.as_ref());
                self.dirs.write().unwrap().remove(key.as_ref());
            }
            Change::Names => {
                self.dirs.write().unwrap().remove(key.as_ref());
            }
            Change::Tree => {
                self.files.write().unwrap().retain(|k, _| !k.starts_with(&key));
                self.dirs.write().unwrap().retain(|k, _| !k.starts_with(&key));
            }
        }
    }

    /// The number of lexed files and directory listings kept.
    pub fn len(&self) -> (usize, usize) {
        (self.files.read().unwrap().len(), self.dirs.read().unwrap().len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == (0, 0)
    }

    /// `p` as a key of the maps: lowercase on case-insensitive file systems.
    fn key<'a>(&self, p: &'a Path) -> Cow<'a, Path> {
        if self.case_insensitive { Cow::Owned(PathBuf::from(p.to_string_lossy().to_lowercase())) } else { Cow::Borrowed(p) }
    }

    /// Whether an entry checked in `session` is still valid.
    fn current(&self, session: u64) -> bool {
        session == WATCHED || session == self.session.load(Ordering::Relaxed)
    }

    /// The session to record for a check that started with `token`.
    fn checked(&self, token: Option<u64>) -> u64 {
        match (token, self.watch.get()) {
            (Some(t), Some(w)) if w.unchanged_since(t) => WATCHED,
            _ => self.session.load(Ordering::Relaxed),
        }
    }

    fn token(&self, p: &Path) -> Option<u64> {
        self.watch.get().and_then(|w| w.token(p))
    }

    pub fn stats(&self) -> Stats {
        Stats { files_lexed: self.files_lexed.load(Ordering::Relaxed), files_reused: self.files_reused.load(Ordering::Relaxed) }
    }

    /// Starts a new session: files and directories are checked again.
    pub fn new_session(&self) {
        self.session.fetch_add(1, Ordering::Relaxed);
    }

    /// The current session.
    pub fn session(&self) -> u64 {
        self.session.load(Ordering::Relaxed)
    }

    fn is_volatile(&self, p: &Path) -> bool {
        if self.watch.get().is_some_and(|w| w.volatile(p)) {
            return true;
        }
        let key = self.key(p);
        let volatile = self.volatile.read().unwrap();
        !volatile.is_empty() && key.ancestors().any(|a| volatile.contains_key(a))
    }

    fn name_key(&self, name: &str) -> String {
        if self.case_insensitive { name.to_lowercase() } else { name.to_string() }
    }

    /// The regular files (and links to them) of a directory.
    fn listing(&self, dir: &Path) -> Option<Arc<HashSet<String>>> {
        let key = self.key(dir);
        if let Some((s, l)) = self.dirs.read().unwrap().get(key.as_ref())
            && self.current(*s)
        {
            return l.clone();
        }
        let token = self.token(dir);
        let mut linked = false;
        let l = std::fs::read_dir(dir).ok().map(|rd| {
            Arc::new(
                rd.flatten()
                    .filter(|e| match e.file_type() {
                        Ok(t) if t.is_symlink() => {
                            linked = true;
                            e.path().is_file()
                        }
                        Ok(t) => t.is_file(),
                        Err(_) => false,
                    })
                    .map(|e| self.name_key(&e.file_name().to_string_lossy()))
                    .collect(),
            )
        });
        let mut dirs = self.dirs.write().unwrap();
        // Where a link leads changes without its directory: a listing with
        // one holds for the session.
        let session = if linked { self.session() } else { self.checked(token) };
        dirs.insert(key.into_owned(), (session, l.clone()));
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
        let key = self.key(p);
        let volatile = self.is_volatile(p);
        let known = self.files.read().unwrap().get(key.as_ref()).cloned();
        if let Some(e) = &known
            && !volatile
            && self.current(e.session.load(Ordering::Relaxed))
        {
            self.files_reused.fetch_add(1, Ordering::Relaxed);
            return Some(e.fd.clone());
        }
        let token = if volatile { None } else { self.token(p) };
        let meta = std::fs::metadata(p).ok().filter(|m| m.is_file())?;
        let stamp = Stamp::of(&meta);
        if let Some(e) = known
            && e.stamp == stamp
        {
            e.session.store(self.checked(token), Ordering::Relaxed);
            self.files_reused.fetch_add(1, Ordering::Relaxed);
            return Some(e.fd.clone());
        }
        let version = (key.into_owned(), stamp);
        let cell = self.lexing.lock().unwrap().entry(version.clone()).or_default().clone();
        let mut lexed = false;
        let fd = cell
            .get_or_init(|| {
                lexed = true;
                let fd = Arc::new(lex::lex(&std::fs::read(p).ok()?));
                self.files_lexed.fetch_add(1, Ordering::Relaxed);
                // The token compared under the lock: an event counted after
                // it drops the entry once it's in.
                let mut files = self.files.write().unwrap();
                files.insert(version.0.clone(), Arc::new(Entry { stamp, session: AtomicU64::new(self.checked(token)), fd: fd.clone() }));
                Some(fd)
            })
            .clone();
        if lexed {
            self.lexing.lock().unwrap().remove(&version);
        } else {
            self.files_reused.fetch_add(1, Ordering::Relaxed);
        }
        fd
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
    /// The path the preprocessor knows (virtual under an overlay), and the
    /// file's real path.
    path: PathBuf,
    real: PathBuf,
    dir: PathBuf,
    /// Where the file was found in the search chain (for `#include_next`).
    found_at: Option<usize>,
    /// Found next to an includer (or in the working directory).
    relative: bool,
    line: u32,
    /// A `#line` changed `__LINE__` and `__FILE__`.
    line_set: bool,
}

/// A file found by a lookup: its path as the preprocessor knows it, its
/// real path, and where: in the search chain, or next to an includer.
struct Found {
    path: PathBuf,
    real: PathBuf,
    at: Option<usize>,
    relative: bool,
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
    /// The macros that macros defined so far push or pop, in text.
    pushed_in_text: HashSet<Sym>,
    once: HashSet<String>,
    seen: HashSet<String>,
    stack: Vec<Frame>,
    /// The main file's directory while forced includes are processed, an
    /// includer's for the MSVC lookup.
    base: Option<PathBuf>,
    main: PathBuf,
    out: Output,
}

fn number(v: i64) -> Token {
    macros::number(v)
}

fn string(s: &str) -> Token {
    let escaped: String = s.chars().flat_map(|c| if c == '\\' || c == '"' { vec!['\\', c] } else { vec![c] }).collect();
    Token { kind: Kind::Str, sym: intern::intern(&format!("\"{escaped}\"")), space: true }
}

/// The dynamic macros, defined although not in the macro table.
const DYNAMIC: &[Sym] =
    &[intern::LINE, intern::FILE, intern::COUNTER, intern::INCLUDE_LEVEL, intern::BASE_FILE, intern::FILE_NAME, intern::DATE, intern::TIME, intern::TIMESTAMP];

impl Context for State<'_> {
    fn lookup(&self, name: Sym) -> Option<Arc<Macro>> {
        self.macros.get(&name).cloned()
    }

    fn is_defined(&self, name: Sym) -> bool {
        self.macros.contains_key(&name)
            || name == intern::HAS_INCLUDE
            || name == intern::HAS_INCLUDE_NEXT
            || self.cfg.operators.contains(&name)
            || DYNAMIC.contains(&name)
    }

    fn dynamic(&mut self, name: Sym) -> Option<Token> {
        // `__COUNTER__` counts its uses in text too, which isn't expanded;
        // `#line` changes `__LINE__` and `__FILE__`; compilers spell
        // `__FILE__` as the command line spells its paths, which the scan
        // makes absolute.
        let line_set = self.stack.last()?.line_set;
        let unknown = if name == intern::COUNTER {
            Some("__COUNTER__")
        } else if line_set && [intern::LINE, intern::FILE, intern::FILE_NAME].contains(&name) {
            Some("__LINE__ or __FILE__ after #line")
        } else if self.cfg.relative_paths && [intern::FILE, intern::BASE_FILE].contains(&name) {
            Some("__FILE__ with relative paths on the command line")
        } else {
            None
        };
        if let Some(what) = unknown {
            self.inexact(format!("{}: {what} in a condition or #include", self.here()));
            return Some(Token { kind: Kind::Unknown, sym: intern::EMPTY, space: true });
        }
        let frame = self.stack.last()?;
        Some(match name {
            intern::LINE => number(frame.line as i64),
            intern::FILE => string(&frame.path.to_string_lossy()),
            intern::INCLUDE_LEVEL => number(self.stack.len() as i64 - 1),
            intern::BASE_FILE => string(&self.main.to_string_lossy()),
            intern::FILE_NAME => string(&frame.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            _ if DYNAMIC.contains(&name) => string("??"),
            _ => return None,
        })
    }

    fn has_include(&mut self, name: &str, angled: bool, next: bool) -> bool {
        match self.resolve(name, angled, next) {
            Some(f) => {
                if !self.out.found.contains(&f.real) {
                    self.out.found.push(f.real);
                }
                true
            }
            None => false,
        }
    }

    fn has_embed(&mut self, name: &str, angled: bool, limit_zero: bool) -> i64 {
        match self.resolve_embed(name, angled) {
            Some(real) => {
                let empty = limit_zero || std::fs::metadata(&real).map(|m| m.len() == 0).unwrap_or(false);
                if !self.out.found.contains(&real) {
                    self.out.found.push(real);
                }
                // __STDC_EMBED_FOUND__ or __STDC_EMBED_EMPTY__.
                if empty { 2 } else { 1 }
            }
            None => 0,
        }
    }

    fn is_query(&self, op: Sym) -> bool {
        op != intern::HAS_INCLUDE && op != intern::HAS_INCLUDE_NEXT && op != intern::HAS_EMBED && self.cfg.operators.contains(&op)
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

    fn redefined(&self, name: Sym) -> bool {
        match (self.macros.get(&name), self.cfg.predefined.get(&name)) {
            (None, None) => false,
            (Some(m), Some(p)) => !Arc::ptr_eq(m, p),
            _ => true,
        }
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

    /// The real file a path names: through the overlays, else on disk.
    fn locate(&self, p: &Path) -> Option<PathBuf> {
        for o in self.cfg.overlays.iter().rev() {
            if o.redirect == Redirect::Fallback && self.cache.exists(p) {
                return Some(p.to_path_buf());
            }
            if let Some(real) = o.map(p)
                && self.cache.exists(&real)
            {
                return Some(real);
            }
            if o.redirect == Redirect::RedirectOnly {
                return None;
            }
        }
        self.cache.exists(p).then(|| p.to_path_buf())
    }

    fn candidate(&self, path: PathBuf, at: Option<usize>) -> Option<Found> {
        let real = self.locate(&path)?;
        Some(Found { path, real, at, relative: false })
    }

    /// Finds an included file.
    fn resolve(&self, name: &str, angled: bool, next: bool) -> Option<Found> {
        let p = Path::new(name);
        if p.is_absolute() {
            return self.candidate(normalize(p), None);
        }
        // `#include_next` goes on after the directory the current file was
        // found in, `<...>` or not. From a file found otherwise, GCC starts
        // at the head of the chain when it was found next to its includer,
        // and searches as `#include` does when it's the main file or was
        // found by its absolute path; Clang searches as `#include` (a file
        // it found next to an includer has the includer's place).
        let after = match self.stack.last() {
            Some(f) if next => match f.found_at {
                Some(i) => Some(i + 1),
                None if f.relative && self.cfg.family == Family::Gcc => Some(0),
                None => None,
            },
            _ => None,
        };
        if !angled && after.is_none() {
            let dirs: Vec<&PathBuf> = if self.cfg.msvc_lookup {
                self.stack.iter().rev().map(|f| &f.dir).chain(&self.base).collect()
            } else {
                self.stack.last().map(|f| &f.dir).into_iter().collect()
            };
            // Clang's lookup leaves the includer's place as the found
            // file's, wherever along the includers it was found.
            let at = if self.cfg.family == Family::Clang { self.stack.last().and_then(|f| f.found_at) } else { None };
            for d in dirs {
                if let Some(f) = self.candidate(normalize(&d.join(p)), at) {
                    return Some(Found { relative: true, ..f });
                }
            }
        }
        let start = after.unwrap_or(if angled { self.cfg.quote_dirs.len() } else { 0 });
        let chain = self.cfg.quote_dirs.iter().chain(&self.cfg.angled_dirs).enumerate().skip(start);
        for (i, d) in chain {
            let found = match d {
                SearchDir::Dir(d) => self.candidate(normalize(&d.join(p)), Some(i)),
                SearchDir::Framework(d) => {
                    // Only `Framework/Header.h` names are in frameworks: the
                    // search goes on with the next directory.
                    let Some((framework, rest)) = name.split_once(['/', '\\']) else { continue };
                    ["Headers", "PrivateHeaders"]
                        .iter()
                        .find_map(|sub| self.candidate(normalize(&d.join(format!("{framework}.framework")).join(sub).join(rest)), Some(i)))
                }
                SearchDir::HeaderMap(m) => m.lookup(name).and_then(|f| self.candidate(f.clone(), Some(i))),
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// Finds an `#embed`ded file: `"..."` next to the current file, then
    /// in the embed directories.
    fn resolve_embed(&self, name: &str, angled: bool) -> Option<PathBuf> {
        let p = Path::new(name);
        if p.is_absolute() {
            return self.locate(&normalize(p));
        }
        let current = if angled { None } else { self.stack.last().map(|f| &f.dir) };
        current.into_iter().chain(&self.cfg.embed_dirs).find_map(|d| self.locate(&normalize(&d.join(p))))
    }

    fn record(&mut self, p: &Path) {
        if self.seen.insert(self.key(p)) {
            self.out.files.push(p.to_path_buf());
        }
    }

    fn include_file(&mut self, f: Found, import: bool) {
        let Found { path, real, at: found_at, relative } = f;
        let key = self.key(&real);
        if self.once.contains(&key) {
            return;
        }
        let Some(fd) = self.cache.directives(&real) else {
            self.out.missing.push(path.to_string_lossy().into_owned());
            return;
        };
        self.record(&real);
        if fd.utf16 {
            self.inexact(format!("{}: UTF-16 text isn't scanned", real.display()));
            return;
        }
        if fd.trigraphs && self.cfg.trigraphs {
            self.inexact(format!("{}: trigraphs aren't replaced", real.display()));
        }
        if fd.lone_cr && self.cfg.family == Family::Msvc {
            self.inexact(format!("{}: cl doesn't end lines at a lone carriage return", real.display()));
        }
        if fd.asm_include {
            self.inexact(format!("{}: an assembler's .include or .incbin", real.display()));
        }
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
        self.stack.push(Frame { path, real, dir, found_at, relative, line: 0, line_set: false });
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
        if d.kind == DirKind::Import && self.cfg.family == Family::Msvc {
            // cl's reads a type library, and includes the headers it
            // generates from it (and `comdef.h`).
            self.inexact(format!("{}: #import of a type library", self.here()));
            return;
        }
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
            Some(f) => self.include_file(f, d.kind == DirKind::Import),
            None => self.out.missing.push(name),
        }
    }

    /// `#embed`: the file is read (as data, not preprocessed).
    fn embed(&mut self, d: &Directive) {
        let target = match d.toks.first() {
            Some(t) if t.kind == Kind::HeaderName => Some((t.sym.as_str().to_string(), true)),
            Some(t) if t.kind == Kind::Str => t.sym.as_str().strip_prefix('"').and_then(|s| s.strip_suffix('"')).map(|s| (s.to_string(), false)),
            _ => None,
        };
        let Some((name, angled)) = target else {
            self.inexact(format!("{}: unsupported #embed", self.here()));
            return;
        };
        match self.resolve_embed(&name, angled) {
            Some(real) => self.record(&real),
            None => self.out.missing.push(name),
        }
    }

    fn pragma(&mut self, d: &Directive) {
        let Some(first) = d.toks.first() else { return };
        if first.sym == intern::PRAGMA_OPERATOR {
            self.inexact(format!("{}: a pragma from a macro in text", self.here()));
            return;
        }
        if first.sym == intern::INCLUDE_ALIAS {
            self.inexact(format!("{}: #pragma include_alias", self.here()));
            return;
        }
        if first.sym == intern::ONCE {
            let key = self.stack.last().map(|f| self.key(&f.real));
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
            self.changing(name);
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

    /// Before a macro's definition changes: a macro that text may push or
    /// pop (see `Pragmas::Macros`) then has an unknown definition.
    fn changing(&mut self, name: Sym) {
        if self.pushed_in_text.contains(&name) {
            self.inexact(format!("{}: {} changes while macros in text may push or pop it", self.here(), name.as_str()));
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
                        match &m.pragmas {
                            Pragmas::None => {}
                            // Pushing and popping macros never changed in
                            // between does nothing.
                            Pragmas::Macros(names) => self.pushed_in_text.extend(names.iter().copied()),
                            Pragmas::Other => self.inexact(format!("{}: {} may run a pragma in text", self.here(), m.name.as_str())),
                        }
                        self.changing(m.name);
                        self.macros.insert(m.name, m.clone());
                    }
                }
                DirKind::Undef => {
                    if let Some(n) = d.toks.first().and_then(Token::ident)
                        && self.macros.remove(&n).is_some()
                    {
                        self.changing(n);
                    }
                }
                DirKind::Include | DirKind::IncludeNext | DirKind::Import => self.include(d),
                DirKind::Embed => self.embed(d),
                DirKind::Pragma => self.pragma(d),
                DirKind::Line => {
                    if let Some(f) = self.stack.last_mut() {
                        f.line_set = true;
                    }
                }
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

/// `-D`/`-U` as `#define`/`#undef` lines. A value ends at a line's end,
/// and a backslash ending it is kept (as GCC and Clang do), not a splice
/// joining the next line.
fn command_text(commands: &[CommandMacro]) -> String {
    let mut text = String::new();
    for c in commands {
        match c {
            CommandMacro::Define(d) => match d.split_once('=') {
                Some((name, value)) => {
                    let value = value.split(['\n', '\r']).next().unwrap_or_default();
                    let splice = if value.trim_end().ends_with('\\') { "\\\n" } else { "" };
                    text.push_str(&format!("#define {name} {value}{splice}\n"));
                }
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
        pushed_in_text: HashSet::new(),
        once: HashSet::new(),
        seen: HashSet::new(),
        stack: Vec::new(),
        base: None,
        main: main.clone(),
        out: Output::default(),
    };
    if !cfg.commands.is_empty() {
        let fd = lex::lex(command_text(&cfg.commands).as_bytes());
        let frame = Frame {
            path: PathBuf::from("<command line>"),
            real: PathBuf::new(),
            dir: PathBuf::new(),
            found_at: None,
            relative: false,
            line: 0,
            line_set: false,
        };
        st.stack.push(frame);
        st.run(&fd);
        st.stack.pop();
    }
    // The MSVC lookup searches the main file's directory for forced
    // includes and what they include, as an includer's (cl's `/FI`
    // first, Clang's `-include` after the working directory).
    if cfg.msvc_lookup {
        st.base = main.parent().map(Path::to_path_buf);
    }
    for f in &cfg.forced {
        let at = |st: &State, path: PathBuf, relative| st.locate(&path).map(|real| Found { path, real, at: None, relative });
        let found = match f {
            Forced::Path(p) => at(&st, normalize(p), false),
            Forced::Gcc { cwd, name } => at(&st, normalize(&cwd.join(name)), true).or_else(|| st.resolve(name, false, false)),
            Forced::Msvc(name) => st.resolve(name, false, false),
        };
        match found {
            Some(found) => st.include_file(found, false),
            None => st.out.missing.push(match f {
                Forced::Path(p) => p.to_string_lossy().into_owned(),
                Forced::Gcc { name, .. } | Forced::Msvc(name) => name.clone(),
            }),
        }
    }
    st.base = None;
    let Some(real) = st.locate(&main).filter(|r| cache.directives(r).is_some()) else {
        st.out.missing.push(main.to_string_lossy().into_owned());
        return st.out;
    };
    st.include_file(Found { path: main, real, at: None, relative: false }, false);
    // The main file first.
    st.out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn many_volatile_dirs() {
        let cache = Cache::new(Vec::new(), false);
        for i in 0..5000 {
            cache.add_volatile(&PathBuf::from(format!("/b/try_compile/{i}")));
        }
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            assert!(!cache.is_volatile(Path::new("/src/include/a.h")));
        }
        assert!(start.elapsed() < std::time::Duration::from_millis(500), "{:?}", start.elapsed());
        assert!(cache.is_volatile(Path::new("/b/try_compile/4321/x.h")));
        assert!(!cache.is_volatile(Path::new("/b/try_compile/43210/x.h")));
        // Not named since: forgotten.
        cache.expire_volatile(std::time::Instant::now() + std::time::Duration::from_millis(1));
        assert!(!cache.is_volatile(Path::new("/b/try_compile/4321/x.h")));
    }

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let d = test_dirs::base().join(format!("cpp_deps_{name}_{}", std::process::id()));
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
            angled_dirs: vec![d.0.join("inc").into(), d.0.join("sys").into()],
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
        let before = cache.stats().files_lexed;
        scan(&cache, &cfg(&d), &NoOracle, &main);
        assert_eq!(cache.stats().files_lexed, before);
    }

    #[test]
    fn include_next_rules() {
        let d = Dir::new("next");
        let mut c = cfg(&d);
        c.quote_dirs = vec![d.0.join("q1").into(), d.0.join("q2").into()];
        let main = d.file("main.c", "#include \"x.h\"\n");
        d.file("q1/x.h", "#include_next <x.h>\n");
        d.file("q2/x.h", "");
        d.file("inc/x.h", "");
        let rel = d.file("rel/main.c", "#include \"y.h\"\n");
        d.file("rel/y.h", "#include_next <y.h>\n");
        d.file("q1/y.h", "");
        d.file("inc/y.h", "");
        let rel2 = d.file("rel/main2.c", "#include_next \"y2.h\"\n");
        d.file("rel/y2.h", "");
        d.file("q1/y2.h", "");
        let cache = Cache::new(vec![], false);
        let files = |c: &Config, main: &Path| d.rel(&scan(&cache, c, &NoOracle, main).files);
        // From an -iquote directory: on with the next one, <...> or not.
        assert_eq!(files(&c, &main), vec!["main.c", "q1/x.h", "q2/x.h"]);
        // From a file found next to its includer: GCC starts at the head
        // of the chain, Clang searches as #include does.
        assert_eq!(files(&c, &rel), vec!["rel/main.c", "rel/y.h", "q1/y.h"]);
        c.family = Family::Clang;
        assert_eq!(files(&c, &rel), vec!["rel/main.c", "rel/y.h", "inc/y.h"]);
        // In the main file: as #include, its directory first.
        for family in [Family::Gcc, Family::Clang] {
            c.family = family;
            assert_eq!(files(&c, &rel2), vec!["rel/main2.c", "rel/y2.h"]);
        }
        // Found next to an includer that came from the chain: Clang goes on
        // after the includer's directory, GCC starts at the head again.
        let inherits = d.file("inherit.c", "#include \"a.h\"\n");
        d.file("q1/a.h", "#include \"b.h\"\n");
        d.file("q1/b.h", "#ifndef B\n#define B\n#include_next <b.h>\n#endif\n");
        d.file("q2/b.h", "");
        d.file("inc/b.h", "");
        cache.new_session();
        c.family = Family::Clang;
        assert_eq!(files(&c, &inherits), vec!["inherit.c", "q1/a.h", "q1/b.h", "q2/b.h"]);
        c.family = Family::Gcc;
        assert_eq!(files(&c, &inherits), vec!["inherit.c", "q1/a.h", "q1/b.h"]);
        // Through the MSVC lookup's includers too.
        let stack = d.file("src/stack.cc", "#include <sub/s.h>\n");
        d.file("inc/sub/s.h", "#include \"s2.h\"\n");
        d.file("src/s2.h", "#include_next <s2.h>\n");
        d.file("inc/s2.h", "");
        d.file("sys/s2.h", "");
        cache.new_session();
        c.family = Family::Clang;
        c.msvc_lookup = true;
        assert_eq!(files(&c, &stack), vec!["src/stack.cc", "inc/sub/s.h", "src/s2.h", "sys/s2.h"]);
    }

    #[test]
    fn msvc_forced_lookup() {
        let d = Dir::new("msvc_forced");
        let src = d.file("src/main.cc", "");
        d.file("inc/pre.h", "#include \"x.h\"\n");
        d.file("src/x.h", "");
        d.file("other/x.h", "");
        d.file("src/pre2.h", "");
        let mut c = cfg(&d);
        c.angled_dirs = vec![d.0.join("inc").into(), d.0.join("other").into()];
        c.msvc_lookup = true;
        let cache = Cache::new(vec![], false);
        // cl's /FI: the source's directory is an includer's for what the
        // forced file includes.
        c.forced = vec![Forced::Msvc("pre.h".into())];
        assert_eq!(d.rel(&scan(&cache, &c, &NoOracle, &src).files), vec!["inc/pre.h", "src/x.h", "src/main.cc"]);
        // Clang's -include with MSVC compatibility: the working directory,
        // then the source's.
        let gcc = |name: &str| Forced::Gcc { cwd: d.0.join("build"), name: name.into() };
        c.forced = vec![gcc("pre.h"), gcc("pre2.h")];
        assert_eq!(d.rel(&scan(&cache, &c, &NoOracle, &src).files), vec!["inc/pre.h", "src/x.h", "src/pre2.h", "src/main.cc"]);
        // Neither without it.
        c.msvc_lookup = false;
        let out = scan(&cache, &c, &NoOracle, &src);
        assert_eq!(d.rel(&out.files), vec!["inc/pre.h", "other/x.h", "src/main.cc"]);
        assert_eq!(out.missing, vec!["pre2.h"]);
    }

    #[test]
    fn trigraphs_and_line_endings() {
        let d = Dir::new("lexing");
        let main = d.file("main.c", "??=include \"t.h\"\n#include \"a.h\"\r#include \"b.h\"\rint x;\r");
        for f in ["t.h", "a.h", "b.h"] {
            d.file(f, "");
        }
        let mut c = cfg(&d);
        let cache = Cache::new(vec![], false);
        // A lone carriage return ends a line; trigraphs are text when off.
        let out = scan(&cache, &c, &NoOracle, &main);
        assert_eq!(d.rel(&out.files), vec!["main.c", "a.h", "b.h"]);
        assert!(out.exact(), "{:?}", out.inexact);
        // When on, the file's directives are unknown.
        c.trigraphs = true;
        assert!(!scan(&cache, &c, &NoOracle, &main).exact());
        // cl doesn't end lines at a lone carriage return.
        c.trigraphs = false;
        c.family = Family::Msvc;
        assert!(!scan(&cache, &c, &NoOracle, &main).exact());
    }

    #[test]
    fn counter_and_line() {
        let d = Dir::new("counter");
        d.file("a.h", "");
        d.file("b.h", "");
        let scanned = |name: &str, text: &str| {
            let out = scan(&Cache::new(vec![], false), &cfg(&d), &NoOracle, &d.file(name, text));
            assert!(out.missing.is_empty(), "{:?}", out.missing);
            out
        };
        // __COUNTER__ counts its uses in text, which isn't expanded: both
        // branches, inexact.
        let out = scanned("counter.c", "int x = __COUNTER__;\n#if __COUNTER__ == 1\n#include \"a.h\"\n#else\n#include \"b.h\"\n#endif\n");
        assert_eq!(d.rel(&out.files), vec!["counter.c", "a.h", "b.h"]);
        assert!(!out.exact());
        assert!(scanned("text.c", "int x = __COUNTER__;\n#include \"a.h\"\n").exact());
        // #line changes __LINE__ and __FILE__, not lookups.
        assert!(!scanned("line.c", "#line 100\n#if __LINE__ == 101\n#include \"a.h\"\n#endif\n").exact());
        assert!(!scanned("marker.c", "# 100 \"x.c\"\n#if __LINE__ == 101\n#include \"a.h\"\n#endif\n").exact());
        let out = scanned("line2.c", "#line 10 \"sub/x.c\"\n#include \"a.h\"\n#if __LINE__ == 3\n#endif\n");
        assert!(!out.exact());
        let out = scanned("line3.c", "#line 10 \"sub/x.c\"\n#include \"a.h\"\n");
        assert_eq!(d.rel(&out.files), vec!["line3.c", "a.h"]);
        assert!(out.exact(), "{:?}", out.inexact);
        assert!(scanned("line4.c", "#if __LINE__ == 1\n#include \"a.h\"\n#endif\n").exact());
    }

    #[test]
    fn file_macro_and_import() {
        let d = Dir::new("file_macro");
        let main = d.file("sub/main.c", "#ifndef AGAIN\n#define AGAIN\n#include __FILE__\n#endif\n");
        d.file("sub/sub/main.c", "");
        let mut c = cfg(&d);
        // Absolute paths on the command line: __FILE__ names the file itself.
        let out = scan(&Cache::new(vec![], false), &c, &NoOracle, &main);
        assert_eq!(d.rel(&out.files), vec!["sub/main.c"]);
        assert!(out.exact(), "{:?}", out.inexact);
        // Relative ones: compilers spell it `sub/main.c`, found next to the
        // file (`sub/sub/main.c`).
        c.relative_paths = true;
        assert!(!scan(&Cache::new(vec![], false), &c, &NoOracle, &main).exact());
        // cl's #import reads a type library, Objective-C's a header.
        let import = d.file("import.cc", "#import \"x.tlb\"\n");
        d.file("x.tlb", "");
        let mut c = cfg(&d);
        assert!(scan(&Cache::new(vec![], false), &c, &NoOracle, &import).exact());
        c.family = Family::Msvc;
        assert!(!scan(&Cache::new(vec![], false), &c, &NoOracle, &import).exact());
        // An assembler's .include reads what the preprocessor doesn't.
        let asm = d.file("blake3.S", "#include \"x.tlb\"\n.include \"macros.s\"\n");
        assert!(!scan(&Cache::new(vec![], false), &cfg(&d), &NoOracle, &asm).exact());
    }

    #[test]
    fn pragmas_the_scan_cant_see() {
        let d = Dir::new("pragmas");
        d.file("a.h", "");
        d.file("real.h", "");
        let exact = |name: &str, text: &str| {
            let out = scan(&Cache::new(vec![], false), &cfg(&d), &NoOracle, &d.file(name, text));
            assert!(out.missing.is_empty(), "{:?}", out.missing);
            out.exact()
        };
        assert!(!exact("alias.c", "#pragma include_alias(\"a.h\", \"real.h\")\n#include \"a.h\"\n"));
        assert!(!exact("alias2.c", "_Pragma(\"include_alias(\\\"a.h\\\", \\\"real.h\\\")\")\n#include \"a.h\"\n"));
        // From macros expanded in text: unknown, but for pushing and popping
        // macros that don't change meanwhile (libc++'s _LIBCPP_PUSH_MACROS).
        let push = "#define PUSH _Pragma(\"push_macro(\\\"M\\\")\") _Pragma(\"push_macro(\\\"N\\\")\")\n#define POP _Pragma(\"pop_macro(\\\"M\\\")\")\n";
        assert!(exact("push.c", &format!("{push}PUSH\n#include \"a.h\"\nPOP\n")));
        assert!(!exact("push2.c", &format!("{push}PUSH\n#define M 2\nPOP\n#if M == 2\n#endif\n")));
        assert!(!exact("push3.c", &format!("#define N 1\n{push}PUSH\n#undef N\nPOP\n")));
        assert!(!exact("once.c", "#define ONCE _Pragma(\"once\")\nONCE\n"));
        assert!(!exact("ms.c", "#define ONCE __pragma(once)\nONCE\n"));
        assert!(!exact("generic.c", "#define DO_PRAGMA(x) _Pragma(#x)\nDO_PRAGMA(push_macro(\"M\"))\n"));
        assert!(!exact("generic2.c", "#define PRAGMA(x) _Pragma(x)\nPRAGMA(\"once\")\n"));
        assert!(!exact("operand.c", "#define ONCE_STR \"once\"\n_Pragma(ONCE_STR)\n"));
        assert!(exact("diagnostic.c", "#define DO_PRAGMA(x) _Pragma(#x)\n#define PUSH _Pragma(\"GCC diagnostic push\")\nDO_PRAGMA(GCC diagnostic pop)\n"));
    }

    #[test]
    fn query_operands() {
        let d = Dir::new("operands");
        d.file("yes.h", "");
        let mut c = cfg(&d);
        c.predefined = Arc::new(macro_table("#define __GNUC__ 4\n#define P __builtin_expect\n"));
        let oracle = Answers(
            [(("__has_builtin".to_string(), "__builtin_expect".to_string()), 1), (("__has_builtin".to_string(), "P".to_string()), 1)].into_iter().collect(),
        );
        let scanned = |name: &str, text: &str| {
            let out = scan(&Cache::new(vec![], false), &c, &oracle, &d.file(name, text));
            assert!(out.missing.is_empty(), "{:?}", out.missing);
            out
        };
        // GCC expands a query's operand, Clang mostly doesn't: a macro of
        // the source makes the answer unknown.
        let query = "#if __has_builtin(B)\n#include \"yes.h\"\n#endif\n";
        assert!(!scanned("macro.c", &format!("#define B __builtin_expect\n{query}")).exact());
        assert!(scanned("plain.c", "#if __has_builtin(__builtin_expect)\n#include \"yes.h\"\n#endif\n").exact());
        // A predefined one is the compiler's: it answered for it.
        let out = scanned("predefined.c", "#if __has_builtin(P)\n#include \"yes.h\"\n#endif\n");
        assert!(out.exact(), "{:?}", out.inexact);
        assert!(!scanned("redefined.c", "#undef P\n#define P x\n#if __has_builtin(P)\n#endif\n").exact());
    }

    #[test]
    fn command_line_values() {
        let d = Dir::new("values");
        let main = d.file("main.c", "#if defined(Y) && !defined(W) && Z == 1\n#include \"yes.h\"\n#endif\n");
        d.file("yes.h", "");
        let mut c = cfg(&d);
        // A value ending with a backslash doesn't take the next -D; one
        // ends at a newline.
        c.commands = vec![CommandMacro::Define("X=a\\".into()), CommandMacro::Define("Y".into()), CommandMacro::Define("Z=1\n#define W".into())];
        let out = scan(&Cache::new(vec![], false), &c, &NoOracle, &main);
        assert_eq!(d.rel(&out.files), vec!["main.c", "yes.h"]);
        assert!(out.exact(), "{:?}", out.inexact);
        assert_eq!(command_text(&c.commands[..1]), "#define X a\\\\\n\n");
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
        c.forced = vec![Forced::Path(forced)];
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

    #[test]
    fn frameworks_and_header_maps() {
        let d = Dir::new("frameworks");
        // A name without a framework part goes on past the framework
        // directory (`-F` comes before `inc` here).
        let main = d.file(
            "main.m.c",
            "#include <Foo/Foo.h>\n#include <Foo/Private.h>\n#include \"Mapped/Name.h\"\n#include <Foo/Missing.h>\n#if __has_include(<plain.h>)\n#include <plain.h>\n#endif\n",
        );
        d.file("fw/Foo.framework/Headers/Foo.h", "#include <Foo/Other.h>\n");
        d.file("fw/Foo.framework/Headers/Other.h", "");
        d.file("fw/Foo.framework/PrivateHeaders/Private.h", "");
        d.file("real/name.h", "");
        d.file("inc/plain.h", "");
        let hmap = d.0.join("x.hmap");
        let real = d.0.join("real").to_string_lossy().replace('\\', "/") + "/";
        std::fs::write(&hmap, crate::hmap::build(&[("Mapped/Name.h", &real, "name.h")])).unwrap();
        let mut c = cfg(&d);
        c.quote_dirs = vec![SearchDir::HeaderMap(Arc::new(HeaderMap::load(&hmap, &d.0).unwrap()))];
        c.angled_dirs.insert(0, SearchDir::Framework(d.0.join("fw")));
        let cache = Cache::new(vec![], false);
        let out = scan(&cache, &c, &NoOracle, &main);
        assert_eq!(
            d.rel(&out.files),
            vec![
                "main.m.c",
                "fw/Foo.framework/Headers/Foo.h",
                "fw/Foo.framework/Headers/Other.h",
                "fw/Foo.framework/PrivateHeaders/Private.h",
                "real/name.h",
                "inc/plain.h"
            ]
        );
        assert_eq!(out.missing, vec!["Foo/Missing.h"]);
    }

    #[test]
    fn byte_order_marks_and_forced_lookup() {
        let d = Dir::new("bom");
        // A UTF-8 byte order mark doesn't hide the first line's directive.
        let main = d.0.join("main.c");
        std::fs::write(&main, b"\xEF\xBB\xBF#include \"a.h\"\n").unwrap();
        d.file("a.h", "");
        let cache = Cache::new(vec![], false);
        let out = scan(&cache, &cfg(&d), &NoOracle, &main);
        assert_eq!(d.rel(&out.files), vec!["main.c", "a.h"]);
        // UTF-16 isn't scanned: inexact.
        let wide = d.0.join("wide.c");
        std::fs::write(&wide, [0xff, 0xfe, b'#', 0]).unwrap();
        let w = scan(&Cache::new(vec![], false), &cfg(&d), &NoOracle, &wide);
        assert!(!w.exact(), "{w:?}");
        // Forced includes are found as the compilers find them: GCC's in the
        // working directory, then along the quote chain; cl's next to the
        // source first.
        d.file("inc/config.h", "");
        d.file("src/pre.h", "");
        d.file("inc/pre.h", "");
        let src = d.file("src/x.c", "");
        let cache = Cache::new(vec![], false);
        let mut c = cfg(&d);
        c.forced = vec![Forced::Gcc { cwd: d.0.join("build"), name: "config.h".into() }];
        assert_eq!(d.rel(&scan(&cache, &c, &NoOracle, &src).files), vec!["inc/config.h", "src/x.c"]);
        c.forced = vec![Forced::Msvc("pre.h".into())];
        c.msvc_lookup = true;
        assert_eq!(d.rel(&scan(&cache, &c, &NoOracle, &src).files), vec!["src/pre.h", "src/x.c"]);
    }

    #[test]
    fn overlays() {
        let d = Dir::new("overlay");
        // A case-insensitive view of real headers, as for Windows SDKs.
        let main = d.file("main.c", "#include <Windows.h>\n#include <Shared.H>\n#include <plain.h>\n");
        d.file("sdk/real/windows.h", "#include \"Sub/Inner.h\"\n#pragma once\n");
        d.file("sdk/real/sub/inner.h", "");
        d.file("sdk/real/shared.h", "");
        d.file("inc/plain.h", "");
        let overlay = d.file(
            "overlays/o.yaml",
            "{ 'version': 0, 'case-sensitive': 'false', 'root-relative': 'overlay-dir', 'overlay-relative': true, 'roots': [\n\
               { 'name': '../sdk/include', 'type': 'directory', 'contents': [\n\
                 { 'name': 'windows.h', 'type': 'file', 'external-contents': '../sdk/real/windows.h' },\n\
                 { 'name': 'shared.h', 'type': 'file', 'external-contents': '../sdk/real/shared.h' },\n\
                 { 'name': 'sub', 'type': 'directory', 'contents': [ { 'name': 'inner.h', 'type': 'file', 'external-contents': '../sdk/real/sub/inner.h' } ] } ] } ] }\n",
        );
        let mut c = cfg(&d);
        c.angled_dirs.push(d.0.join("sdk/include").into());
        c.overlays = vec![Arc::new(Overlay::load(&overlay, &d.0).unwrap())];
        let cache = Cache::new(vec![], false);
        let out = scan(&cache, &c, &NoOracle, &main);
        // The real files are reported (they're the inputs).
        assert_eq!(d.rel(&out.files), vec!["main.c", "sdk/real/windows.h", "sdk/real/sub/inner.h", "sdk/real/shared.h", "inc/plain.h"]);
        assert!(out.missing.is_empty(), "{:?}", out.missing);
    }

    /// A watch covering every path, whose changes the test makes.
    #[derive(Default)]
    struct FakeWatch {
        seq: AtomicU64,
        covers: std::sync::atomic::AtomicBool,
    }
    impl Watch for FakeWatch {
        fn token(&self, _: &Path) -> Option<u64> {
            self.covers.load(Ordering::Relaxed).then(|| self.seq.load(Ordering::Relaxed))
        }
        fn unchanged_since(&self, token: u64) -> bool {
            self.seq.load(Ordering::Relaxed) == token
        }
    }

    #[test]
    fn watched_entries() {
        let d = Dir::new("watched");
        let a = d.file(
            "a.h",
            "#define A 1
",
        );
        let cache = Cache::new(vec![], cfg!(windows));
        let watch = Arc::new(FakeWatch::default());
        watch.covers.store(true, Ordering::Relaxed);
        cache.set_watch(watch.clone());
        let lexed = || cache.stats().files_lexed;
        let first = cache.directives(&a).unwrap();
        assert_eq!(lexed(), 1);
        // A new session doesn't check what the watch vouches for.
        d.file(
            "a.h",
            "#define A 22
",
        );
        cache.new_session();
        assert!(Arc::ptr_eq(&cache.directives(&a).unwrap(), &first));
        assert!(cache.exists(&a));
        // Until it reports a change (with the case on disk, which may
        // differ from the include's on Windows).
        cache.changed(&d.0.join(if cfg!(windows) { "A.H" } else { "a.h" }), Change::Content);
        assert!(!Arc::ptr_eq(&cache.directives(&a).unwrap(), &first));
        assert_eq!(lexed(), 2);
        // Names: a file created in a listed directory.
        let b = d.0.join("b.h");
        assert!(!cache.exists(&b));
        d.file("b.h", "");
        cache.new_session();
        assert!(!cache.exists(&b));
        cache.changed(&d.0, Change::Names);
        assert!(cache.exists(&b));
        // A change seen during a check: valid for the session only.
        d.file(
            "a.h",
            "#define A 333
",
        );
        cache.changed(&a, Change::Content);
        watch.seq.store(7, Ordering::Relaxed);
        let racing = Cache::new(vec![], cfg!(windows));
        struct Racing(Arc<FakeWatch>);
        impl Watch for Racing {
            fn token(&self, _: &Path) -> Option<u64> {
                let t = self.0.seq.fetch_add(1, Ordering::Relaxed);
                Some(t)
            }
            fn unchanged_since(&self, token: u64) -> bool {
                self.0.seq.load(Ordering::Relaxed) == token
            }
        }
        racing.set_watch(Arc::new(Racing(watch.clone())));
        racing.directives(&a).unwrap();
        d.file(
            "a.h",
            "#define A 4444
",
        );
        racing.new_session();
        racing.directives(&a).unwrap();
        assert_eq!(racing.stats().files_lexed, 2);
        // Tree: everything under a directory.
        cache.directives(&a).unwrap();
        cache.changed(&d.0, Change::Tree);
        assert!(cache.is_empty());
    }

    /// A change reported while a file is lexed, between the check of its
    /// token and the entry's insertion: the entry doesn't outlive it.
    #[test]
    fn changed_while_lexed() {
        struct Late {
            cache: OnceLock<Arc<Cache>>,
            path: PathBuf,
            armed: std::sync::atomic::AtomicBool,
            event: Mutex<Option<std::thread::JoinHandle<()>>>,
        }
        impl Watch for Late {
            fn token(&self, _: &Path) -> Option<u64> {
                Some(0)
            }
            fn unchanged_since(&self, _: u64) -> bool {
                // The event is reported now, by the watcher's thread; the
                // check saw none.
                if self.armed.swap(false, Ordering::SeqCst) {
                    let mut event = self.event.lock().unwrap();
                    let (cache, path) = (self.cache.get().unwrap().clone(), self.path.clone());
                    let (tx, rx) = std::sync::mpsc::channel();
                    *event = Some(std::thread::spawn(move || {
                        cache.changed(&path, Change::Content);
                        let _ = tx.send(());
                    }));
                    let _ = rx.recv_timeout(std::time::Duration::from_millis(200));
                }
                true
            }
        }
        let d = Dir::new("late");
        let p = d.file("x.h", "#include \"a.h\"\n");
        let cache = Arc::new(Cache::new(vec![], cfg!(windows)));
        let late = Arc::new(Late { cache: OnceLock::new(), path: p.clone(), armed: true.into(), event: Mutex::default() });
        let _ = late.cache.set(cache.clone());
        cache.set_watch(late.clone());
        let first = cache.directives(&p).unwrap();
        late.event.lock().unwrap().take().unwrap().join().unwrap();
        d.file("x.h", "#include \"bbbbbbbbbbbbbbbbbbbbbbbb.h\"\n");
        cache.new_session();
        assert!(!Arc::ptr_eq(&cache.directives(&p).unwrap(), &first));
    }

    /// A file replaced by one of the same size with its modification time
    /// restored (`cp -p`, an archive extracted over it): lexed again.
    #[cfg(unix)]
    #[test]
    fn same_size_and_time() {
        let d = Dir::new("stamp");
        let p = d.file("x.h", "#include \"a.h\"\n");
        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        let cache = Cache::new(vec![], false);
        let first = cache.directives(&p).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        d.file("x.h", "#include \"b.h\"\n");
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(mtime).unwrap();
        cache.new_session();
        assert!(!Arc::ptr_eq(&cache.directives(&p).unwrap(), &first));
    }

    /// A listing with a symbolic link holds for a session only: whether the
    /// link leads to a file changes without its directory changing (a
    /// dangling link's target created elsewhere).
    #[cfg(unix)]
    #[test]
    fn listings_with_links() {
        let d = Dir::new("links");
        std::fs::create_dir_all(d.0.join("inc")).unwrap();
        std::fs::create_dir_all(d.0.join("gen")).unwrap();
        std::os::unix::fs::symlink("../gen/foo.h", d.0.join("inc/foo.h")).unwrap();
        let cache = Cache::new(vec![], false);
        let watch = Arc::new(FakeWatch::default());
        watch.covers.store(true, Ordering::Relaxed);
        cache.set_watch(watch);
        let link = d.0.join("inc/foo.h");
        assert!(!cache.exists(&link));
        d.file("gen/foo.h", "");
        cache.new_session();
        assert!(cache.exists(&link));
    }

    #[test]
    fn embed_and_pragma_operators() {
        let d = Dir::new("embed");
        let main = d.file(
            "main.c",
            "#embed \"local.bin\"\n#embed <data.bin> limit(4)\n#if __has_embed(<empty.bin>) == 2 && __has_embed(\"local.bin\" limit(0)) == 2 && !__has_embed(<none.bin>)\n#include <yes.h>\n#endif\n\
             #include <once.h>\n#include <once.h>\n#define M 1\n_Pragma(\"push_macro(\\\"M\\\")\") int x;\n#undef M\n_Pragma(\"pop_macro(\\\"M\\\")\")\n#if M == 1\n#include <popped.h>\n#endif\n",
        );
        d.file("local.bin", "x");
        d.file("data/data.bin", "abc");
        d.file("data/empty.bin", "");
        d.file("inc/once.h", "_Pragma(\"once\")\n#include <counted.h>\n");
        for f in ["inc/yes.h", "inc/counted.h", "inc/popped.h"] {
            d.file(f, "");
        }
        let mut c = cfg(&d);
        c.embed_dirs = vec![d.0.join("data")];
        c.operators = Arc::new(["__has_builtin", "__has_embed"].iter().map(|s| intern::intern(s)).collect());
        let cache = Cache::new(vec![], false);
        let out = scan(&cache, &c, &NoOracle, &main);
        assert_eq!(d.rel(&out.files), vec!["main.c", "local.bin", "data/data.bin", "inc/yes.h", "inc/once.h", "inc/counted.h", "inc/popped.h"]);
        assert_eq!(d.rel(&out.found), vec!["data/empty.bin", "local.bin"]);
        assert!(out.exact(), "{:?}", out.inexact);
    }
}
