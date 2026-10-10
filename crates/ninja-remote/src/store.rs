//! Caches that outlive a single action (and, in a future daemon, a single
//! build): file digests, include scanning results, toolchain input trees,
//! the members of thin archives and the set of blobs known to be in the
//! remote CAS.
//!
//! Everything lives behind this one `Store`, owned by the service; nothing
//! is global. The file digest cache can be saved to and loaded from disk
//! (keyed by path, size and mtime); the other caches are in memory for now
//! but keyed so that they could be persisted the same way.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rbe_client::proto as re;
use rbe_client::tree::MemDigestCache;
use rbe_client::{BlobSet, DigestCache, InputTree};

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
}

impl Store {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The Merkle tree of a toolchain input directory (cached; concurrent
    /// callers for the same directory wait for the first one).
    pub fn toolchain_tree(&self, dir: &Path) -> Result<Arc<ToolchainTree>, String> {
        let cell = self.toolchain_trees.lock().unwrap().entry(dir.to_path_buf()).or_default().clone();
        cell.get_or_init(|| {
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
        let Ok(text) = std::fs::read_to_string(file) else { return };
        let mut lines = text.lines();
        if lines.next() != Some(DIGESTS_SIGNATURE) {
            return;
        }
        for line in lines {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 4 {
                continue;
            }
            let (Ok(size), Ok(ns)) = (f[1].parse::<u64>(), f[2].parse::<u128>()) else { continue };
            let mtime = UNIX_EPOCH + Duration::from_nanos(ns as u64);
            self.digests.put(Path::new(f[0]), size, mtime, re::Digest { hash: f[3].to_string(), size_bytes: size as i64 });
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

const DIGESTS_SIGNATURE: &str = "# ninja-rs digests v1";

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
        let a = s.toolchain_tree(&dir).unwrap();
        let b = s.toolchain_tree(&dir).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(*a.files, vec!["bin/cc".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
