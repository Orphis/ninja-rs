// Copied from cargo-rbe (crates/rbe-client, commit a22f361); see ../README.md.
//! Building REAPI input roots (Merkle trees of `Directory` messages) from
//! local files, and materializing output trees back to disk.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use prost::Message;

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

/// What a cached digest is valid for (see `ninja_os::FileStamp`).
pub use ninja_os::FileStamp;

/// A cache of file digests keyed by path and `FileStamp`, owned by the
/// caller (no process-wide state).
pub trait DigestCache: Send + Sync {
    fn get(&self, path: &Path, stamp: &FileStamp) -> Option<re::Digest>;
    fn put(&self, path: &Path, stamp: FileStamp, digest: re::Digest);
}

/// A simple in-memory [`DigestCache`]. Each entry says whether its file
/// could still change unseen when it was put (see `FileStamp::is_racy`).
#[derive(Default)]
pub struct MemDigestCache(Mutex<HashMap<PathBuf, (FileStamp, re::Digest, bool)>>);

impl MemDigestCache {
    /// The entries to persist the cache with: not those put while their
    /// file could still change unseen, however long ago that was (a file
    /// changed again within its time granularity would keep its stamp).
    pub fn entries(&self) -> Vec<(PathBuf, FileStamp, re::Digest)> {
        self.0.lock().unwrap().iter().filter(|(_, (_, _, racy))| !racy).map(|(p, (s, d, _))| (p.clone(), *s, d.clone())).collect()
    }
}

impl DigestCache for MemDigestCache {
    fn get(&self, path: &Path, stamp: &FileStamp) -> Option<re::Digest> {
        let map = self.0.lock().unwrap();
        let (s, d, _) = map.get(path)?;
        if s == stamp { Some(d.clone()) } else { None }
    }
    fn put(&self, path: &Path, stamp: FileStamp, digest: re::Digest) {
        let racy = stamp.is_racy(std::time::SystemTime::now());
        self.0.lock().unwrap().insert(path.to_path_buf(), (stamp, digest, racy));
    }
}

/// SHA-256 of a file's contents.
pub fn hash_file(path: &Path) -> Result<re::Digest> {
    let d = ninja_os::hash_file(path)?;
    Ok(re::Digest { hash: d.hash, size_bytes: d.size as i64 })
}

/// The digest of a file, through `cache` when given.
/// The cache is keyed by the symlink-resolved path, so that many links to
/// one file (e.g. LLVM's multi-call binary) are hashed once.
pub fn file_digest(path: &Path, cache: Option<&dyn DigestCache>) -> Result<re::Digest> {
    let (_, stamp) = ninja_os::stat(path).with_context(|| format!("stat {}", path.display()))?;
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
        self.insert(rel, Node::File { digest, executable: ninja_os::is_executable(&meta), src: BlobSource::File(local.to_path_buf()) })
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

/// The most files and directories an output `Tree` may expand to: its
/// directories may share children, each listed once, so that a small tree
/// can stand for a huge one (2^40 paths).
const MAX_TREE_ENTRIES: usize = 1 << 20;

/// Counts `n` more entries of an output tree being expanded.
fn expanding(entries: &mut usize, n: usize) -> Result<()> {
    *entries += n;
    if *entries > MAX_TREE_ENTRIES {
        bail!("output tree expands to more than {MAX_TREE_ENTRIES} files and directories");
    }
    Ok(())
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
    let mut entries = 0;
    while let Some((rel, dir)) = stack.pop() {
        expanding(&mut entries, 1 + dir.files.len())?;
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
    let mut entries = 0;
    while let Some((rel, dir)) = stack.pop() {
        expanding(&mut entries, 1)?;
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

    #[test]
    fn trees_sharing_children_are_bounded() {
        // 40 levels of directories naming the one below twice: 2^40 paths.
        let mut dir = re::Directory { files: vec![re::FileNode { name: "f".into(), digest: Some(crate::digest_of(b"x")), ..Default::default() }], ..Default::default() };
        let mut children = Vec::new();
        for _ in 0..40 {
            let d = crate::digest_of_message(&dir).0;
            children.push(dir);
            let sub = |name: &str| re::DirectoryNode { name: name.into(), digest: Some(d.clone()) };
            dir = re::Directory { directories: vec![sub("a"), sub("b")], ..Default::default() };
        }
        let tree = re::Tree { root: Some(dir), children };
        assert!(format!("{:#}", tree_files(&tree).unwrap_err()).contains("expands to more than"));
        assert!(tree_dirs(&tree).is_err());
    }

    #[test]
    fn digests_put_while_racy_arent_persisted() {
        let dir = test_dirs::TestDir::new("rbe-racy-digests");
        let (fresh, old) = (dir.join("fresh"), dir.join("old"));
        std::fs::write(&fresh, "fresh").unwrap();
        std::fs::write(&old, "old").unwrap();
        let set = |p: &Path, t| std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
        let now = std::time::SystemTime::now();
        // Modified within the time granularity of when it's hashed (here, a
        // clock ahead): used, but not persisted, even once that's long past.
        set(&fresh, now + std::time::Duration::from_secs(3600));
        set(&old, now - std::time::Duration::from_secs(3600));
        let cache = MemDigestCache::default();
        let d = file_digest(&fresh, Some(&cache)).unwrap();
        file_digest(&old, Some(&cache)).unwrap();
        assert_eq!(cache.get(&fresh, &ninja_os::stat(&fresh).unwrap().1), Some(d));
        let kept: Vec<PathBuf> = cache.entries().into_iter().map(|(p, _, _)| p).collect();
        assert_eq!(kept, [old]);
    }

    #[cfg(unix)]
    #[test]
    fn materialize_local_links_real_files_and_resolves_subtrees() {
        let src = test_dirs::TestDir::new("rbe-materialize-src");
        let real = src.join("real");
        std::fs::write(&real, b"payload").unwrap();
        let link = src.join("alias");
        std::os::unix::fs::symlink("real", &link).unwrap();
        let sub = test_dirs::TestDir::new("rbe-materialize-sub");

        let mut t = InputTree::new();
        t.add_file("bin/tool", &link).unwrap();
        t.add_bytes("gen/data", b"inline".to_vec(), false).unwrap();
        let sub_digest = crate::digest_of(b"subtree");
        t.add_subtree("toolchain", sub_digest.clone()).unwrap();

        let root = test_dirs::TestDir::new("rbe-materialize-root");
        t.materialize_local(&root, &|rel, d| (rel == "toolchain" && *d == sub_digest).then(|| sub.to_path_buf()))
            .unwrap();
        // A symlinked input is materialized as the file it points to.
        assert_eq!(std::fs::read(root.join("bin/tool")).unwrap(), b"payload");
        assert!(!std::fs::symlink_metadata(root.join("bin/tool")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(root.join("gen/data")).unwrap(), b"inline");
        assert_eq!(std::fs::read_link(root.join("toolchain")).unwrap(), sub.path());
        // An unknown subtree is an error.
        let other = test_dirs::TestDir::new("rbe-materialize-other");
        assert!(t.materialize_local(&other, &|_, _| None).is_err());
    }
}
