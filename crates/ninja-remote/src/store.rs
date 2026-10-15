//! Caches that outlive a single action: file digests, include scanning
//! results, toolchain input trees, the members of thin archives, the
//! linker directives of objects and libraries and the set of blobs known
//! to be in the remote CAS. (The daemon keeps its own
//! between builds: see `ninja_daemon`.)
//!
//! Everything lives behind this one `Store`, owned by the service; nothing
//! is global. The file digest cache can be saved to and loaded from disk
//! (keyed by path and `FileStamp`); the other caches are in memory for now
//! but keyed so that they could be persisted the same way.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rbe_client::proto as re;
use rbe_client::tree::MemDigestCache;
use rbe_client::{BlobSet, DigestCache, InputTree};

use ninja_daemon::files::{DIGESTS_SIGNATURE, read_digests};
use ninja_daemon::proto::TreeInfo;

use crate::scanner::ScanCache;

/// A toolchain input directory as a Merkle subtree, built once.
pub struct ToolchainTree {
    pub digest: re::Digest,
    pub blobs: Arc<BlobSet>,
    /// Files in the directory, '/'-separated and relative to it.
    pub files: Arc<Vec<String>>,
}

/// Lists the files under `dir` (following symlinks), relative to it. Walks
/// skip what the daemon's do (`ninja_daemon::watch::walk_skips`): its sync
/// files, there for a moment, and repositories' own directories, whose
/// changes it doesn't follow.
pub fn walk_files(dir: &Path, rel: &str, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().filter(|e| !ninja_daemon::watch::walk_skips(&e.file_name())).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let p = e.path();
        if p.is_dir() {
            walk_files(&p, &child, out);
        } else {
            out.push(child);
        }
    }
}

/// The most files, and bytes, the directories of computed includes send
/// (see `walk`): above, they're narrowed, else the compile runs locally
/// (see `Scanner::computed_files`).
pub const DIR_FILES_MAX: usize = 5000;
pub const DIR_BYTES_MAX: u64 = 64 << 20;

/// How deep `dir_files` goes (links to directories are followed).
pub const DIR_DEPTH_MAX: usize = 16;

/// What a walk of a directory found for computed includes (see `walk`).
#[derive(Debug, Default)]
pub struct Walk {
    /// The files, with their sizes.
    pub files: Vec<(PathBuf, u64)>,
    /// The directories listed, how deep under each the walk went, and
    /// their stamps, taken before (None: not a directory).
    pub dirs: Vec<(PathBuf, usize, Option<ninja_os::FileStamp>)>,
    /// Other build directories, listed but not walked, with their stamps.
    pub builds: Vec<(PathBuf, Option<ninja_os::FileStamp>)>,
    /// Each stamp's time was a clock tick older than when it was taken: a
    /// change since shows in it (see `FileStamp::is_racy`).
    pub trusted: bool,
    /// Why the walk stopped: over `DIR_FILES_MAX` files (or directories)
    /// or `DIR_BYTES_MAX` bytes.
    pub over: Option<String>,
}

/// The files under `dir`, to `depth` levels below it, for computed
/// includes the scanner can't name: their subdirectories' too (Boost.MPL's
/// reach `preprocessed/plain/`), but not another build directory's (a
/// configuration's beside it, the build directory in a source root) nor
/// repositories' own (see `walk_skips`); of the directories `written`
/// says the build writes to, the header-like files (generated headers, not
/// objects or logs). Links are followed.
pub fn walk(dir: &Path, depth: usize, written: &dyn Fn(&Path) -> bool) -> Walk {
    let mut w = Walk { trusted: true, ..Default::default() };
    let (mut bytes, mut todo, mut walked, mut files) = (0u64, vec![(dir.to_path_buf(), depth)], HashSet::new(), HashSet::new());
    let mb = DIR_BYTES_MAX >> 20;
    while let Some((d, depth)) = todo.pop() {
        // (A directory under another one walked once; as many directories
        // as files at most: links may loop.)
        if !walked.insert((d.clone(), depth)) {
            continue;
        }
        if walked.len() > DIR_FILES_MAX {
            w.over = Some(format!("computed #include with over {DIR_FILES_MAX} directories to send: {}...", d.display()));
            break;
        }
        let at = std::time::SystemTime::now();
        let stamp = ninja_os::stat(&d).ok().filter(|(m, _)| m.is_dir()).map(|(_, s)| s);
        w.trusted &= stamp.is_none_or(|s| !s.is_racy(at));
        let Ok(rd) = std::fs::read_dir(&d) else {
            w.dirs.push((d, depth, stamp));
            continue;
        };
        let entries: Vec<_> = rd.flatten().filter(|e| !ninja_daemon::watch::walk_skips(&e.file_name())).collect();
        // Another build directory, CMake's or one ninja ran in.
        if d != dir && entries.iter().any(|e| ["CMakeCache.txt", ".ninja_log"].iter().any(|b| e.file_name().eq_ignore_ascii_case(b))) {
            w.builds.push((d, stamp));
            continue;
        }
        w.dirs.push((d.clone(), depth, stamp));
        let headers_only = written(&d);
        for e in entries {
            let p = e.path();
            // (Links followed.)
            let meta = match e.file_type() {
                Ok(t) if !t.is_symlink() => e.metadata(),
                _ => std::fs::metadata(&p),
            };
            let Ok(meta) = meta else { continue };
            if meta.is_dir() {
                if depth > 0 {
                    todo.push((p, depth - 1));
                }
                continue;
            }
            if !meta.is_file() || (headers_only && !crate::scanner::header_like(&e.file_name().to_string_lossy())) || !files.insert(p.clone()) {
                continue;
            }
            bytes += meta.len();
            w.files.push((p, meta.len()));
            if w.files.len() > DIR_FILES_MAX || bytes > DIR_BYTES_MAX {
                w.over = Some(format!("computed #include with over {DIR_FILES_MAX} files or {mb} MB to send: {}...", d.display()));
                return w;
            }
        }
    }
    w
}

/// The files under `dirs` (see `walk`). Err above `DIR_FILES_MAX` files
/// or `DIR_BYTES_MAX` bytes.
pub fn dir_files<'a>(dirs: impl IntoIterator<Item = &'a PathBuf>, written: &dyn Fn(&Path) -> bool) -> Result<Vec<PathBuf>, String> {
    files_under(dirs.into_iter().map(|d| (d.clone(), DIR_DEPTH_MAX)), written)
}

/// `dir_files`, each directory with how deep under it to go (0: its own
/// files only).
pub fn files_under(dirs: impl IntoIterator<Item = (PathBuf, usize)>, written: &dyn Fn(&Path) -> bool) -> Result<Vec<PathBuf>, String> {
    let mut files = std::collections::BTreeMap::new();
    for (dir, depth) in dirs {
        let w = walk(&dir, depth, written);
        if let Some(why) = w.over {
            return Err(why);
        }
        files.extend(w.files);
        if files.len() > DIR_FILES_MAX || files.values().sum::<u64>() > DIR_BYTES_MAX {
            return Err(format!("computed #include with over {DIR_FILES_MAX} files or {} MB to send: {}...", DIR_BYTES_MAX >> 20, dir.display()));
        }
    }
    Ok(files.into_keys().collect())
}

impl ToolchainTree {
    /// The tree of `dir` from its listing, as `InputTree::add_dir` builds
    /// it.
    fn from_listing(dir: &Path, listed: TreeInfo) -> Result<ToolchainTree, String> {
        let mut tree = InputTree::new();
        for d in &listed.dirs {
            tree.ensure_dir(d).map_err(|e| format!("{e:#}"))?;
        }
        for (f, info) in &listed.files {
            let digest = re::Digest { hash: info.hash.clone(), size_bytes: info.size as i64 };
            tree.add_file_with_digest(f, &dir.join(f), digest, info.executable).map_err(|e| format!("{e:#}"))?;
        }
        let mut blobs = BlobSet::default();
        let digest = tree.build(&mut blobs);
        let files = listed.files.into_iter().map(|(f, _)| f).collect();
        Ok(ToolchainTree { digest, blobs: Arc::new(blobs), files: Arc::new(files) })
    }
}

/// A toolchain tree, or why it couldn't be built and when, unless that
/// lasts (see `lasting`).
type TreeCell = Arc<Mutex<Option<Result<Arc<ToolchainTree>, (String, Option<std::time::Instant>)>>>>;

/// How long a toolchain tree's failure that doesn't last is kept: the
/// actions meanwhile don't each walk and hash the directory again to fail
/// the same way (a file held without read sharing for a while, by an
/// indexer or a virus scanner), and one after tries again.
const TRANSIENT_KEPT: std::time::Duration = std::time::Duration::from_secs(5);

/// What is known of files, for a version of each (its stamp).
type Stamped<T> = Mutex<HashMap<PathBuf, (ninja_os::FileStamp, T)>>;

/// Whether a toolchain tree's failure lasts: walking the directory again
/// would fail the same way (a dangling link, a file gone or unreadable, a
/// tree that can't be built), unlike when out of file descriptors, or with
/// a file locked for a moment (an installer, a virus scanner), which the
/// next action tries again, as the daemon does.
fn lasting(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::*;
    match e.chain().find_map(|c| c.downcast_ref::<std::io::Error>()) {
        Some(io) => matches!(io.kind(), NotFound | PermissionDenied | NotADirectory | IsADirectory | InvalidInput),
        None => true,
    }
}

#[derive(Default)]
pub struct Store {
    pub digests: MemDigestCache,
    pub scan: ScanCache,
    /// One cell per directory, so concurrent actions build each tree once.
    toolchain_trees: Mutex<HashMap<PathBuf, TreeCell>>,
    /// The members of files if they're thin archives.
    thin_archives: Stamped<Option<Arc<Vec<String>>>>,
    /// The linker directives of objects and libraries (see `directives`).
    directives: Stamped<Option<Arc<Vec<String>>>>,
    /// Hashes of blobs uploaded to (or found in) the CAS during this session.
    uploaded: Mutex<HashSet<String>>,
    /// The preprocessor scanning compiles (`--remote_include_scanner`):
    /// lexed headers, directory listings and what compilers answered.
    preprocessor: Mutex<Option<Arc<cpp_deps::Driver>>>,
}

impl Store {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The Merkle tree of a toolchain input directory (cached; concurrent
    /// callers for the same directory wait for the first one): from the
    /// files `daemon` lists (None: no daemon), or read here (also when the
    /// daemon couldn't list them). A failure is kept too, a lasting one (see
    /// `lasting`) for good, another for a while (`TRANSIENT_KEPT`): actions
    /// don't each walk the directory again.
    pub fn toolchain_tree(&self, dir: &Path, daemon: &dyn Fn(&Path) -> Option<Result<TreeInfo, String>>) -> Result<Arc<ToolchainTree>, String> {
        self.toolchain_tree_keeping(dir, daemon, TRANSIENT_KEPT)
    }

    /// `toolchain_tree`, a failure that doesn't last kept for `transient`.
    fn toolchain_tree_keeping(
        &self,
        dir: &Path,
        daemon: &dyn Fn(&Path) -> Option<Result<TreeInfo, String>>,
        transient: std::time::Duration,
    ) -> Result<Arc<ToolchainTree>, String> {
        let cell = self.toolchain_trees.lock().unwrap().entry(dir.to_path_buf()).or_default().clone();
        let mut kept = cell.lock().unwrap();
        match &*kept {
            Some(Ok(t)) => return Ok(t.clone()),
            Some(Err((why, failed))) if failed.is_none_or(|at| at.elapsed() < transient) => return Err(why.clone()),
            _ => {}
        }
        let listed = daemon(dir).and_then(Result::ok).and_then(|listed| ToolchainTree::from_listing(dir, listed).ok());
        if let Some(t) = listed {
            let t = Arc::new(t);
            *kept = Some(Ok(t.clone()));
            return Ok(t);
        }
        let mut tree = InputTree::new();
        match tree.add_dir_cached("", dir, &|p| p.file_name().is_some_and(ninja_daemon::watch::walk_skips), Some(&self.digests)) {
            Ok(()) => {
                let mut blobs = BlobSet::default();
                let digest = tree.build(&mut blobs);
                let mut files = Vec::new();
                walk_files(dir, "", &mut files);
                let t = Arc::new(ToolchainTree { digest, blobs: Arc::new(blobs), files: Arc::new(files) });
                *kept = Some(Ok(t.clone()));
                Ok(t)
            }
            Err(e) => {
                let why = format!("{e:#}");
                let failed = (!lasting(&e)).then(std::time::Instant::now);
                *kept = Some(Err((why.clone(), failed)));
                Err(why)
            }
        }
    }

    /// Forgets the trees of the toolchain directories holding `path`, found
    /// changed: they're built again.
    pub fn forget_toolchain_trees_holding(&self, path: &Path) {
        self.toolchain_trees.lock().unwrap().retain(|dir, _| !path.starts_with(dir));
    }

    /// The members of `path` if it's a thin archive (see `archive`), read
    /// once per version of the file (its `FileStamp`).
    pub fn thin_members(&self, path: &Path) -> Result<Option<Arc<Vec<String>>>, String> {
        let (_, stamp) = ninja_os::stat(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some((s, members)) = self.thin_archives.lock().unwrap().get(path)
            && *s == stamp
        {
            return Ok(members.clone());
        }
        let members = crate::archive::read_members(path).map_err(|e| format!("{}: {e}", path.display()))?.map(Arc::new);
        self.thin_archives.lock().unwrap().insert(path.to_path_buf(), (stamp, members.clone()));
        Ok(members)
    }

    /// The linker directives of an object or a library (see `directives`),
    /// read once per version of the file. None when they can't be read.
    pub fn link_directives(&self, path: &Path) -> Option<Arc<Vec<String>>> {
        let (_, stamp) = ninja_os::stat(path).ok()?;
        if let Some((s, options)) = self.directives.lock().unwrap().get(path)
            && *s == stamp
        {
            return options.clone();
        }
        let options = crate::directives::read(path).ok().flatten().map(Arc::new);
        self.directives.lock().unwrap().insert(path.to_path_buf(), (stamp, options.clone()));
        options
    }

    /// The preprocessor, created on first use; `build_dir`'s contents may
    /// change during a build (generated headers), so they aren't cached.
    pub fn preprocessor(&self, build_dir: &Path, case_insensitive: bool) -> Arc<cpp_deps::Driver> {
        self.preprocessor
            .lock()
            .unwrap()
            .get_or_insert_with(|| Arc::new(cpp_deps::Driver::new(cpp_deps::Cache::new(vec![build_dir.to_path_buf()], case_insensitive))))
            .clone()
    }

    /// A new build: the preprocessor checks headers and directories again.
    pub fn new_build(&self) {
        if let Some(p) = self.preprocessor.lock().unwrap().as_ref() {
            p.cache.new_session();
        }
    }

    /// Keeps only the blobs not yet known to be in the CAS.
    pub fn filter_unknown(&self, blobs: BlobSet) -> BlobSet {
        let known = self.uploaded.lock().unwrap();
        BlobSet(blobs.0.into_iter().filter(|(h, _)| !known.contains(h)).collect())
    }

    /// Whether a blob is known to be in the CAS.
    pub fn is_uploaded(&self, hash: &str) -> bool {
        self.uploaded.lock().unwrap().contains(hash)
    }

    pub fn mark_uploaded<'a>(&self, hashes: impl IntoIterator<Item = &'a String>) {
        let mut known = self.uploaded.lock().unwrap();
        for h in hashes {
            known.insert(h.clone());
        }
    }

    /// Forgets that these blobs are in the CAS (it evicted some).
    pub fn forget_uploaded<'a>(&self, hashes: impl IntoIterator<Item = &'a String>) {
        let mut known = self.uploaded.lock().unwrap();
        for h in hashes {
            known.remove(h);
        }
    }

    /// Loads persisted file digests (see `read_digests`). Entries are only
    /// used when the file's `FileStamp` still matches.
    pub fn load_digests(&self, file: &Path) {
        for (p, stamp, hash) in read_digests(file) {
            self.digests.put(&p, stamp, re::Digest { hash, size_bytes: stamp.len as i64 });
        }
    }

    /// Saves the file digest cache: but the digests of files that could
    /// still change unseen when hashed (see `MemDigestCache::entries`).
    pub fn save_digests(&self, file: &Path) -> std::io::Result<()> {
        let mut out = String::from(DIGESTS_SIGNATURE);
        out.push('\n');
        let mut entries = self.digests.entries();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (p, s, d) in entries {
            let ps = p.to_string_lossy();
            if ps.contains(['\t', '\n']) {
                continue;
            }
            out.push_str(&format!("{ps}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n", s.len, s.mtime_ns, s.change, s.created, s.volume, s.id, d.hash));
        }
        let tmp = file.with_extension("tmp");
        std::fs::File::create(&tmp)?.write_all(out.as_bytes())?;
        std::fs::rename(&tmp, file)
    }
}

/// Name of the persisted digest cache in the build directory.
pub const DIGESTS_FILE: &str = ".ninja_rs_digests";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_round_trip() {
        let dir = test_dirs::TestDir::new("ninja_rs_store");
        let f = dir.join("a.txt");
        std::fs::write(&f, "hello").unwrap();
        // Modified a while ago: a recent file's digest isn't saved.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
        let s = Store::default();
        let d = rbe_client::tree::file_digest(&f, Some(&s.digests)).unwrap();
        s.save_digests(&dir.join("digests")).unwrap();
        let s2 = Store::default();
        s2.load_digests(&dir.join("digests"));
        let stamp = |f: &Path| ninja_os::stat(f).unwrap().1;
        assert_eq!(s2.digests.get(&f, &stamp(&f)), Some(d.clone()));
        // Changes within a tick of the file system's clock get the same
        // change time: one goes by.
        let tick = || {
            let (probe, last) = (dir.join("tick"), stamp(&f).change);
            loop {
                std::fs::write(&probe, "").unwrap();
                if stamp(&probe).change > last {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        // New contents with the same size and modification time (as an
        // archive extracted in place gives): not the cached digest. Nor for
        // a file created again under its name, which NTFS gives its old
        // creation time back.
        tick();
        std::fs::write(&f, "jello").unwrap();
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
        assert_ne!(rbe_client::tree::file_digest(&f, Some(&s2.digests)).unwrap(), d);
        tick();
        std::fs::remove_file(&f).unwrap();
        std::fs::write(&f, "hello").unwrap();
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
        assert_eq!(rbe_client::tree::file_digest(&f, Some(&s2.digests)).unwrap(), d);
        // Modified within the time granularity of when they were hashed
        // (later, whatever the time granularity): not saved. A second ago
        // with a finer time is old enough.
        let now = std::time::SystemTime::now();
        let whole = |t: std::time::SystemTime| {
            let s = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(s)
        };
        let set = |name: &str, t: std::time::SystemTime| {
            let g = dir.join(name);
            std::fs::write(&g, name).unwrap();
            std::fs::File::options().write(true).open(&g).unwrap().set_modified(t).unwrap();
            rbe_client::tree::file_digest(&g, Some(&s2.digests)).unwrap();
            g
        };
        let hour = std::time::Duration::from_secs(3600);
        let fresh = set("fresh.txt", now + hour + std::time::Duration::from_millis(1));
        let second = set("second.txt", whole(now + hour));
        let fine = set("fine.txt", whole(now) - std::time::Duration::from_secs(1) + std::time::Duration::from_millis(1));
        s2.save_digests(&dir.join("digests")).unwrap();
        let s3 = Store::default();
        s3.load_digests(&dir.join("digests"));
        assert!(s3.digests.get(&fresh, &stamp(&fresh)).is_none());
        assert!(s3.digests.get(&second, &stamp(&second)).is_none());
        assert!(s3.digests.get(&f, &stamp(&f)).is_some());
        // Unless the file system keeps whole seconds only (FAT, ext3).
        if !stamp(&fine).mtime_ns.is_multiple_of(1_000_000_000) {
            assert!(s3.digests.get(&fine, &stamp(&fine)).is_some());
        }
    }

    #[test]
    fn toolchain_tree_is_built_once() {
        let dir = test_dirs::TestDir::new("ninja_rs_tc");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/cc"), "x").unwrap();
        let s = Store::default();
        let a = s.toolchain_tree(&dir, &|_| None).unwrap();
        let b = s.toolchain_tree(&dir, &|_| None).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(*a.files, vec!["bin/cc".to_string()]);
        // The same tree from a listing.
        let info = ninja_daemon::proto::FileInfo {
            hash: rbe_client::digest_of(b"x").hash,
            size: 1,
            executable: false,
            mtime_ns: 0,
            change: 0,
            created: 0,
            volume: 0,
            id: 0,
        };
        let listed = TreeInfo { dirs: vec!["bin".into()], files: vec![("bin/cc".into(), info)] };
        let c = Store::default().toolchain_tree(&dir, &|_| Some(Ok(listed.clone()))).unwrap();
        assert_eq!(c.digest, a.digest);
        assert_eq!(c.files, a.files);
        // Built again once a file of it is found changed (by an upload),
        // not before; other trees are kept.
        let other = test_dirs::TestDir::new("ninja_rs_tc_other");
        let o = s.toolchain_tree(&other, &|_| None).unwrap();
        std::fs::write(dir.join("bin/cc"), "yy").unwrap();
        assert!(Arc::ptr_eq(&s.toolchain_tree(&dir, &|_| None).unwrap(), &a));
        s.forget_toolchain_trees_holding(&dir.join("bin").join("cc"));
        assert_ne!(s.toolchain_tree(&dir, &|_| None).unwrap().digest, a.digest);
        assert!(Arc::ptr_eq(&s.toolchain_tree(&other, &|_| None).unwrap(), &o));
    }

    /// A tree that couldn't be built is kept for the invocation: actions
    /// don't each walk it again (its directory missing: until a change).
    /// One the daemon couldn't list is read here.
    #[test]
    fn toolchain_tree_errors() {
        let base = test_dirs::TestDir::new("ninja_rs_tc_err");
        let dir = base.join("tc");
        let s = Store::default();
        assert!(s.toolchain_tree(&dir, &|_| None).is_err());
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/cc"), "x").unwrap();
        assert!(s.toolchain_tree(&dir, &|_| None).is_err());
        let a = Store::default().toolchain_tree(&dir, &|_| None).unwrap();
        assert_eq!(*a.files, vec!["bin/cc".to_string()]);
        let b = Store::default().toolchain_tree(&dir, &|_| Some(Err("stat bin/.ninja-rs-cookie-1-2: not found".into()))).unwrap();
        assert_eq!(b.digest, a.digest);
        // A daemon syncing the directory meanwhile: its cookie isn't part of it.
        std::fs::write(dir.join(format!("{}1-2", ninja_daemon::watch::COOKIE_PREFIX)), "").unwrap();
        let c = Store::default().toolchain_tree(&dir, &|_| None).unwrap();
        assert_eq!((c.digest.clone(), c.files.clone()), (a.digest.clone(), a.files.clone()));
    }

    /// Which failures of a toolchain tree last the invocation: those
    /// walking it again would meet (a file gone, unreadable, a dangling
    /// link), not a file locked for a moment or too many open files.
    #[test]
    fn lasting_tree_failures() {
        let io = |kind| anyhow::Error::from(std::io::Error::from(kind)).context("hashing tc/bin/cc");
        for kind in [std::io::ErrorKind::NotFound, std::io::ErrorKind::PermissionDenied, std::io::ErrorKind::NotADirectory] {
            assert!(lasting(&io(kind)), "{kind:?}");
        }
        for kind in [std::io::ErrorKind::Other, std::io::ErrorKind::Interrupted, std::io::ErrorKind::ResourceBusy] {
            assert!(!lasting(&io(kind)), "{kind:?}");
        }
        assert!(!lasting(&anyhow::Error::from(std::io::Error::from_raw_os_error(if cfg!(windows) { 32 } else { 24 }))));
        assert!(lasting(&anyhow::anyhow!("path conflict at bin")));
    }

    /// A file locked while the tree is built (Windows: opened without
    /// sharing, by an installer or a virus scanner): the failure is kept for
    /// a while, then an action builds the tree again, once it's released.
    #[cfg(windows)]
    #[test]
    fn locked_toolchain_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = test_dirs::TestDir::new("ninja_rs_tc_locked");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/cc"), "x").unwrap();
        let s = Store::default();
        let hour = std::time::Duration::from_secs(3600);
        let lock = std::fs::File::options().read(true).share_mode(0).open(dir.join("bin/cc")).unwrap();
        assert!(s.toolchain_tree_keeping(&dir, &|_| None, hour).is_err());
        drop(lock);
        // Kept: not walked again meanwhile.
        assert!(s.toolchain_tree_keeping(&dir, &|_| None, hour).is_err());
        let tree = s.toolchain_tree_keeping(&dir, &|_| None, std::time::Duration::ZERO).unwrap();
        assert_eq!(*tree.files, vec!["bin/cc".to_string()]);
    }

    /// Thin archives' members and linker directives are read again for a
    /// new version of the file, even with the size and time of the old one.
    #[test]
    fn stamped_caches() {
        let dir = test_dirs::TestDir::new("ninja_rs_stamped");
        let s = Store::default();
        let lib = dir.join("x.lib");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let write = |data: &[u8]| {
            std::fs::write(&lib, data).unwrap();
            std::fs::File::options().write(true).open(&lib).unwrap().set_modified(old).unwrap();
        };
        // Another version gets another change time: one tick of the file
        // system's clock goes by.
        let tick = || {
            let (probe, last) = (dir.join("tick"), ninja_os::stat(&lib).unwrap().1.change);
            while {
                std::fs::write(&probe, "").unwrap();
                ninja_os::stat(&probe).unwrap().1.change <= last
            } {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        let obj = |lib: &[u8]| crate::directives::tests::coff(&[(".drectve", [b"/DEFAULTLIB:".as_slice(), lib].concat().as_slice())]);
        write(&obj(b"aaa"));
        let first = s.link_directives(&lib).unwrap();
        assert_eq!(crate::directives::default_libs(&first).collect::<Vec<_>>(), ["aaa"]);
        // Rewritten in place, and removed and made again (which NTFS gives
        // its old creation time back).
        tick();
        write(&obj(b"bbb"));
        assert_eq!(crate::directives::default_libs(&s.link_directives(&lib).unwrap()).collect::<Vec<_>>(), ["bbb"]);
        tick();
        std::fs::remove_file(&lib).unwrap();
        write(&obj(b"ccc"));
        assert_eq!(crate::directives::default_libs(&s.link_directives(&lib).unwrap()).collect::<Vec<_>>(), ["ccc"]);
        tick();
        write(crate::archive::MAGIC);
        assert_eq!(s.thin_members(&lib).unwrap().as_deref(), Some(&Vec::new()));
        tick();
        write(b"!<arch>\n");
        assert_eq!(s.thin_members(&lib).unwrap(), None);
    }

    /// Repositories' own directories aren't part of toolchain trees (a
    /// sysroot that is a checkout), as the daemon lists them: the same keys
    /// with and without it.
    #[test]
    fn toolchain_trees_skip_repositories() {
        let dir = test_dirs::TestDir::new("ninja_rs_tc_vcs");
        std::fs::create_dir_all(dir.join(".git/refs")).unwrap();
        std::fs::create_dir_all(dir.join("bin/.svn")).unwrap();
        std::fs::write(dir.join("bin/cc"), "x").unwrap();
        std::fs::write(dir.join(".git/refs/main"), "1").unwrap();
        std::fs::write(dir.join("bin/.svn/entries"), "e").unwrap();
        let a = Store::default().toolchain_tree(&dir, &|_| None).unwrap();
        assert_eq!(*a.files, vec!["bin/cc".to_string()]);
        let info = ninja_daemon::proto::FileInfo {
            hash: rbe_client::digest_of(b"x").hash,
            size: 1,
            executable: false,
            mtime_ns: 0,
            change: 0,
            created: 0,
            volume: 0,
            id: 0,
        };
        let listed = TreeInfo { dirs: vec!["bin".into()], files: vec![("bin/cc".into(), info)] };
        assert_eq!(Store::default().toolchain_tree(&dir, &|_| Some(Ok(listed.clone()))).unwrap().digest, a.digest);
        let mut files = Vec::new();
        walk_files(&dir, "", &mut files);
        assert_eq!(files, vec!["bin/cc".to_string()]);
    }
}
