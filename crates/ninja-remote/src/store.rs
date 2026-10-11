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
use std::sync::{Arc, Mutex, OnceLock};
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

/// Lists the files under `dir` (following symlinks), relative to it.
pub fn walk_files(dir: &Path, rel: &str, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
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

type TreeCell = Arc<OnceLock<Result<Arc<ToolchainTree>, String>>>;

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
    /// files `daemon` lists (None: no daemon), or read here.
    pub fn toolchain_tree(&self, dir: &Path, daemon: &dyn Fn(&Path) -> Option<Result<TreeInfo, String>>) -> Result<Arc<ToolchainTree>, String> {
        let cell = self.toolchain_trees.lock().unwrap().entry(dir.to_path_buf()).or_default().clone();
        cell.get_or_init(|| {
            if let Some(listed) = daemon(dir) {
                return ToolchainTree::from_listing(dir, listed?).map(Arc::new);
            }
            let mut tree = InputTree::new();
            tree.add_dir_cached("", dir, &|_| false, Some(&self.digests)).map_err(|e| format!("{e:#}"))?;
            let mut blobs = BlobSet::default();
            let digest = tree.build(&mut blobs);
            let mut files = Vec::new();
            walk_files(dir, "", &mut files);
            Ok(Arc::new(ToolchainTree { digest, blobs: Arc::new(blobs), files: Arc::new(files) }))
        })
        .clone()
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

    /// Loads persisted file digests (`path\tsize\tmtime_ns\thash`). Entries
    /// are only used when size and mtime still match.
    pub fn load_digests(&self, file: &Path) {
        for (p, size, mtime, hash) in read_digests(file) {
            self.digests.put(&p, size, mtime, re::Digest { hash, size_bytes: size as i64 });
        }
    }

    /// Saves the file digest cache.
    pub fn save_digests(&self, file: &Path) -> std::io::Result<()> {
        let mut out = String::from(DIGESTS_SIGNATURE);
        out.push('\n');
        let mut entries = self.digests.entries();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (p, size, mtime, d) in entries {
            let ns = mtime.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let ps = p.to_string_lossy();
            if ps.contains(['\t', '\n']) {
                continue;
            }
            out.push_str(&format!("{ps}\t{size}\t{ns}\t{}\n", d.hash));
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
        let s = Store::default();
        let d = rbe_client::tree::file_digest(&f, Some(&s.digests)).unwrap();
        s.save_digests(&dir.join("digests")).unwrap();
        let s2 = Store::default();
        s2.load_digests(&dir.join("digests"));
        let m = std::fs::metadata(&f).unwrap();
        assert_eq!(s2.digests.get(&f, m.len(), m.modified().unwrap()), Some(d));
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
        let info = ninja_daemon::proto::FileInfo { hash: rbe_client::digest_of(b"x").hash, size: 1, executable: false, mtime_ns: 0 };
        let listed = TreeInfo { dirs: vec!["bin".into()], files: vec![("bin/cc".into(), info)] };
        let c = Store::default().toolchain_tree(&dir, &|_| Some(Ok(listed.clone()))).unwrap();
        assert_eq!(c.digest, a.digest);
        assert_eq!(c.files, a.files);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
