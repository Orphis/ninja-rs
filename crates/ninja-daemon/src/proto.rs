//! The messages between builds and the daemon: each a little-endian u32
//! length, then JSON. A connection carries one request at a time.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Changes with the messages: a client and a daemon must agree.
pub const PROTOCOL: u32 = 1;

/// Larger frames are refused (a corrupt length).
const MAX_FRAME: usize = 1 << 30;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Hello {
        protocol: u32,
    },
    /// A build starts.
    Begin(Begin),
    /// Scans a compile command (response files expanded) run in `cwd`.
    Scan {
        session: u64,
        argv: Vec<String>,
        cwd: PathBuf,
    },
    /// The digests of files.
    Files {
        paths: Vec<PathBuf>,
    },
    /// The files under a directory, with their digests (a toolchain input).
    Tree {
        dir: PathBuf,
    },
    Status,
    Stop,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Begin {
    /// Its files may change during the build: they are always checked.
    pub build_dir: PathBuf,
    /// The build's environment, which compilers run in.
    pub env: Vec<(String, String)>,
    /// Where compiler profiles persist (in the disk cache).
    pub profile_dir: Option<PathBuf>,
    /// File digests the build saved (`.ninja_rs_digests`), to start from.
    pub digests_file: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Hello { protocol: u32, pid: u32, version: String },
    Begun { session: u64 },
    Scanned(Result<ScanOutput, String>),
    Files(Vec<Result<FileInfo, String>>),
    Tree(Result<TreeInfo, String>),
    Status(Status),
    Stopping,
    Error(String),
}

/// `cpp_deps::Output`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOutput {
    pub files: Vec<PathBuf>,
    pub found: Vec<PathBuf>,
    pub missing: Vec<String>,
    pub unknown: Vec<(String, String)>,
    pub inexact: Vec<String>,
}

impl From<cpp_deps::Output> for ScanOutput {
    fn from(o: cpp_deps::Output) -> Self {
        ScanOutput { files: o.files, found: o.found, missing: o.missing, unknown: o.unknown, inexact: o.inexact }
    }
}

impl From<ScanOutput> for cpp_deps::Output {
    fn from(o: ScanOutput) -> Self {
        cpp_deps::Output { files: o.files, found: o.found, missing: o.missing, unknown: o.unknown, inexact: o.inexact }
    }
}

/// A file's digest, and the version it's of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    /// SHA-256, hex.
    pub hash: String,
    pub size: u64,
    pub executable: bool,
    /// Nanoseconds since the Unix epoch.
    pub mtime_ns: u64,
}

/// A directory's contents, as `InputTree::add_dir` adds them (symlinks
/// followed): every directory and file, `/`-separated and relative to it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeInfo {
    pub dirs: Vec<String>,
    pub files: Vec<(String, FileInfo)>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub version: String,
    pub uptime_secs: u64,
    pub builds: u64,
    pub requests: u64,
    /// File events seen (outside build directories).
    pub events: u64,
    /// Watched roots and their states.
    pub roots: Vec<(PathBuf, String)>,
    /// Lexed files and directory listings the preprocessor keeps.
    pub lexed: usize,
    pub listings: usize,
    pub digests: usize,
    pub trees: usize,
    /// Files the preprocessor lexed, and found lexed already.
    pub files_lexed: u64,
    pub files_reused: u64,
}

pub fn write<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame)?;
    w.flush()
}

/// The next message; None when the other side closed the connection.
pub fn read<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("a {len} byte message")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames() {
        let mut buf = Vec::new();
        write(&mut buf, &Request::Files { paths: vec!["a/b.h".into()] }).unwrap();
        write(&mut buf, &Request::Status).unwrap();
        let mut r = buf.as_slice();
        assert!(matches!(read::<Request>(&mut r).unwrap(), Some(Request::Files { paths }) if paths == vec![PathBuf::from("a/b.h")]));
        assert!(matches!(read::<Request>(&mut r).unwrap(), Some(Request::Status)));
        assert!(read::<Request>(&mut r).unwrap().is_none());
        let mut bad = &[0xff, 0xff, 0xff, 0xff][..];
        assert!(read::<Request>(&mut bad).is_err());
    }
}
