//! Thin archives: GNU-format archives whose members are paths to files next
//! to them rather than copies (`llvm-ar --thin`, `ar T`,
//! `llvm-lib /llvmlibthin`). An action reading one reads its members too,
//! so they are its inputs as well.
//!
//! The member list is in the member headers and the name table, which
//! follow the symbol table: [`symbols_end`] finds where they start from the
//! first [`HEAD`] bytes, and [`members_from`] reads the rest. An archive
//! that only exists remotely is read that way, with ranged reads that skip
//! its symbol table.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::remote_outputs::RemoteOutputs;
use crate::store::Store;

pub const MAGIC: &[u8; 8] = b"!<thin>\n";
const HEADER: usize = 60;
/// The bytes to read first: the magic and the first member's header.
pub const HEAD: usize = MAGIC.len() + HEADER;

struct Header {
    name: String,
    size: usize,
}

fn header(b: &[u8]) -> Result<Header, String> {
    if b.len() < HEADER {
        return Err("truncated member header".into());
    }
    if &b[58..60] != b"`\n" {
        return Err("bad member header".into());
    }
    let name = String::from_utf8_lossy(&b[..16]).trim_end().to_string();
    let size = std::str::from_utf8(&b[48..58]).ok().and_then(|s| s.trim().parse().ok()).ok_or("bad member size")?;
    Ok(Header { name, size })
}

fn is_symbol_table(name: &str) -> bool {
    name == "/" || name == "/SYM64/"
}

/// Members are aligned on even offsets.
fn padded(n: usize) -> usize {
    n + (n & 1)
}

/// Whether `head`, the first bytes of a file, starts a thin archive.
pub fn is_thin(head: &[u8]) -> bool {
    head.starts_with(MAGIC)
}

/// The offset past the symbol table, where [`members_from`] starts. `head`
/// is the first [`HEAD`] bytes of the archive (all of it, if shorter).
pub fn symbols_end(head: &[u8]) -> Result<usize, String> {
    if !is_thin(head) {
        return Err("not a thin archive".into());
    }
    if head.len() < HEAD {
        return Ok(MAGIC.len());
    }
    let h = header(&head[MAGIC.len()..])?;
    Ok(if is_symbol_table(&h.name) { HEAD + padded(h.size) } else { MAGIC.len() })
}

/// The member paths of a thin archive, as stored (relative to the archive's
/// directory, or absolute), from its bytes past the symbol table.
pub fn members_from(data: &[u8]) -> Result<Vec<String>, String> {
    let mut names: &[u8] = &[];
    let mut members = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let h = header(&data[pos..])?;
        pos += HEADER;
        // The symbol tables and the name table are the only members with
        // data in a thin archive.
        if is_symbol_table(&h.name) || h.name == "//" {
            let body = data.get(pos..pos + h.size).ok_or("truncated archive")?;
            if h.name == "//" {
                names = body;
            }
            pos = (pos + padded(h.size)).min(data.len());
            continue;
        }
        let name = match h.name.strip_prefix('/') {
            Some(offset) => {
                let offset: usize = offset.parse().map_err(|_| format!("bad member name '{}'", h.name))?;
                let rest = names.get(offset..).ok_or("member name outside the name table")?;
                let end = rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len());
                let name = String::from_utf8_lossy(&rest[..end]);
                name.strip_suffix('/').unwrap_or(&name).to_string()
            }
            None => h.name.strip_suffix('/').unwrap_or(&h.name).to_string(),
        };
        members.push(name);
    }
    Ok(members)
}

/// The members of a thin archive, from all its bytes. None if it isn't one.
pub fn members(data: &[u8]) -> Result<Option<Vec<String>>, String> {
    if !is_thin(data) {
        return Ok(None);
    }
    let start = symbols_end(&data[..data.len().min(HEAD)])?;
    members_from(data.get(start..).ok_or("truncated archive")?).map(Some)
}

/// The members of a thin archive on disk, reading its symbol table no more
/// than [`members_from`] needs. None if it isn't one.
pub fn read_members(path: &Path) -> std::io::Result<Option<Vec<String>>> {
    let mut f = std::fs::File::open(path)?;
    let mut head = Vec::with_capacity(HEAD);
    (&mut f).take(HEAD as u64).read_to_end(&mut head)?;
    if !is_thin(&head) {
        return Ok(None);
    }
    let invalid = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e}", path.display()));
    let start = symbols_end(&head).map_err(invalid)?;
    f.seek(SeekFrom::Start(start as u64))?;
    let mut rest = Vec::new();
    f.read_to_end(&mut rest)?;
    members_from(&rest).map(Some).map_err(invalid)
}

/// Whether a command writes a thin archive: an `ar`-style archiver with
/// `--thin` or the `T` modifier, or `llvm-lib /llvmlibthin`.
pub fn creates_thin(argv: &[String]) -> bool {
    let Some(program) = argv.first() else { return false };
    let stem = Path::new(&program.replace('\\', "/")).file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let args = &argv[1..];
    if stem == "llvm-lib" || stem == "lib" {
        return args.iter().any(|a| a.eq_ignore_ascii_case("/llvmlibthin") || a.eq_ignore_ascii_case("-llvmlibthin"));
    }
    if !stem.ends_with("ar") {
        return false;
    }
    if args.iter().any(|a| a == "--thin") {
        return true;
    }
    // The operation and its modifiers: the first argument that isn't a
    // long option or a response file.
    args.iter()
        .find(|a| !a.starts_with("--") && !a.starts_with('@'))
        .map(|ops| ops.trim_start_matches('-'))
        .is_some_and(|ops| !ops.is_empty() && ops.chars().all(|c| c.is_ascii_alphanumeric()) && ops.contains('T'))
}

fn is_abs(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/') || p.starts_with('\\') || (b.len() > 2 && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\'))
}

/// A member's path as the build names paths: `name` resolved against the
/// directory of `archive`, both relative to the build directory. Members
/// are objects the build made: one outside the build directory is refused
/// (an archive from a result, or its ranged reads, would otherwise make
/// every action reading it send any file the user can read).
pub fn member_path(archive: &str, name: &str) -> Result<String, String> {
    if is_abs(name) {
        // Remotely, the path would name a file outside the input root.
        return Err(format!("thin archive {archive} names its member {name} by an absolute path"));
    }
    let archive = archive.replace('\\', "/");
    let joined = match archive.rfind('/') {
        Some(i) => format!("{}/{}", &archive[..i], name.replace('\\', "/")),
        None => name.replace('\\', "/"),
    };
    let path = ninja_core::canon::canonicalize_path_with(&joined, false).0;
    if path == ".." || path.starts_with("../") {
        return Err(format!("thin archive {archive} names its member {name} outside the build directory"));
    }
    Ok(path)
}

/// What [`expand`] does with a member outside the build directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outside {
    /// Refuse it, for an action's input tree: an archive from a result, or
    /// its ranged reads, would otherwise make the action send any file the
    /// user can read. Also when only a symlink or junction in the build
    /// directory makes it look inside.
    Refuse,
    /// Skip it, for a local command: it reads the member where it is (a
    /// prebuilt object of the source tree), and it's no remote-only output
    /// to download. Archives that can't be read are skipped too: the
    /// command reports them.
    Skip,
}

/// The members of the thin archives among `inputs` (paths relative to the
/// build directory, on disk or recorded as remote-only outputs), and of thin
/// archives among those members, that aren't inputs already.
pub fn expand(inputs: &[String], build_dir: &Path, remote: Option<&RemoteOutputs>, store: &Store, outside: Outside) -> Result<Vec<String>, String> {
    let mut seen: std::collections::HashSet<String> = inputs.iter().map(|i| crate::remote_outputs::key(i)).collect();
    let mut queue: Vec<String> = inputs.to_vec();
    let mut out = Vec::new();
    let mut resolved = Resolved::default();
    while let Some(input) = queue.pop() {
        let local = build_dir.join(&input);
        let names = if local.is_file() {
            match store.thin_members(&local) {
                Ok(names) => names,
                Err(_) if outside == Outside::Skip => continue,
                Err(e) => return Err(e),
            }
        } else {
            remote.and_then(|r| r.get(&input)).and_then(|e| e.members)
        };
        for name in names.iter().flat_map(|n| n.iter()) {
            let m = match member_path(&input, name) {
                Ok(m) => m,
                Err(_) if outside == Outside::Skip => continue,
                Err(e) => return Err(e),
            };
            if outside == Outside::Refuse && !resolved.inside(&build_dir.join(&m), build_dir) {
                return Err(format!("thin archive {input} names its member {name} through a link out of the build directory"));
            }
            if seen.insert(crate::remote_outputs::key(&m)) {
                out.push(m.clone());
                queue.push(m);
            }
        }
    }
    Ok(out)
}

/// Where files of the build directory really are, symlinks and junctions
/// followed: the build directory's canonical path, and its directories'.
#[derive(Default)]
struct Resolved {
    root: Option<Option<std::path::PathBuf>>,
    dirs: std::collections::HashMap<std::path::PathBuf, Option<std::path::PathBuf>>,
}

impl Resolved {
    /// Whether `local`, a file of `build_dir` by its name, is in it once
    /// links are followed. One that isn't there is: it can only be a
    /// remote-only output, sent by its digest.
    fn inside(&mut self, local: &Path, build_dir: &Path) -> bool {
        let Ok(meta) = std::fs::symlink_metadata(local) else { return true };
        let real = if meta.file_type().is_symlink() {
            std::fs::canonicalize(local).ok()
        } else {
            let (Some(dir), Some(name)) = (local.parent(), local.file_name()) else { return false };
            self.dirs.entry(dir.to_path_buf()).or_insert_with(|| std::fs::canonicalize(dir).ok()).as_ref().map(|d| d.join(name))
        };
        let root = self.root.get_or_insert_with(|| std::fs::canonicalize(build_dir).ok());
        match (real, root.as_ref()) {
            (Some(real), Some(root)) => real.starts_with(root),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LLVM_GNU: &[u8] = include_bytes!("../testdata/llvm-gnu.a");
    const LLVM_COFF: &[u8] = include_bytes!("../testdata/llvm-coff.lib");
    const BINUTILS: &[u8] = include_bytes!("../testdata/binutils.a");
    const NO_SYMBOLS: &[u8] = include_bytes!("../testdata/nosym.a");
    const FAT: &[u8] = include_bytes!("../testdata/fat.a");

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn archivers_formats() {
        // llvm-ar --thin, with a symbol table and long names.
        assert_eq!(members(LLVM_GNU).unwrap(), Some(v(&["../obj/a.o", "../obj/a_member_with_a_long_name.o"])));
        // GNU ar T writes the same layout.
        assert_eq!(members(BINUTILS).unwrap(), Some(v(&["../obj/a.o", "../obj/a_member_with_a_long_name.o"])));
        // llvm-lib /llvmlibthin (COFF objects, still the GNU format).
        assert_eq!(members(LLVM_COFF).unwrap(), Some(v(&["../obj/b.obj", "../obj/a.obj"])));
        // No symbol table: members follow the magic.
        assert_eq!(members(NO_SYMBOLS).unwrap(), Some(v(&["../a.c"])));
        assert_eq!(symbols_end(&NO_SYMBOLS[..HEAD]).unwrap(), MAGIC.len());
        assert_eq!(members(FAT).unwrap(), None);
        assert_eq!(members(MAGIC).unwrap(), Some(vec![]));
    }

    #[test]
    fn ranged_reading_skips_the_symbol_table() {
        let start = symbols_end(&LLVM_GNU[..HEAD]).unwrap();
        assert!(start > HEAD);
        assert!(LLVM_GNU[start..].starts_with(b"//"));
        assert_eq!(members_from(&LLVM_GNU[start..]).unwrap(), members(LLVM_GNU).unwrap().unwrap());
    }

    #[test]
    fn errors() {
        assert!(members(&LLVM_GNU[..LLVM_GNU.len() - 10]).is_err());
        assert!(symbols_end(FAT).is_err());
        let mut bad = LLVM_GNU.to_vec();
        bad[HEAD - 2] = b'x';
        assert!(members(&bad).is_err());
    }

    #[test]
    fn short_names() {
        let mut a = MAGIC.to_vec();
        a.extend_from_slice(format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", "x.o/", 0, 0, 0, 644, 1234).as_bytes());
        assert_eq!(members(&a).unwrap(), Some(v(&["x.o"])));
    }

    #[test]
    fn reads_from_disk() {
        let dir = test_dirs::TestDir::new("ninja_rs_thin");
        std::fs::write(dir.join("t.a"), LLVM_GNU).unwrap();
        std::fs::write(dir.join("f.a"), FAT).unwrap();
        assert_eq!(read_members(&dir.join("t.a")).unwrap(), members(LLVM_GNU).unwrap());
        assert_eq!(read_members(&dir.join("f.a")).unwrap(), None);
    }

    #[test]
    fn thin_archivers() {
        let a = |s: &str| creates_thin(&v(&s.split(' ').collect::<Vec<_>>()));
        assert!(a("/usr/bin/llvm-ar qcT lib.a a.o"));
        assert!(a("llvm-ar --format=gnu qcT lib.a a.o"));
        assert!(a("ar -rcsT lib.a a.o"));
        assert!(a("x86_64-linux-gnu-ar --thin qc lib.a a.o"));
        assert!(a("C:\\llvm\\bin\\llvm-ar.exe qcT lib.a a.o"));
        assert!(a("llvm-lib.exe /llvmlibthin /out:x.lib a.obj"));
        assert!(!a("llvm-ar qc lib.a T.o"));
        assert!(!a("llvm-ar qc lib.a a.o"));
        assert!(!a("llvm-lib /out:x.lib a.obj"));
        assert!(!a("clang -T script.ld a.o"));
    }

    #[test]
    fn member_paths() {
        assert_eq!(member_path("lib/libt.a", "../obj/a.o").unwrap(), "obj/a.o");
        assert_eq!(member_path("libt.a", "obj/a.o").unwrap(), "obj/a.o");
        assert_eq!(member_path("lib\\libt.a", "..\\obj\\a.o").unwrap(), "obj/a.o");
        // Outside the build directory: refused, as an archive from a result
        // could name any file.
        assert!(member_path("libt.a", "../src/x.o").is_err());
        assert!(member_path("lib/libt.a", "../../../home/u/.ssh/id_ed25519").is_err());
        assert!(member_path("libt.a", "/abs/a.o").is_err());
        assert!(member_path("libt.a", "C:/abs/a.o").is_err());
    }

    #[test]
    fn expands_nested_archives() {
        let dir = test_dirs::TestDir::new("ninja_rs_thin_expand");
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::create_dir_all(dir.join("obj")).unwrap();
        std::fs::write(dir.join("lib/llvm-gnu.a"), LLVM_GNU).unwrap();
        // A thin archive whose member is a thin archive (GNU ar can nest).
        let mut outer = MAGIC.to_vec();
        outer.extend_from_slice(format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", "llvm-gnu.a/", 0, 0, 0, 644, LLVM_GNU.len()).as_bytes());
        std::fs::write(dir.join("lib/outer.a"), outer).unwrap();
        let store = Store::default();
        let mut got = expand(&v(&["lib/outer.a", "obj/a.o"]), &dir, None, &store, Outside::Refuse).unwrap();
        got.sort();
        assert_eq!(got, v(&["lib/llvm-gnu.a", "obj/a_member_with_a_long_name.o"]));
    }

    /// A thin archive naming `names` (short names: 15 bytes at most).
    fn thin(names: &[&str]) -> Vec<u8> {
        let mut a = MAGIC.to_vec();
        for n in names {
            assert!(n.len() < 16, "{n} is too long for a short name");
            a.extend_from_slice(format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", format!("{n}/"), 0, 0, 0, 644, 10).as_bytes());
        }
        assert_eq!(members(&a).unwrap().unwrap(), v(names));
        a
    }

    /// Members outside the build directory: refused for an input tree,
    /// skipped for a local command. Also through a link in the build
    /// directory (a symlink, or a junction on Windows).
    #[test]
    fn members_outside() {
        let dir = test_dirs::TestDir::new("ninja_rs_thin_outside");
        let build = dir.join("build");
        std::fs::create_dir_all(build.join("lib")).unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out/x.o"), "x").unwrap();
        std::fs::write(build.join("lib/out.a"), thin(&["../../out/x.o"])).unwrap();
        let store = Store::default();
        let out = v(&["lib/out.a"]);
        assert!(expand(&out, &build, None, &store, Outside::Refuse).is_err());
        assert_eq!(expand(&out, &build, None, &store, Outside::Skip).unwrap(), Vec::<String>::new());
        // An archive that can't be read: the local command reports it.
        std::fs::write(build.join("lib/bad.a"), [MAGIC.as_slice(), b"short"].concat()).unwrap();
        assert!(expand(&v(&["lib/bad.a"]), &build, None, &store, Outside::Refuse).is_err());
        assert!(expand(&v(&["lib/bad.a"]), &build, None, &store, Outside::Skip).unwrap().is_empty());
        // Inside by its name, outside through a link.
        let link = build.join("lnk");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("out"), &link).unwrap();
        #[cfg(windows)]
        assert!(std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(&link).arg(dir.join("out")).output().unwrap().status.success());
        std::fs::write(build.join("lib/linked.a"), thin(&["../lnk/x.o"])).unwrap();
        let linked = v(&["lib/linked.a"]);
        assert!(expand(&linked, &build, None, &store, Outside::Refuse).unwrap_err().contains("through a link"));
        assert_eq!(expand(&linked, &build, None, &store, Outside::Skip).unwrap(), v(&["lnk/x.o"]));
        // A member really inside, and one that isn't there (remote-only).
        std::fs::write(build.join("lib/in.a"), thin(&["a.o", "gone.o"])).unwrap();
        std::fs::write(build.join("lib/a.o"), "a").unwrap();
        assert_eq!(expand(&v(&["lib/in.a"]), &build, None, &store, Outside::Refuse).unwrap().len(), 2);
    }
}
