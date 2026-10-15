//! Scanning a translation unit: the preprocessor's directives evaluated
//! exactly (macros, conditionals, includes), text skipped.
//!
//! A conditional the scanner can't decide (an unanswered compiler query, a
//! malformed expression) processes every branch it can't rule out, and the
//! result is marked inexact: it's then likely a superset, but no guarantee.

use ninja_os::FileStamp;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::expr;
use crate::hmap::HeaderMap;
use crate::intern::{self, Sym};
use crate::lex::{self, DirKind, Directive, FileDirectives, Kind, P, Token};
use crate::macros::{self, Context, Expander, Macro, Pragmas, XTok};
use crate::path_map::PathMap;
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
    /// The source is assembly (`.S`): text may be an assembler's
    /// `.include`, which reads a file the preprocessor doesn't see.
    pub assembler: bool,
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

/// A file's identity: its volume's serial number and file ID (Windows),
/// its device and inode (Unix); zeros when the file system has none.
pub type FileId = (u64, u128);

/// A file's stamp (what tells its versions apart: see `ninja_os::FileStamp`,
/// which sees a file rewritten in place with its size and modification
/// time restored, as an archive extracted over it leaves it) and identity,
/// when it's a file.
fn stat(p: &Path) -> Option<(FileStamp, FileId)> {
    let (meta, stamp) = ninja_os::stat(p).ok().filter(|(m, _)| m.is_file())?;
    #[cfg(unix)]
    let id = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), u128::from(meta.ino()))
    };
    #[cfg(not(unix))]
    let id = {
        let _ = meta;
        (stamp.volume, stamp.id)
    };
    Some((stamp, id))
}

/// A file's key and stamp (what tells its versions apart: a file replaced
/// by one of the same size with its modification time restored, as `cp -p`
/// or an archive extraction leaves it, has a new one).
type Version = (PathBuf, FileStamp);
type Lexing = Arc<OnceLock<Option<Arc<FileDirectives>>>>;

/// The session of entries a watch vouches for: valid until it reports a
/// change (`Cache::changed`).
const WATCHED: u64 = u64::MAX;

struct Entry {
    stamp: FileStamp,
    id: FileId,
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
/// concurrent scans don't wait for each other. What's under a directory
/// goes without going through the rest (see `PathMap`).
pub struct Cache {
    files: RwLock<PathMap<Arc<Entry>>>,
    /// Files being lexed (key and stamp): concurrent scans wait for the one
    /// lexing.
    lexing: Mutex<HashMap<Version, Lexing>>,
    dirs: RwLock<PathMap<Listing>>,
    /// Whether a path is a symbolic link leading elsewhere than its own
    /// directory (see `dotdot_after_link`), and the session it's from.
    links: RwLock<HashMap<PathBuf, (u64, bool)>>,
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
            links: RwLock::default(),
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
                self.files.write().unwrap().remove_under(&key);
                self.dirs.write().unwrap().remove_under(&key);
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
        !volatile.is_empty()
            && key.ancestors().any(|a| {
                #[cfg(test)]
                tests::VOLATILE_LOOKUPS.with(|n| n.set(n.get() + 1));
                volatile.contains_key(a)
            })
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
        self.file(p).map(|(fd, _, _)| fd)
    }

    /// A file's directives (lexed once per version), stamp and identity.
    fn file(&self, p: &Path) -> Option<(Arc<FileDirectives>, FileStamp, FileId)> {
        let key = self.key(p);
        let volatile = self.is_volatile(p);
        let known = self.files.read().unwrap().get(key.as_ref()).cloned();
        if let Some(e) = &known
            && !volatile
            && self.current(e.session.load(Ordering::Relaxed))
        {
            self.files_reused.fetch_add(1, Ordering::Relaxed);
            return Some((e.fd.clone(), e.stamp, e.id));
        }
        let token = if volatile { None } else { self.token(p) };
        let (stamp, id) = stat(p)?;
        if let Some(e) = known
            && e.stamp == stamp
        {
            e.session.store(self.checked(token), Ordering::Relaxed);
            self.files_reused.fetch_add(1, Ordering::Relaxed);
            return Some((e.fd.clone(), stamp, id));
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
                files.insert(version.0.clone(), Arc::new(Entry { stamp, id, session: AtomicU64::new(self.checked(token)), fd: fd.clone() }));
                Some(fd)
            })
            .clone();
        if lexed {
            self.lexing.lock().unwrap().remove(&version);
        } else {
            self.files_reused.fetch_add(1, Ordering::Relaxed);
        }
        fd.map(|fd| (fd, stamp, id))
    }

    /// `dotdot_after_link`, with what links are taken once per session.
    pub fn dotdot_after_link(&self, p: &Path) -> Option<PathBuf> {
        dotdot_after(p, &mut |link| {
            let session = self.session();
            let key = self.key(link);
            if let Some((s, elsewhere)) = self.links.read().unwrap().get(key.as_ref())
                && *s == session
            {
                return *elsewhere;
            }
            let elsewhere = link_elsewhere(link);
            self.links.write().unwrap().insert(key.into_owned(), (session, elsewhere));
            elsewhere
        })
    }
}

/// Where a `..` of `p` comes after a symbolic link that leads elsewhere
/// than its own directory: compilers go to the parent of the link's target
/// (`lnk/../b.h` is `real/b.h`), unlike remote workers, whose inputs are
/// plain directories, and unlike `normalize`. Never on Windows, which
/// applies `..` to a path before following its links (or junctions), as
/// `normalize` does.
pub fn dotdot_after_link(p: &Path) -> Option<PathBuf> {
    dotdot_after(p, &mut |link| link_elsewhere(link))
}

fn dotdot_after(p: &Path, elsewhere: &mut dyn FnMut(&Path) -> bool) -> Option<PathBuf> {
    if cfg!(windows) || !p.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    let mut prefix = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if elsewhere(&prefix) {
                    return Some(prefix);
                }
                if !prefix.pop() {
                    prefix.push("..");
                }
            }
            Component::CurDir => {}
            c => prefix.push(c.as_os_str()),
        }
    }
    None
}

/// Whether `p` is a symbolic link (or junction) to somewhere not in its own
/// directory.
fn link_elsewhere(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
        && match (std::fs::canonicalize(p), p.parent().map(std::fs::canonicalize)) {
            (Ok(target), Some(Ok(dir))) => target.parent() != Some(dir.as_path()),
            // Dangling: compilers find nothing there.
            _ => true,
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
    /// Clang's other possible place for it (see `State::resolve_from`).
    alt_at: Option<Option<usize>>,
    /// Found next to an includer (or in the working directory).
    relative: bool,
    line: u32,
    /// A `#line` changed `__LINE__` and `__FILE__`.
    line_set: bool,
    /// Its identity, size and contents' hash (for `#pragma once`).
    id: FileId,
    len: u64,
    hash: u64,
}

/// A file found by a lookup: its path as the preprocessor knows it, its
/// real path, and where: in the search chain, or next to an includer.
struct Found {
    path: PathBuf,
    real: PathBuf,
    at: Option<usize>,
    alt_at: Option<Option<usize>>,
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
    /// The macros the `_Pragma` operands of macros defined so far were
    /// expanded with (see `macros::operand_pragmas`), and those macros:
    /// classified again when one changes.
    pragma_users: HashMap<Sym, Vec<Sym>>,
    /// The macros defined so far whose pragmas their arguments make (see
    /// `Context::arg_pragma`), and their names' hashes (see
    /// `lex::name_hash`), for text that calls them.
    arg_pragmas: HashSet<Sym>,
    arg_pragma_hashes: Vec<u64>,
    /// The macros undefined so far (see `text_runs_pragma`).
    undefined: HashSet<Sym>,
    /// The files with `#pragma once` (or `#import`ed), by key; their
    /// paths by identity, and by size and contents (see `once_elsewhere`).
    once: HashSet<String>,
    once_ids: HashMap<FileId, PathBuf>,
    once_contents: HashMap<(u64, u64), PathBuf>,
    /// What lookups noted that makes the scan inexact (see `join`).
    notes: std::cell::RefCell<Vec<String>>,
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
        match self.resolve_checked(name, angled, next) {
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

    fn arg_pragma(&self, name: Sym) -> bool {
        !self.arg_pragmas.is_empty() && self.arg_pragmas.contains(&name)
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
        Some(Found { path, real, at, alt_at: None, relative: false })
    }

    /// `dir/name`, `..` applied lexically: as compilers spell the paths
    /// they report, and as remote workers (whose inputs are plain
    /// directories) find them. A `..` after a symbolic link leads elsewhere
    /// on this machine: noted (the scan is then inexact).
    fn join(&self, dir: &Path, name: &Path) -> PathBuf {
        let joined = dir.join(name);
        if let Some(link) = self.cache.dotdot_after_link(&joined) {
            let note = format!("{}: `..` after a symbolic link: {}", self.here(), link.display());
            let mut notes = self.notes.borrow_mut();
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
        normalize(&joined)
    }

    /// Finds an included file.
    fn resolve(&self, name: &str, angled: bool, next: bool) -> Option<Found> {
        let from = self.stack.last().map_or((None, false), |f| (f.found_at, f.relative));
        self.resolve_from(name, angled, next, from)
    }

    /// `resolve`, from a file whose place for `#include_next` Clang may take
    /// one of two ways (see `resolve_from`): inexact when they find
    /// different files.
    fn resolve_checked(&mut self, name: &str, angled: bool, next: bool) -> Option<Found> {
        let found = self.resolve(name, angled, next);
        let alt = self.stack.last().filter(|_| next).and_then(|f| f.alt_at.map(|at| (at, f.relative)));
        if let Some(from) = alt {
            let other = self.resolve_from(name, angled, true, from);
            let place = |f: &Option<Found>| f.as_ref().map(|f| (f.real.clone(), f.at));
            if place(&other) != place(&found) {
                let why = "#include_next in a header found next to an outer includer (where Clang goes on depends on -Wmicrosoft-include)";
                self.inexact(format!("{}: {why}", self.here()));
            }
        }
        found
    }

    /// Finds an included file; for `#include_next`, `from` is where the
    /// current file was found in the chain, and whether next to an
    /// includer.
    fn resolve_from(&self, name: &str, angled: bool, next: bool, (found_at, relative): (Option<usize>, bool)) -> Option<Found> {
        let p = Path::new(name);
        if p.is_absolute() {
            return self.candidate(normalize(p), None);
        }
        // `#include_next` goes on after the directory the current file was
        // found in, `<...>` or not. From a file found otherwise, GCC starts
        // at the head of the chain when it was found next to its includer,
        // and searches as `#include` does when it's the main file or was
        // found by its absolute path; Clang searches as `#include`.
        let after = match (next, found_at) {
            (false, _) => None,
            (true, Some(i)) => Some(i + 1),
            (true, None) if relative && self.cfg.family == Family::Gcc => Some(0),
            (true, None) => None,
        };
        if !angled && after.is_none() {
            let dirs: Vec<&PathBuf> = if self.cfg.msvc_lookup {
                self.stack.iter().rev().map(|f| &f.dir).chain(&self.base).collect()
            } else {
                self.stack.last().map(|f| &f.dir).into_iter().collect()
            };
            let current = self.stack.last().and_then(|f| f.found_at);
            for (k, d) in dirs.into_iter().enumerate() {
                let Some(f) = self.candidate(self.join(d, p), None) else { continue };
                // Clang: a file found next to the current file has its place
                // in the chain. Next to an outer includer (MSVC's lookup;
                // `base`, after the working directory, for `-include`), Clang
                // goes on along the chain when -Wmicrosoft-include is on (as
                // by default, outside system headers): the place of the first
                // directory with that name, if any; else the current file's.
                let (at, alt_at) = match self.cfg.family {
                    Family::Clang if k > 0 || self.stack.is_empty() => {
                        let warned = self.search_chain(name, 0).and_then(|f| f.at);
                        (warned, (warned != current).then_some(current))
                    }
                    Family::Clang => (current, None),
                    _ => (None, None),
                };
                return Some(Found { relative: true, at, alt_at, ..f });
            }
        }
        let start = after.unwrap_or(if angled { self.cfg.quote_dirs.len() } else { 0 });
        self.search_chain(name, start)
    }

    /// Searches the chain, from its entry `start`, for `name`.
    fn search_chain(&self, name: &str, start: usize) -> Option<Found> {
        let mut name = Cow::Borrowed(name);
        let chain = self.cfg.quote_dirs.iter().chain(&self.cfg.angled_dirs).enumerate().skip(start);
        for (i, d) in chain {
            let found = match d {
                SearchDir::Dir(d) => self.candidate(self.join(d, Path::new(name.as_ref())), Some(i)),
                SearchDir::Framework(d) => {
                    // Only `Framework/Header.h` names are in frameworks: the
                    // search goes on with the next directory.
                    let Some((framework, rest)) = name.split_once(['/', '\\']) else { continue };
                    ["Headers", "PrivateHeaders"]
                        .iter()
                        .find_map(|sub| self.candidate(normalize(&d.join(format!("{framework}.framework")).join(sub).join(rest)), Some(i)))
                }
                SearchDir::HeaderMap(m) => match m.lookup(&name) {
                    None => None,
                    Some(dest) if Path::new(dest).is_absolute() => self.candidate(normalize(Path::new(dest)), Some(i)),
                    // A relative destination is another name, which Clang
                    // looks up in the map again (a file there, relative to
                    // the working directory), and searches the directories
                    // after it for.
                    Some(dest) => {
                        let found = m.lookup(dest).and_then(|again| self.candidate(normalize(&m.cwd.join(again)), Some(i)));
                        name = Cow::Owned(dest.to_string());
                        found
                    }
                },
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
        current.into_iter().chain(&self.cfg.embed_dirs).find_map(|d| self.locate(&self.join(d, p)))
    }

    fn record(&mut self, p: &Path) {
        if self.seen.insert(self.key(p)) {
            self.out.files.push(p.to_path_buf());
        }
    }

    fn include_file(&mut self, f: Found, import: bool) {
        let Found { path, real, at: found_at, alt_at, relative } = f;
        let key = self.key(&real);
        if self.once.contains(&key) {
            return;
        }
        let Some((fd, stamp, id)) = self.cache.file(&real) else {
            self.out.missing.push(path.to_string_lossy().into_owned());
            return;
        };
        self.record(&real);
        if self.once_elsewhere(&real, &fd, stamp.len, id) {
            return;
        }
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
        if fd.asm_include && self.cfg.assembler {
            self.inexact(format!("{}: an assembler's .include or .incbin", real.display()));
        }
        if let Some(g) = fd.guard
            && self.macros.contains_key(&g)
        {
            return;
        }
        if import {
            self.mark_once(key, real.clone(), id, (stamp.len, fd.hash));
        }
        if self.stack.len() >= 200 {
            self.inexact(format!("{}: #include nested too deeply", self.here()));
            return;
        }
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        self.stack.push(Frame { path, real, dir, found_at, alt_at, relative, line: 0, line_set: false, id, len: stamp.len, hash: fd.hash });
        self.run(&fd);
        // Its text calls a macro whose pragmas its arguments make: text
        // isn't expanded.
        let frame = self.stack.pop();
        if self.arg_pragma_hashes.iter().any(|h| fd.calls.binary_search(h).is_ok())
            && let Some(f) = frame
            && self.text_runs_pragma(&f.real, &fd)
        {
            self.inexact(format!("{}:{}: text calls a macro whose pragmas its arguments make", f.path.display(), f.line));
        }
    }

    /// Marks a file `#pragma once` (or `#import`ed): `contents` its size
    /// and contents' hash.
    fn mark_once(&mut self, key: String, real: PathBuf, id: FileId, contents: (u64, u64)) {
        if self.once.insert(key) {
            if id != (0, 0) {
                self.once_ids.entry(id).or_insert_with(|| real.clone());
            }
            self.once_contents.entry(contents).or_insert(real);
        }
    }

    /// Whether `real` is a file with `#pragma once` (or `#import`ed) under
    /// another name (a link), or has the contents of one (which GCC takes
    /// for it, given the same modification time): the compiler skips it
    /// here, but remote copies may differ (links not kept, identical files
    /// linked, other times): inexact, unless including it again changes
    /// nothing. (Contents compared by hash, as lexed: not read again.)
    fn once_elsewhere(&mut self, real: &Path, fd: &FileDirectives, len: u64, id: FileId) -> bool {
        let same_file = (id != (0, 0)).then(|| self.once_ids.get(&id)).flatten();
        let Some(other) = same_file.or_else(|| self.once_contents.get(&(len, fd.hash))).cloned() else { return false };
        if !self.again_changes_nothing(fd) {
            self.inexact(format!(
                "{}: the same as a file with #pragma once (whether it's read again depends on the file system): {}",
                real.display(),
                other.display()
            ));
        }
        true
    }

    /// Whether including a file again would change nothing the scan sees:
    /// its include guard is defined (after its `#pragma once`), or it has
    /// nothing but `#pragma once`, conditionals and directives that change
    /// nothing.
    fn again_changes_nothing(&self, fd: &FileDirectives) -> bool {
        let once = |d: &Directive| d.kind == DirKind::Pragma && d.toks.first().is_some_and(|t| t.sym == intern::ONCE);
        let after_once = fd.dirs.iter().position(|d| !once(d)).unwrap_or(fd.dirs.len());
        fd.guard.or_else(|| lex::include_guard(&fd.dirs[after_once..])).is_some_and(|g| self.macros.contains_key(&g))
            || fd.dirs.iter().all(|d| match d.kind {
                DirKind::Pragma => d.toks.first().is_some_and(|t| t.sym == intern::ONCE),
                DirKind::If
                | DirKind::Ifdef
                | DirKind::Ifndef
                | DirKind::Elif
                | DirKind::Elifdef
                | DirKind::Elifndef
                | DirKind::Else
                | DirKind::Endif
                | DirKind::Line
                | DirKind::Other => true,
                _ => false,
            })
    }

    /// Whether a file's text, which may call macros whose pragmas their
    /// arguments make (see `arg_pragmas`), runs one: unless their every
    /// argument is words that aren't macros (nor were in this scan: text
    /// came before the end of the file) nor pragmas' (`DO_PRAGMA(GCC
    /// diagnostic push)`), strings that don't start with one, numbers or
    /// punctuation. Its text is read again for this, once per version.
    fn text_runs_pragma(&self, real: &Path, fd: &FileDirectives) -> bool {
        let text = fd.text.get_or_init(|| {
            let content = std::fs::read(real).ok().filter(|c| lex::content_hash(c) == fd.hash)?;
            Some(lex::text_tokens(&content).into_boxed_slice())
        });
        let Some(toks) = text else { return true };
        let harmless = |a: &Token| match a.kind {
            Kind::Ident => {
                !self.macros.contains_key(&a.sym)
                    && !self.undefined.contains(&a.sym)
                    && !DYNAMIC.contains(&a.sym)
                    && !macros::PRAGMAS.contains(&a.sym)
                    && a.sym != intern::PRAGMA_OPERATOR
                    && a.sym != intern::MS_PRAGMA_OPERATOR
            }
            Kind::Str => !macros::pragma_string(a.sym.as_str()),
            Kind::Number | Kind::Char => true,
            Kind::Punct(p) => !matches!(p, P::Hash | P::HashHash),
            _ => false,
        };
        let mut i = 0;
        while i < toks.len() {
            if toks[i].kind == Kind::Ident && self.arg_pragmas.contains(&toks[i].sym) {
                // (Not called here: maybe through what follows.)
                if !toks.get(i + 1).is_some_and(|p| p.is(P::LParen)) {
                    return true;
                }
                let end = macros::closing(toks, i + 1);
                if end == toks.len() || !toks[i + 2..end].iter().all(harmless) {
                    return true;
                }
                i = end;
            }
            i += 1;
        }
        false
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
        match self.resolve_checked(&name, angled, next) {
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
        if first.kind == Kind::Unknown && first.sym == lex::CALLEES {
            // Pragmas' words given to these in text: a pragma if one is a
            // macro.
            if d.toks[1..].iter().any(|c| self.macros.contains_key(&c.sym)) {
                self.inexact(format!("{}: a pragma from a macro in text", self.here()));
            }
            return;
        }
        if first.sym == intern::PRAGMA_OPERATOR {
            // `_Pragma(OPERAND)` in text, its operand made by macros (see
            // `macros::operand_pragmas`), or a pragma run through a macro.
            let pragmas = match &d.toks[..] {
                [_, l, operand @ .., r] if l.is(P::LParen) && r.is(P::RParen) => macros::operand_pragmas(operand, None, &[], self).0,
                _ => Pragmas::Other,
            };
            match pragmas {
                Pragmas::None => {}
                Pragmas::Macros(names) => self.pushed_in_text.extend(names.iter().copied()),
                Pragmas::Other => self.inexact(format!("{}: a pragma from a macro in text", self.here())),
            }
            return;
        }
        if first.sym == intern::INCLUDE_ALIAS {
            self.inexact(format!("{}: #pragma include_alias", self.here()));
            return;
        }
        if first.sym == intern::GCC && d.toks.get(1).is_some_and(|t| t.sym == intern::DEPENDENCY) && self.cfg.family != Family::Msvc {
            self.dependency(&d.toks[2..]);
            return;
        }
        if first.sym == intern::ONCE {
            if let Some(f) = self.stack.last() {
                let (key, real, id, contents) = (self.key(&f.real), f.real.clone(), f.id, (f.len, f.hash));
                self.mark_once(key, real, id, contents);
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
                    self.undefined.insert(name);
                }
            }
            self.changed(name);
        }
    }

    /// `#pragma GCC dependency "file" [text]`: GCC and Clang look the file
    /// up as an `#include`'s (an error when missing) to compare its time
    /// with the current file's. No dependency list names it, but the
    /// compile needs it: found, as `__has_include`'s.
    fn dependency(&mut self, toks: &[Token]) {
        let target = match toks {
            [t, ..] if t.kind == Kind::Str => t.sym.as_str().strip_prefix('"').and_then(|s| s.strip_suffix('"')).map(|s| (s.to_string(), false)),
            toks => header_name(toks),
        };
        let Some((name, angled)) = target else { return };
        match self.resolve_checked(&name, angled, false) {
            Some(f) => {
                if !self.out.found.contains(&f.real) {
                    self.out.found.push(f.real);
                }
            }
            None => self.out.missing.push(name),
        }
    }

    /// Before a macro's definition changes: a macro that text may push or
    /// pop (see `Pragmas::Macros`) then has an unknown definition.
    fn changing(&mut self, name: Sym) {
        if self.pushed_in_text.contains(&name) {
            self.inexact(format!("{}: a macro changes while macros in text may push or pop it: {}", self.here(), name.as_str()));
        }
    }

    /// After a macro's definition changed: the macros whose `_Pragma`
    /// operands were expanded with it are classified again.
    fn changed(&mut self, name: Sym) {
        let Some(users) = self.pragma_users.get(&name).cloned() else { return };
        for u in users {
            if let Some(m) = self.macros.get(&u).cloned() {
                self.classify(&m);
            }
        }
    }

    /// What a macro does through pragmas where text uses it (which isn't
    /// expanded), with the macros defined now.
    fn classify(&mut self, m: &Macro) {
        let (pragmas, deps, arg) = m.pragmas_in(self);
        for d in deps {
            let users = self.pragma_users.entry(d).or_default();
            if !users.contains(&m.name) {
                users.push(m.name);
            }
        }
        match pragmas {
            Pragmas::None => {}
            // Pushing and popping macros never changed in between does
            // nothing.
            Pragmas::Macros(names) => self.pushed_in_text.extend(names.iter().copied()),
            Pragmas::Other => self.inexact(format!("{}: a macro may run a pragma in text: {}", self.here(), m.name.as_str())),
        }
        // Its pragma is its arguments': the macros defined so far that name
        // it are classified again (`#define WRAP(x) DO_PRAGMA(x)` before
        // `DO_PRAGMA`), and text that calls it is seen.
        if arg && self.arg_pragmas.insert(m.name) {
            self.arg_pragma_hashes.push(lex::name_hash(m.name.as_str().as_bytes()));
            let naming: Vec<Arc<Macro>> =
                self.macros.values().filter(|o| o.name != m.name && o.body.iter().any(|t| t.kind == Kind::Ident && t.sym == m.name)).cloned().collect();
            for o in naming {
                self.classify(&o);
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
                        // (Defined again the same: no change.)
                        if self.macros.get(&m.name).is_none_or(|old| **old != **m) {
                            self.changing(m.name);
                            self.macros.insert(m.name, m.clone());
                            self.classify(m);
                            self.changed(m.name);
                        }
                    }
                }
                DirKind::Undef => {
                    if let Some(n) = d.toks.first().and_then(Token::ident)
                        && self.macros.remove(&n).is_some()
                    {
                        self.undefined.insert(n);
                        self.changing(n);
                        self.changed(n);
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
        pragma_users: HashMap::new(),
        arg_pragmas: HashSet::new(),
        arg_pragma_hashes: Vec::new(),
        undefined: HashSet::new(),
        once: HashSet::new(),
        once_ids: HashMap::new(),
        once_contents: HashMap::new(),
        notes: Default::default(),
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
            alt_at: None,
            relative: false,
            line: 0,
            line_set: false,
            id: (0, 0),
            len: 0,
            hash: 0,
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
        let at = |st: &State, path: PathBuf, relative| st.locate(&path).map(|real| Found { path, real, at: None, alt_at: None, relative });
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
    st.include_file(Found { path: main, real, at: None, alt_at: None, relative: false }, false);
    for note in st.notes.take() {
        st.inexact(note);
    }
    // The main file first.
    st.out
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// The lookups `Cache::is_volatile` made on this thread.
        pub(super) static VOLATILE_LOOKUPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn many_volatile_dirs() {
        let cache = Cache::new(Vec::new(), false);
        for i in 0..50_000 {
            cache.add_volatile(&PathBuf::from(format!("/b/try_compile/{i}")));
        }
        // A lookup per ancestor, however many there are.
        VOLATILE_LOOKUPS.with(|n| n.set(0));
        assert!(!cache.is_volatile(Path::new("/src/include/a.h")));
        assert_eq!(VOLATILE_LOOKUPS.with(|n| n.get()), Path::new("/src/include/a.h").ancestors().count() as u64);
        assert!(cache.is_volatile(Path::new("/b/try_compile/4321/x.h")));
        assert!(!cache.is_volatile(Path::new("/b/try_compile/432100/x.h")));
    }

    struct Dir(test_dirs::TestDir);

    impl Dir {
        fn new(name: &str) -> Dir {
            Dir(test_dirs::TestDir::new(&format!("cpp_deps_{name}")))
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
        // An assembler's .include reads what the preprocessor doesn't; in C,
        // it's a designated initializer.
        let asm = d.file("blake3.S", "#include \"x.tlb\"\n.include \"macros.s\"\n");
        let assembler = Config { assembler: true, ..cfg(&d) };
        assert!(!scan(&Cache::new(vec![], false), &assembler, &NoOracle, &asm).exact());
        let c = d.file("init.c", "#include \"x.tlb\"\nstruct o { int include; } x = {\n    .include = 1,\n};\n");
        assert!(scan(&Cache::new(vec![], false), &cfg(&d), &NoOracle, &c).exact());
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
        assert!(!exact("operand2.c", "#define ONCE_STR \"once\"\n#define ONCE _Pragma(ONCE_STR)\nONCE\n"));
        assert!(exact("diagnostic.c", "#define DO_PRAGMA(x) _Pragma(#x)\n#define PUSH _Pragma(\"GCC diagnostic push\")\nDO_PRAGMA(GCC diagnostic pop)\n"));
        // A pragma's words given to what isn't a macro (`if (once)`).
        assert!(exact("if.c", "int f(int once) { if (once) return 1; return g(x, once); }\n#include \"a.h\"\n"));
        // What a macro whose pragma is its argument's is given in text: a
        // macro, or a pragma's words as another argument; or through
        // another macro (defined before it or not).
        let do_pragma = "#define STR(x) #x\n#define XSTR(x) STR(x)\n#define DO_PRAGMA(x) _Pragma(XSTR(x))\n";
        assert!(!exact("arg_macro.c", &format!("{do_pragma}#define ONCE_WORD once\nDO_PRAGMA(ONCE_WORD)\n")));
        assert!(!exact("arg_undef.c", &format!("{do_pragma}#define ONCE_WORD once\nDO_PRAGMA(ONCE_WORD)\n#undef ONCE_WORD\n")));
        assert!(!exact("second_arg.c", "#define STR(x) #x\n#define P2(a, b) _Pragma(STR(b))\nP2(x, once)\n"));
        assert!(!exact("wrap.c", &format!("{do_pragma}#define WRAP(x) DO_PRAGMA(x)\n#define ONCE_WORD once\nWRAP(ONCE_WORD)\n")));
        assert!(!exact("wrap_before.c", &format!("#define WRAP(x) DO_PRAGMA(x)\n{do_pragma}#define ONCE_WORD once\nWRAP(ONCE_WORD)\n")));
        assert!(!exact("alias.c", &format!("{do_pragma}#define DP DO_PRAGMA\n#define ONCE_WORD once\nDP(ONCE_WORD)\n")));
        assert!(!exact("in_body.c", &format!("{do_pragma}#define ONCE_WORD once\n#define ONCE DO_PRAGMA(ONCE_WORD)\n")));
        assert!(exact("arg_words.c", &format!("{do_pragma}DO_PRAGMA(GCC diagnostic ignored \"-Wx\")\nDO_PRAGMA(message(\"hi\" 1))\n#include \"a.h\"\n")));
        // A TODO message with the place: harmless.
        let todo = "#define TODO(m) _Pragma(XSTR(message(__FILE__ \":\" XSTR(__LINE__) \": \" m)))\n";
        assert!(exact("todo.c", &format!("{do_pragma}{todo}TODO(\"later\")\n#include \"a.h\"\n")));
        // The file's name, which may be a pragma's words.
        assert!(!exact("file.c", "#define P _Pragma(__FILE__)\nP\n"));
        // Operands macros make, read with the macros defined: libc++'s,
        // Boost's and GLib's diagnostics and messages are harmless.
        let libcxx = "#define TOSTR2(x) #x\n#define TOSTR(x) TOSTR2(x)\n#define IGNORED(str) _Pragma(TOSTR(clang diagnostic ignored str))\n";
        assert!(exact("libcxx.cc", &format!("{libcxx}IGNORED(\"-Wfoo\")\n#include \"a.h\"\n")));
        let boost = "#define BOOST_DO_STRINGIZE(X) #X\n#define BOOST_STRINGIZE(X) BOOST_DO_STRINGIZE(X)\n#define BOOST_PRAGMA_MESSAGE(x) _Pragma(BOOST_STRINGIZE(message(x)))\n";
        assert!(exact("boost.cc", &format!("{boost}BOOST_PRAGMA_MESSAGE(\"hi\")\n")));
        let glib = "#define G_STRINGIFY(m) G_STRINGIFY_ARG (m)\n#define G_STRINGIFY_ARG(c) #c\n#define _GLIB_GNUC_DO_PRAGMA(x) _Pragma(G_STRINGIFY (x))\n";
        assert!(exact("glib.c", &format!("{glib}#define DEPRECATED _GLIB_GNUC_DO_PRAGMA(GCC warning \"deprecated\")\n")));
        assert!(!exact("glib2.c", &format!("{glib}#define ONCE _GLIB_GNUC_DO_PRAGMA(once)\n")));
        assert!(exact("text.c", "#define S \"GCC diagnostic push\"\n_Pragma(S)\n"));
        // A macro an operand used changes: what the pragma does too.
        assert!(!exact("changed.c", "#define S \"clang diagnostic push\"\n#define P _Pragma(S)\n#undef S\n#define S \"once\"\nP\n"));
        assert!(exact("same.c", "#define S \"clang diagnostic push\"\n#define P _Pragma(S)\n#define S \"clang diagnostic push\"\nP\n"));
        // One defined later (libc++'s _LIBCPP_VERSION, in the ABI tag of a
        // `clang attribute` pragma): what it does then.
        assert!(exact("later.c", "#define P _Pragma(S)\n#define S \"clang diagnostic push\"\nP\n"));
        assert!(!exact("later2.c", "#define P _Pragma(S)\n#define S \"once\"\nP\n"));
        assert!(exact(
            "version.cc",
            &format!("{libcxx}#define SIG TOSTR(V)\n#define END _Pragma(TOSTR(clang attribute X.push(__abi_tag__(SIG))))\n#define V 230100\nEND\n")
        ));
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

    /// Waits for the file system's clock to move past `change` (a file's
    /// change time): changes within one of its ticks get the same one.
    fn tick_past(dir: &Path, change: u64) {
        let probe = dir.join("tick");
        loop {
            std::fs::write(&probe, "").unwrap();
            if ninja_os::stat(&probe).unwrap().1.change > change {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// A file replaced by one of the same size with its modification time
    /// restored (`cp -p`, an archive extracted over it), in place or created
    /// again (NTFS gives it the old one's creation time back): lexed again.
    #[test]
    fn same_size_and_time() {
        let d = Dir::new("stamp");
        let p = d.file("x.h", "#include \"a.h\"\n");
        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        let cache = Cache::new(vec![], cfg!(windows));
        let mut last = cache.directives(&p).unwrap();
        for (recreate, text) in [(false, "#include \"b.h\"\n"), (true, "#include \"c.h\"\n")] {
            tick_past(&d.0, ninja_os::stat(&p).unwrap().1.change);
            if recreate {
                std::fs::remove_file(&p).unwrap();
            }
            d.file("x.h", text);
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(mtime).unwrap();
            cache.new_session();
            let now = cache.directives(&p).unwrap();
            assert!(!Arc::ptr_eq(&now, &last), "recreated: {recreate}");
            last = now;
        }
    }

    /// A link to a directory: a symbolic link on Unix, a junction on
    /// Windows (which needs no privilege). False when it can't be made.
    fn link_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, link).is_ok();
        #[cfg(windows)]
        return std::process::Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(normalize(link))
            .arg(normalize(target))
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        #[cfg(not(any(unix, windows)))]
        false
    }

    /// A `..` after a link to a directory elsewhere: compilers go to the
    /// target's parent, remote workers (plain directories) to the link's:
    /// inexact. But on Windows, which applies `..` first, as they do.
    #[test]
    fn dotdot_after_links() {
        let d = Dir::new("dotdot");
        let main = d.file("main.c", "#include \"lnk/a.h\"\n");
        d.file("real/sub/a.h", "#include \"../b.h\"\n");
        d.file("b.h", "");
        d.file("real/b.h", "");
        assert!(link_dir(&d.0.join("real/sub"), &d.0.join("lnk")), "can't link a directory");
        let out = scan(&Cache::new(vec![], cfg!(windows)), &cfg(&d), &NoOracle, &main);
        let argv: Vec<String> = ["cc", "-Ilnk/../inc", "-c", "main.c"].iter().map(|a| a.to_string()).collect();
        if cfg!(windows) {
            assert!(out.exact(), "{:?}", out.inexact);
            assert_eq!(d.rel(&out.files), vec!["main.c", "lnk/a.h", "b.h"]);
            assert!(crate::driver::parse(&argv, &d.0).is_ok());
            return;
        }
        assert!(out.inexact.iter().any(|w| w.contains("after a symbolic link")), "{:?}", out.inexact);
        // Without a link, `..` is the same everywhere.
        let plain = d.file("plain.c", "#include \"real/sub/a.h\"\n");
        let out = scan(&Cache::new(vec![], cfg!(windows)), &cfg(&d), &NoOracle, &plain);
        assert!(out.exact(), "{:?}", out.inexact);
        assert_eq!(d.rel(&out.files), vec!["plain.c", "real/sub/a.h", "real/b.h"]);
        // The command line's: falls back.
        assert!(crate::driver::parse(&argv, &d.0).unwrap_err().contains("symbolic link"));
    }

    /// Files with the contents of one with `#pragma once` are told by the
    /// hash of their contents taken when they were lexed: not read again
    /// by each scan (the session's view holds, as for their directives).
    #[test]
    fn once_contents_by_hash() {
        let d = Dir::new("once_hash");
        let once = "#pragma once\n#ifdef SEEN\n#include \"second.h\"\n#endif\n#define SEEN\n";
        d.file("a.h", once);
        d.file("copy.h", once);
        d.file("second.h", "");
        let main = d.file("main.c", "#include \"a.h\"\n#include \"copy.h\"\n");
        let cache = Cache::new(vec![], false);
        assert!(!scan(&cache, &cfg(&d), &NoOracle, &main).exact());
        d.file("copy.h", &once.replace("SEEN", "XXXX"));
        assert!(!scan(&cache, &cfg(&d), &NoOracle, &main).exact());
    }

    /// `#pragma GCC dependency`: GCC and Clang look the file up (an error
    /// when it's missing), which no dependency list names.
    #[test]
    fn pragma_dependency() {
        let d = Dir::new("dependency");
        d.file("inc/dep.txt", "");
        d.file("inc/dep2.txt", "");
        let main =
            d.file("main.c", "#pragma GCC dependency \"dep.txt\" newer than this\n#pragma GCC dependency <dep2.txt>\n#pragma GCC dependency \"none.txt\"\n");
        let out = scan(&Cache::new(vec![], false), &cfg(&d), &NoOracle, &main);
        assert_eq!(d.rel(&out.found), vec!["inc/dep.txt", "inc/dep2.txt"]);
        assert_eq!(out.missing, vec!["none.txt"]);
        // Not cl's.
        let mut c = cfg(&d);
        c.family = Family::Msvc;
        let out = scan(&Cache::new(vec![], false), &c, &NoOracle, &main);
        assert!(out.found.is_empty() && out.missing.is_empty(), "{:?} {:?}", out.found, out.missing);
    }

    /// `#pragma once` is about the file, not its path: the same file under
    /// another name (a link) is skipped by the compiler here, maybe not
    /// remotely, as an identical copy is by GCC: inexact, unless including
    /// it again changes nothing.
    #[test]
    fn pragma_once_by_file() {
        let d = Dir::new("once_file");
        let once = "#pragma once\n#ifdef SEEN\n#include \"second.h\"\n#endif\n#define SEEN\n";
        d.file("a.h", once);
        d.file("second.h", "");
        std::fs::hard_link(d.0.join("a.h"), d.0.join("lnk.h")).unwrap();
        let scanned = |name: &str, text: &str| scan(&Cache::new(vec![], cfg!(windows)), &cfg(&d), &NoOracle, &d.file(name, text));
        let linked = scanned("linked.c", "#include \"a.h\"\n#include \"lnk.h\"\n");
        assert!(!linked.exact());
        assert_eq!(d.rel(&linked.files), vec!["linked.c", "a.h", "lnk.h"]);
        d.file("copy.h", once);
        assert!(!scanned("copied.c", "#include \"a.h\"\n#include \"copy.h\"\n").exact());
        // Another file, or one whose second inclusion does nothing.
        d.file("other.h", "#pragma once\n#define OTHER\n");
        assert!(scanned("other.c", "#include \"a.h\"\n#include \"other.h\"\n").exact());
        d.file("stub.h", "#pragma once\n");
        d.file("stub2.h", "#pragma once\n");
        assert!(scanned("stubs.c", "#include \"stub.h\"\n#include \"stub2.h\"\n").exact());
        d.file("guarded.h", "#pragma once\n#ifndef G\n#define G\n#endif\n");
        d.file("guarded2.h", "#pragma once\n#ifndef G\n#define G\n#endif\n");
        let out = scanned("guarded.c", "#include \"guarded.h\"\n#include \"guarded2.h\"\n");
        assert!(out.exact(), "{:?}", out.inexact);
    }

    /// Clang: a header found next to an outer includer (MSVC's lookup)
    /// takes the place of the first directory of the chain with its name
    /// with -Wmicrosoft-include (on by default), the current file's
    /// without: an `#include_next` from it is inexact when they differ.
    #[test]
    fn clang_msvc_lookup_places() {
        let d = Dir::new("msvc_places");
        let main = d.file("src/main.cc", "#include <sub/a.h>\n");
        d.file("other/sub/a.h", "#include \"x.h\"\n");
        d.file("src/x.h", "#include_next <x.h>\n");
        for f in ["inc/x.h", "other/x.h", "third/x.h"] {
            d.file(f, "");
        }
        let mut c = cfg(&d);
        c.angled_dirs = vec![d.0.join("inc").into(), d.0.join("other").into(), d.0.join("third").into()];
        c.family = Family::Clang;
        c.msvc_lookup = true;
        // other/x.h with the warning, third/x.h without.
        let out = scan(&Cache::new(vec![], false), &c, &NoOracle, &main);
        assert!(out.inexact.iter().any(|w| w.contains("-Wmicrosoft-include")), "{:?}", out.inexact);
        // Both ways the same: exact.
        std::fs::remove_file(d.0.join("other/x.h")).unwrap();
        let out = scan(&Cache::new(vec![], false), &c, &NoOracle, &main);
        assert!(out.exact(), "{:?}", out.inexact);
        assert_eq!(d.rel(&out.files), vec!["src/main.cc", "other/sub/a.h", "src/x.h", "third/x.h"]);
        // Next to the current file: its place.
        let near = d.file("near/main.cc", "#include <sub/n.h>\n");
        d.file("inc/sub/n.h", "#include \"n2.h\"\n");
        d.file("inc/sub/n2.h", "#include_next <x.h>\n");
        let out = scan(&Cache::new(vec![], false), &c, &NoOracle, &near);
        assert!(out.exact(), "{:?}", out.inexact);
        assert_eq!(d.rel(&out.files), vec!["near/main.cc", "inc/sub/n.h", "inc/sub/n2.h", "third/x.h"]);
    }

    /// A header map's relative destination is another name, searched for in
    /// the directories after it (as Clang does).
    #[test]
    fn header_map_renames() {
        let d = Dir::new("hmap_rename");
        let main = d.file("main.c", "#include \"Old.h\"\n");
        d.file("inc/new/name.h", "");
        d.file("new/name.h", "");
        let hmap = d.0.join("x.hmap");
        std::fs::write(&hmap, crate::hmap::build(&[("Old.h", "new/", "name.h")])).unwrap();
        let mut c = cfg(&d);
        c.quote_dirs = vec![SearchDir::HeaderMap(Arc::new(HeaderMap::load(&hmap, &d.0).unwrap()))];
        let out = scan(&Cache::new(vec![], false), &c, &NoOracle, &main);
        assert_eq!(d.rel(&out.files), vec!["main.c", "inc/new/name.h"]);
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
