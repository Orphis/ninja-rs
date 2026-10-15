//! Caches that outlive a single action: file digests, include scanning
//! results, toolchain input trees, the members of thin archives and the
//! set of blobs known to be in the remote CAS. (The daemon keeps its own
//! between builds: see `ninja_daemon`.)
//!
//! Everything lives behind this one `Store`, owned by the service; nothing
//! is global. The file digest cache can be saved to and loaded from disk
//! (keyed by path, size and mtime); the other caches are in memory for now
//! but keyed so that they could be persisted the same way.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Whether a file is a daemon's sync file (`ninja_daemon::watch::COOKIE_PREFIX`),
/// which a walk skips: it is there for a moment, during a sync.
fn is_cookie(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with(ninja_daemon::watch::COOKIE_PREFIX)
}

/// Lists the files under `dir` (following symlinks), relative to it.
pub fn walk_files(dir: &Path, rel: &str, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().filter(|e| !is_cookie(&e.file_name())).collect();
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

type TreeCell = Arc<Mutex<Option<Arc<ToolchainTree>>>>;

/// The members of a file if it's a thin archive, for its size and mtime.
type ThinEntry = (u64, SystemTime, Option<Arc<Vec<String>>>);

#[derive(Default)]
pub struct Store {
    pub digests: MemDigestCache,
    pub scan: ScanCache,
    /// One cell per directory, so concurrent actions build each tree once.
    toolchain_trees: Mutex<HashMap<PathBuf, TreeCell>>,
    thin_archives: Mutex<HashMap<PathBuf, ThinEntry>>,
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
    /// daemon couldn't list them). A failure isn't kept: the next action
    /// tries again.
    pub fn toolchain_tree(&self, dir: &Path, daemon: &dyn Fn(&Path) -> Option<Result<TreeInfo, String>>) -> Result<Arc<ToolchainTree>, String> {
        let cell = self.toolchain_trees.lock().unwrap().entry(dir.to_path_buf()).or_default().clone();
        let mut kept = cell.lock().unwrap();
        if let Some(t) = &*kept {
            return Ok(t.clone());
        }
        let listed = daemon(dir).and_then(Result::ok).and_then(|listed| ToolchainTree::from_listing(dir, listed).ok());
        let t = Arc::new(match listed {
            Some(t) => t,
            None => {
                let mut tree = InputTree::new();
                tree.add_dir_cached("", dir, &|p| p.file_name().is_some_and(is_cookie), Some(&self.digests)).map_err(|e| format!("{e:#}"))?;
                let mut blobs = BlobSet::default();
                let digest = tree.build(&mut blobs);
                let mut files = Vec::new();
                walk_files(dir, "", &mut files);
                ToolchainTree { digest, blobs: Arc::new(blobs), files: Arc::new(files) }
            }
        });
        *kept = Some(t.clone());
        Ok(t)
    }

    /// The members of `path` if it's a thin archive (see `archive`), read
    /// once per version of the file.
    pub fn thin_members(&self, path: &Path) -> Result<Option<Arc<Vec<String>>>, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (len, mtime) = (meta.len(), meta.modified().unwrap_or(UNIX_EPOCH));
        if let Some((l, m, members)) = self.thin_archives.lock().unwrap().get(path)
            && (*l, *m) == (len, mtime)
        {
            return Ok(members.clone());
        }
        let members = crate::archive::read_members(path).map_err(|e| format!("{}: {e}", path.display()))?.map(Arc::new);
        self.thin_archives.lock().unwrap().insert(path.to_path_buf(), (len, mtime, members.clone()));
        Ok(members)
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

    /// Saves the file digest cache.
    pub fn save_digests(&self, file: &Path) -> std::io::Result<()> {
        let mut out = String::from(DIGESTS_SIGNATURE);
        out.push('\n');
        let mut entries = self.digests.entries();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let now = std::time::SystemTime::now();
        for (p, s, d) in entries {
            let ps = p.to_string_lossy();
            // A file modified within the time granularity of now could
            // change again unseen: hashed again next time.
            if ps.contains(['\t', '\n']) || s.is_racy(now) {
                continue;
            }
            out.push_str(&format!("{ps}\t{}\t{}\t{}\t{}\t{}\n", s.len, s.mtime_ns, s.change, s.inode, d.hash));
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
        let dir = std::env::temp_dir().join(format!("ninja_rs_store_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        let stamp = |f: &Path| rbe_client::tree::FileStamp::of(&std::fs::metadata(f).unwrap());
        assert_eq!(s2.digests.get(&f, &stamp(&f)), Some(d.clone()));
        // New contents with the same size and modification time (as an
        // archive extracted in place gives): not the cached digest. (On
        // Windows the creation time stands in, which NTFS gives back to a
        // file created again under the same name within 15 s.)
        #[cfg(unix)]
        {
            std::fs::write(&f, "jello").unwrap();
            std::fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
            assert_ne!(rbe_client::tree::file_digest(&f, Some(&s2.digests)).unwrap(), d);
        }
        // Modified within the time granularity: not saved. A second ago is
        // only too recent for a file system counting in seconds.
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
        let fresh = set("fresh.txt", now);
        let second = set("second.txt", whole(now));
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toolchain_tree_is_built_once() {
        let dir = std::env::temp_dir().join(format!("ninja_rs_tc_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/cc"), "x").unwrap();
        let s = Store::default();
        let a = s.toolchain_tree(&dir, &|_| None).unwrap();
        let b = s.toolchain_tree(&dir, &|_| None).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(*a.files, vec!["bin/cc".to_string()]);
        // The same tree from a listing.
        let info = ninja_daemon::proto::FileInfo { hash: rbe_client::digest_of(b"x").hash, size: 1, executable: false, mtime_ns: 0, change: 0, inode: 0 };
        let listed = TreeInfo { dirs: vec!["bin".into()], files: vec![("bin/cc".into(), info)] };
        let c = Store::default().toolchain_tree(&dir, &|_| Some(Ok(listed.clone()))).unwrap();
        assert_eq!(c.digest, a.digest);
        assert_eq!(c.files, a.files);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tree that couldn't be built isn't kept: the next action tries
    /// again. One the daemon couldn't list is read here.
    #[test]
    fn toolchain_tree_errors_are_not_kept() {
        let dir = std::env::temp_dir().join(format!("ninja_rs_tc_err_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::default();
        assert!(s.toolchain_tree(&dir, &|_| None).is_err());
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/cc"), "x").unwrap();
        let a = s.toolchain_tree(&dir, &|_| None).unwrap();
        assert_eq!(*a.files, vec!["bin/cc".to_string()]);
        let b = Store::default().toolchain_tree(&dir, &|_| Some(Err("stat bin/.ninja-rs-cookie-1-2: not found".into()))).unwrap();
        assert_eq!(b.digest, a.digest);
        // A daemon syncing the directory meanwhile: its cookie isn't part of it.
        std::fs::write(dir.join(format!("{}1-2", ninja_daemon::watch::COOKIE_PREFIX)), "").unwrap();
        let c = Store::default().toolchain_tree(&dir, &|_| None).unwrap();
        assert_eq!((c.digest.clone(), c.files.clone()), (a.digest.clone(), a.files.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
