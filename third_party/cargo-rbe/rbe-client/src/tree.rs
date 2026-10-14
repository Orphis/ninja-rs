// Copied from cargo-rbe (crates/rbe-client, commit a22f361); see ../README.md.
//! Building REAPI input roots (Merkle trees of `Directory` messages) from
//! local files, and materializing output trees back to disk.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use prost::Message;
use sha2::{Digest as _, Sha256};

use crate::proto as re;

/// Where the bytes of a blob come from. Files are only read when the server
/// reports the blob as missing.
#[derive(Clone, Debug)]
pub enum BlobSource {
    Bytes(Vec<u8>),
    File(PathBuf),
    /// Only exists in the CAS (an output of an earlier remote action that was
    /// never downloaded). Uploading it again is impossible.
    Remote(PathBuf),
}

impl BlobSource {
    pub fn load(&self) -> Result<Vec<u8>> {
        match self {
            BlobSource::Bytes(b) => Ok(b.clone()),
            BlobSource::File(p) => fs::read(p).with_context(|| format!("reading {}", p.display())),
            BlobSource::Remote(p) => bail!(
                "{} was never downloaded and is no longer in the remote CAS (evicted?); rebuild it",
                p.display()
            ),
        }
    }
}

/// Blobs keyed by hash; what `Client::upload_blobs` consumes.
#[derive(Default, Debug)]
pub struct BlobSet(pub HashMap<String, (re::Digest, BlobSource)>);

impl BlobSet {
    /// Blobs whose bytes only exist in the CAS, with the local path they stand for.
    pub fn remote_only(&self) -> Vec<(re::Digest, PathBuf)> {
        self.0
            .values()
            .filter_map(|(d, s)| match s {
                BlobSource::Remote(p) => Some((d.clone(), p.clone())),
                _ => None,
            })
            .collect()
    }

    pub fn insert(&mut self, d: re::Digest, src: BlobSource) {
        self.0.entry(d.hash.clone()).or_insert((d, src));
    }

    pub fn insert_message<M: Message>(&mut self, m: &M) -> re::Digest {
        let (d, bytes) = crate::digest_of_message(m);
        self.insert(d.clone(), BlobSource::Bytes(bytes));
        d
    }

    pub fn extend(&mut self, other: BlobSet) {
        for (_, (d, s)) in other.0 {
            self.insert(d, s);
        }
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What a file's cached digest is valid for, besides its path: its size,
/// modification time, and on Unix its change time and inode, which no tool
/// sets (files extracted in place from an archive with normalized times
/// can have new contents, the same size and an old modification time; not
/// an old change time). Windows has its creation time instead, new for a
/// file extracted into a new directory, but not for one written in place,
/// or created again under its name within 15 s (NTFS gives it back): its
/// change time needs a handle per file, too costly here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileStamp {
    pub len: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime_ns: u64,
    /// Unix: change time, in nanoseconds; Windows: creation time, in 100 ns.
    pub change: u64,
    pub inode: u64,
}

impl FileStamp {
    pub fn of(meta: &fs::Metadata) -> FileStamp {
        let mtime_ns = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64);
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
        FileStamp { len: meta.len(), mtime_ns, change, inode }
    }

    /// Whether the file was modified within its file system's time
    /// granularity of `now`: changed again since, it could have kept this
    /// stamp, so its digest isn't kept across builds (git's "racy"
    /// entries). Whole seconds are 2 s (FAT counts in 2 s, ext3 and HFS+ in
    /// seconds), finer times 100 ms (kernels stamp files with a clock that
    /// ticks every few milliseconds).
    pub fn is_racy(&self, now: SystemTime) -> bool {
        let now_ns = now.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        let granularity = if self.mtime_ns.is_multiple_of(1_000_000_000) { 2_000_000_000 } else { 100_000_000 };
        self.mtime_ns.saturating_add(granularity) > now_ns
    }
}

/// A cache of file digests keyed by path and `FileStamp`, owned by the
/// caller (no process-wide state).
pub trait DigestCache: Send + Sync {
    fn get(&self, path: &Path, stamp: &FileStamp) -> Option<re::Digest>;
    fn put(&self, path: &Path, stamp: FileStamp, digest: re::Digest);
}

/// A simple in-memory [`DigestCache`].
#[derive(Default)]
pub struct MemDigestCache(Mutex<HashMap<PathBuf, (FileStamp, re::Digest)>>);

impl MemDigestCache {
    /// All entries, for persisting the cache.
    pub fn entries(&self) -> Vec<(PathBuf, FileStamp, re::Digest)> {
        self.0.lock().unwrap().iter().map(|(p, (s, d))| (p.clone(), *s, d.clone())).collect()
    }
}

impl DigestCache for MemDigestCache {
    fn get(&self, path: &Path, stamp: &FileStamp) -> Option<re::Digest> {
        let map = self.0.lock().unwrap();
        let (s, d) = map.get(path)?;
        if s == stamp { Some(d.clone()) } else { None }
    }
    fn put(&self, path: &Path, stamp: FileStamp, digest: re::Digest) {
        self.0.lock().unwrap().insert(path.to_path_buf(), (stamp, digest));
    }
}

/// SHA-256 of a file's contents.
pub fn hash_file(path: &Path) -> Result<re::Digest> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n_total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        n_total += n as u64;
        h.update(&buf[..n]);
    }
    Ok(re::Digest { hash: hex::encode(h.finalize()), size_bytes: n_total as i64 })
}

/// The digest of a file, through `cache` when given.
/// The cache is keyed by the symlink-resolved path, so that many links to
/// one file (e.g. LLVM's multi-call binary) are hashed once.
pub fn file_digest(path: &Path, cache: Option<&dyn DigestCache>) -> Result<re::Digest> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let stamp = FileStamp::of(&meta);
    let Some(c) = cache else { return hash_file(path) };
    let is_link = fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false);
    let key = if is_link { fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()) } else { path.to_path_buf() };
    if let Some(d) = c.get(&key, &stamp) {
        return Ok(d);
    }
    let d = hash_file(path)?;
    c.put(&key, stamp, d.clone());
    Ok(d)
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

#[derive(Debug)]
enum Node {
    File { digest: re::Digest, executable: bool, src: BlobSource },
    Dir(DirNode),
    /// A directory whose `Directory` blobs are already in the CAS.
    Subtree(re::Digest),
}

#[derive(Debug, Default)]
struct DirNode {
    children: BTreeMap<String, Node>,
}

/// An input root under construction. Paths are `/`-separated and relative.
#[derive(Debug, Default)]
pub struct InputTree {
    root: DirNode,
}

fn components(rel: &str) -> Vec<&str> {
    rel.split('/').filter(|c| !c.is_empty() && *c != ".").collect()
}

impl InputTree {
    pub fn new() -> Self {
        Self::default()
    }

    fn dir_mut(&mut self, comps: &[&str]) -> Result<&mut DirNode> {
        let mut cur = &mut self.root;
        for c in comps {
            if *c == ".." {
                bail!("`..` is not allowed in input paths");
            }
            let node = cur.children.entry(c.to_string()).or_insert_with(|| Node::Dir(DirNode::default()));
            cur = match node {
                Node::Dir(d) => d,
                Node::Subtree(_) => bail!("cannot add below grafted subtree `{c}`"),
                Node::File { .. } => bail!("`{c}` is a file, expected a directory"),
            };
        }
        Ok(cur)
    }

    pub fn ensure_dir(&mut self, rel: &str) -> Result<()> {
        self.dir_mut(&components(rel)).map(|_| ())
    }

    fn insert(&mut self, rel: &str, node: Node) -> Result<()> {
        let comps = components(rel);
        let (name, parent) = comps.split_last().context("empty input path")?;
        let dir = self.dir_mut(parent)?;
        match dir.children.get(*name) {
            Some(Node::File { digest, .. }) if matches!(&node, Node::File { digest: d, .. } if d == digest) => Ok(()),
            Some(_) => bail!("input path `{rel}` added twice with different contents"),
            None => {
                dir.children.insert(name.to_string(), node);
                Ok(())
            }
        }
    }

    pub fn add_file(&mut self, rel: &str, local: &Path) -> Result<()> {
        self.add_file_cached(rel, local, None)
    }

    pub fn add_file_cached(&mut self, rel: &str, local: &Path, cache: Option<&dyn DigestCache>) -> Result<()> {
        let meta = fs::metadata(local).with_context(|| format!("stat {}", local.display()))?;
        let digest = file_digest(local, cache)?;
        self.insert(rel, Node::File { digest, executable: is_executable(&meta), src: BlobSource::File(local.to_path_buf()) })
    }

    /// A local file whose digest the caller already knows.
    pub fn add_file_with_digest(&mut self, rel: &str, local: &Path, digest: re::Digest, executable: bool) -> Result<()> {
        self.insert(rel, Node::File { digest, executable, src: BlobSource::File(local.to_path_buf()) })
    }

    /// Whether `rel` is already in the tree (as a file or directory).
    pub fn contains(&self, rel: &str) -> bool {
        let comps = components(rel);
        let mut cur = &self.root;
        for (i, c) in comps.iter().enumerate() {
            match cur.children.get(*c) {
                None => return false,
                Some(Node::Dir(d)) => cur = d,
                Some(_) => return i + 1 == comps.len(),
            }
        }
        true
    }

    /// A file whose bytes only exist in the CAS.
    pub fn add_remote_file(&mut self, rel: &str, digest: re::Digest, executable: bool, local: &Path) -> Result<()> {
        self.insert(rel, Node::File { digest, executable, src: BlobSource::Remote(local.to_path_buf()) })
    }

    pub fn add_bytes(&mut self, rel: &str, bytes: Vec<u8>, executable: bool) -> Result<()> {
        let digest = crate::digest_of(&bytes);
        self.insert(rel, Node::File { digest, executable, src: BlobSource::Bytes(bytes) })
    }

    pub fn add_subtree(&mut self, rel: &str, digest: re::Digest) -> Result<()> {
        self.insert(rel, Node::Subtree(digest))
    }

    /// Recursively adds `local` at `rel`, following symlinks. `skip` gets the
    /// path relative to `local` and can prune files or directories.
    pub fn add_dir(&mut self, rel: &str, local: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<()> {
        self.add_dir_cached(rel, local, skip, None)
    }

    pub fn add_dir_cached(
        &mut self,
        rel: &str,
        local: &Path,
        skip: &dyn Fn(&Path) -> bool,
        cache: Option<&dyn DigestCache>,
    ) -> Result<()> {
        self.ensure_dir(rel)?;
        self.add_dir_inner(rel, local, Path::new(""), skip, cache)
    }

    fn add_dir_inner(
        &mut self,
        rel: &str,
        base: &Path,
        sub: &Path,
        skip: &dyn Fn(&Path) -> bool,
        cache: Option<&dyn DigestCache>,
    ) -> Result<()> {
        let dir = base.join(sub);
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .with_context(|| format!("reading dir {}", dir.display()))?
            .collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let sub_child = sub.join(&name);
            if skip(&sub_child) {
                continue;
            }
            let child_rel = format!("{rel}/{name}");
            let meta = fs::metadata(e.path())?;
            if meta.is_dir() {
                self.ensure_dir(&child_rel)?;
                self.add_dir_inner(&child_rel, base, &sub_child, skip, cache)?;
            } else {
                self.add_file_cached(&child_rel, &e.path(), cache)?;
            }
        }
        Ok(())
    }

    /// Every file path in the tree (grafted subtrees are listed as `dir/`).
    pub fn paths(&self) -> Vec<String> {
        fn walk(d: &DirNode, prefix: &str, out: &mut Vec<String>) {
            for (name, n) in &d.children {
                let p = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                match n {
                    Node::File { digest, .. } => out.push(format!("{p} ({})", crate::digest_str(digest))),
                    Node::Dir(sub) => walk(sub, &p, out),
                    Node::Subtree(_) => out.push(format!("{p}/")),
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, "", &mut out);
        out
    }

    /// Every file and grafted subtree with its digest (`hash/size`, a
    /// subtree's prefixed with `tree:`), sorted by path: what makes the
    /// input root's digest.
    pub fn digests(&self) -> Vec<(String, String)> {
        fn walk(dir: &DirNode, prefix: &str, out: &mut Vec<(String, String)>) {
            for (name, node) in &dir.children {
                let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                match node {
                    Node::File { digest, executable, .. } => {
                        out.push((path, format!("{}/{}{}", digest.hash, digest.size_bytes, if *executable { " x" } else { "" })))
                    }
                    Node::Subtree(d) => out.push((path, format!("tree:{}/{}", d.hash, d.size_bytes))),
                    Node::Dir(d) => walk(d, &path, out),
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, "", &mut out);
        out
    }

    /// Serializes the tree, adding every blob (Directory protos and files) to
    /// `blobs`, and returns the root digest.
    pub fn build(&self, blobs: &mut BlobSet) -> re::Digest {
        build_dir(&self.root, blobs)
    }
}

impl InputTree {
    /// Lays the tree out under `root` for local execution of the same
    /// action: files are hardlinked (copied across filesystems), inline bytes
    /// written, and grafted subtrees symlinked to the local directory
    /// `subtree_dir` returns for their path and digest.
    ///
    /// Hardlinks share the original inode, so an action that modifies an
    /// input in place would modify the original too. Well-behaved actions
    /// (rustc, build scripts writing to OUT_DIR) don't.
    pub fn materialize_local(&self, root: &Path, subtree_dir: &dyn Fn(&str, &re::Digest) -> Option<PathBuf>) -> Result<()> {
        fn walk(dir: &DirNode, root: &Path, rel: &str, subtree_dir: &dyn Fn(&str, &re::Digest) -> Option<PathBuf>) -> Result<()> {
            let here = root.join(rel);
            fs::create_dir_all(&here)?;
            for (name, node) in &dir.children {
                let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                let dst = root.join(&child_rel);
                match node {
                    Node::Dir(d) => walk(d, root, &child_rel, subtree_dir)?,
                    Node::File { src, executable, .. } => {
                        match src {
                            BlobSource::File(p) => {
                                // Link the real file: hard-linking a symlink links
                                // the (possibly relative) symlink itself.
                                let p = &fs::canonicalize(p).with_context(|| format!("resolving {}", p.display()))?;
                                if fs::hard_link(p, &dst).is_err() {
                                    fs::copy(p, &dst).with_context(|| format!("copying {}", p.display()))?;
                                }
                            }
                            BlobSource::Bytes(b) => fs::write(&dst, b)?,
                            BlobSource::Remote(p) => bail!("{} only exists in the CAS; materialize it first", p.display()),
                        }
                        #[cfg(not(unix))]
                        let _ = executable;
                        #[cfg(unix)]
                        if *executable {
                            use std::os::unix::fs::PermissionsExt;
                            let mode = fs::metadata(&dst)?.permissions().mode();
                            if mode & 0o111 == 0 {
                                fs::set_permissions(&dst, fs::Permissions::from_mode(mode | 0o755))?;
                            }
                        }
                    }
                    Node::Subtree(digest) => {
                        let target = subtree_dir(&child_rel, digest).with_context(|| format!("no local directory for subtree `{child_rel}`"))?;
                        #[cfg(unix)]
                        std::os::unix::fs::symlink(&target, &dst)?;
                        #[cfg(not(unix))]
                        bail!("local execution needs symlinks ({})", target.display());
                    }
                }
            }
            Ok(())
        }
        walk(&self.root, root, "", subtree_dir)
    }

    /// Serializes the tree as an output `Tree` (root plus every descendant
    /// directory), e.g. to publish a locally produced directory to the
    /// action cache. Grafted subtrees aren't supported here.
    pub fn into_output_tree(self, blobs: &mut BlobSet) -> Result<re::Tree> {
        let mut children = Vec::new();
        let root = collect_dir(self.root, blobs, &mut children)?;
        Ok(re::Tree { root: Some(root), children })
    }
}

fn collect_dir(dir: DirNode, blobs: &mut BlobSet, children: &mut Vec<re::Directory>) -> Result<re::Directory> {
    let mut msg = re::Directory::default();
    for (name, node) in dir.children {
        match node {
            Node::File { digest, executable, src } => {
                blobs.insert(digest.clone(), src);
                msg.files.push(re::FileNode { name, digest: Some(digest), is_executable: executable, node_properties: None });
            }
            Node::Dir(d) => {
                let sub = collect_dir(d, blobs, children)?;
                let digest = blobs.insert_message(&sub);
                children.push(sub);
                msg.directories.push(re::DirectoryNode { name, digest: Some(digest) });
            }
            Node::Subtree(_) => bail!("grafted subtrees can't be part of an output tree"),
        }
    }
    Ok(msg)
}

fn build_dir(dir: &DirNode, blobs: &mut BlobSet) -> re::Digest {
    let mut msg = re::Directory::default();
    // BTreeMap<String> iterates in byte order, which is what REAPI requires.
    for (name, node) in &dir.children {
        let name = name.clone();
        match node {
            Node::File { digest, executable, src } => {
                blobs.insert(digest.clone(), src.clone());
                msg.files.push(re::FileNode { name, digest: Some(digest.clone()), is_executable: *executable, node_properties: None });
            }
            Node::Dir(d) => {
                let digest = build_dir(d, blobs);
                msg.directories.push(re::DirectoryNode { name, digest: Some(digest) });
            }
            Node::Subtree(digest) => {
                msg.directories.push(re::DirectoryNode { name, digest: Some(digest.clone()) });
            }
        }
    }
    blobs.insert_message(&msg)
}

/// Whether `name` is a single path component an output may have: not
/// empty, `.` or `..`, without separators (or, on Windows, a drive or
/// stream colon). A server's names must not reach outside the output.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && !(cfg!(windows) && name.contains(':'))
}

/// Lists every file of an output `Tree` with its path relative to the root.
pub fn tree_files(tree: &re::Tree) -> Result<Vec<(PathBuf, re::FileNode)>> {
    let by_digest: HashMap<String, &re::Directory> = tree
        .children
        .iter()
        .map(|d| (crate::digest_of_message(d).0.hash, d))
        .collect();
    let root = tree.root.as_ref().context("output tree without root")?;
    let mut files = Vec::new();
    let mut stack = vec![(PathBuf::new(), root)];
    while let Some((rel, dir)) = stack.pop() {
        for f in &dir.files {
            if !valid_name(&f.name) {
                bail!("output tree names a file {:?}", f.name);
            }
            files.push((rel.join(&f.name), f.clone()));
        }
        if let Some(l) = dir.symlinks.first() {
            bail!("output tree holds a symlink ({:?}), which isn't supported", l.name);
        }
        for sub in &dir.directories {
            if !valid_name(&sub.name) {
                bail!("output tree names a directory {:?}", sub.name);
            }
            let h = sub.digest.as_ref().map(|d| d.hash.clone()).unwrap_or_default();
            let child = by_digest.get(&h).with_context(|| format!("tree is missing directory {h}"))?;
            stack.push((rel.join(&sub.name), child));
        }
    }
    Ok(files)
}

/// Lists every directory of an output `Tree` (including empty ones),
/// relative to the root.
pub fn tree_dirs(tree: &re::Tree) -> Result<Vec<PathBuf>> {
    let by_digest: HashMap<String, &re::Directory> = tree
        .children
        .iter()
        .map(|d| (crate::digest_of_message(d).0.hash, d))
        .collect();
    let root = tree.root.as_ref().context("output tree without root")?;
    let mut dirs = Vec::new();
    let mut stack = vec![(PathBuf::new(), root)];
    while let Some((rel, dir)) = stack.pop() {
        for sub in &dir.directories {
            let h = sub.digest.as_ref().map(|d| d.hash.clone()).unwrap_or_default();
            let child = by_digest.get(&h).with_context(|| format!("tree is missing directory {h}"))?;
            dirs.push(rel.join(&sub.name));
            stack.push((rel.join(&sub.name), child));
        }
    }
    Ok(dirs)
}

/// Writes an output `Tree` to `dest` (created if needed). Returns the
/// relative paths of the files written.
pub async fn materialize_tree(client: &crate::Client, tree: &re::Tree, dest: &Path) -> Result<Vec<(PathBuf, re::Digest)>> {
    let files = tree_files(tree)?;
    let digests: Vec<re::Digest> = files.iter().map(|(_, f)| f.digest.clone().unwrap_or_default()).collect();
    let blobs = client.read_blobs(&digests).await?;
    let mut written = Vec::new();
    for (rel, f) in files {
        let d = f.digest.clone().unwrap_or_default();
        let data = blobs.get(&d.hash).with_context(|| format!("missing blob for {}", rel.display()))?;
        let path = dest.join(&rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let _ = fs::remove_file(&path);
        fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        if f.is_executable {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
        written.push((rel, d));
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_of_empty_is_the_sha256_of_nothing() {
        assert_eq!(crate::digest_of(b"").hash, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn root_digest_is_independent_of_insertion_order() {
        let mut a = InputTree::new();
        a.add_bytes("x/1", b"one".to_vec(), false).unwrap();
        a.add_bytes("y/2", b"two".to_vec(), true).unwrap();
        let mut b = InputTree::new();
        b.add_bytes("y/2", b"two".to_vec(), true).unwrap();
        b.add_bytes("x/1", b"one".to_vec(), false).unwrap();
        assert_eq!(a.build(&mut BlobSet::default()), b.build(&mut BlobSet::default()));
        // The executable bit is part of the digest.
        let mut c = InputTree::new();
        c.add_bytes("x/1", b"one".to_vec(), false).unwrap();
        c.add_bytes("y/2", b"two".to_vec(), false).unwrap();
        assert_ne!(a.build(&mut BlobSet::default()), c.build(&mut BlobSet::default()));
    }

    #[test]
    fn empty_dirs_are_part_of_the_tree() {
        let mut a = InputTree::new();
        a.add_bytes("f", b"x".to_vec(), false).unwrap();
        let mut b = InputTree::new();
        b.add_bytes("f", b"x".to_vec(), false).unwrap();
        b.ensure_dir("empty").unwrap();
        assert_ne!(a.build(&mut BlobSet::default()), b.build(&mut BlobSet::default()));
    }

    #[test]
    fn conflicting_inputs_are_rejected() {
        let mut t = InputTree::new();
        t.add_bytes("a", b"1".to_vec(), false).unwrap();
        t.add_bytes("a", b"1".to_vec(), false).unwrap();
        assert!(t.add_bytes("a", b"2".to_vec(), false).is_err());
        assert!(t.add_bytes("a/b", b"3".to_vec(), false).is_err());
        assert!(t.add_bytes("../escape", b"4".to_vec(), false).is_err());
    }

    #[test]
    fn output_tree_round_trip_keeps_empty_dirs() {
        let mut t = InputTree::new();
        t.add_bytes("a/b/file", b"data".to_vec(), true).unwrap();
        t.ensure_dir("a/empty").unwrap();
        let mut blobs = BlobSet::default();
        let tree = t.into_output_tree(&mut blobs).unwrap();
        let files = tree_files(&tree).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, PathBuf::from("a/b/file"));
        assert!(files[0].1.is_executable);
        let mut dirs = tree_dirs(&tree).unwrap();
        dirs.sort();
        assert_eq!(dirs, vec![PathBuf::from("a"), PathBuf::from("a/b"), PathBuf::from("a/empty")]);
    }

    #[cfg(unix)]
    #[test]
    fn materialize_local_links_real_files_and_resolves_subtrees() {
        let src = tempfile::tempdir().unwrap();
        let real = src.path().join("real");
        std::fs::write(&real, b"payload").unwrap();
        let link = src.path().join("alias");
        std::os::unix::fs::symlink("real", &link).unwrap();
        let sub = tempfile::tempdir().unwrap();

        let mut t = InputTree::new();
        t.add_file("bin/tool", &link).unwrap();
        t.add_bytes("gen/data", b"inline".to_vec(), false).unwrap();
        let sub_digest = crate::digest_of(b"subtree");
        t.add_subtree("toolchain", sub_digest.clone()).unwrap();

        let root = tempfile::tempdir().unwrap();
        t.materialize_local(root.path(), &|rel, d| (rel == "toolchain" && *d == sub_digest).then(|| sub.path().to_path_buf()))
            .unwrap();
        // A symlinked input is materialized as the file it points to.
        assert_eq!(std::fs::read(root.path().join("bin/tool")).unwrap(), b"payload");
        assert!(!std::fs::symlink_metadata(root.path().join("bin/tool")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(root.path().join("gen/data")).unwrap(), b"inline");
        assert_eq!(std::fs::read_link(root.path().join("toolchain")).unwrap(), sub.path());
        // An unknown subtree is an error.
        let other = tempfile::tempdir().unwrap();
        assert!(t.materialize_local(other.path(), &|_, _| None).is_err());
    }
}
