//! The linker directives of COFF objects and libraries: the options in
//! their `.drectve` sections (`/DEFAULTLIB:msvcrt` from `/MD`, what
//! `#pragma comment(lib, ...)` asks for), with which an MSVC link pulls in
//! libraries its command line doesn't name (see `link::env_files_read`).
//!
//! Objects are COFF files (regular or `/bigobj`); libraries are archives of
//! them and of import objects, which have no directives, and resource files
//! (`.res`) have none either. Anything else (LTO bitcode, `/GL` objects,
//! thin archives) can't be read here: its directives are unknown.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The archive signature.
const ARCHIVE: &[u8; 8] = b"!<arch>\n";
const MEMBER_HEADER: usize = 60;
/// The class of `/bigobj` objects' header.
const BIGOBJ_CLASS: [u8; 16] = [0xc7, 0xa1, 0xba, 0xd1, 0xee, 0xba, 0xa9, 0x4b, 0xaf, 0x20, 0xfa, 0xf6, 0x6a, 0xa4, 0xdc, 0xb8];
/// The machines of COFF objects (x86, x64, ARM, ARM64 and its EC and X
/// kinds), and none (`IMAGE_FILE_MACHINE_UNKNOWN`: an object for any, as
/// MSVC's runtime libraries hold, `msvcrt.lib`'s and `oldnames.lib`'s
/// among them): other first bytes aren't one.
const MACHINES: &[u16] = &[0x014c, 0x8664, 0x01c4, 0xaa64, 0xa641, 0xa64e, 0];
/// A resource file starts with an empty resource.
const RES_HEADER: [u8; 16] = [0, 0, 0, 0, 0x20, 0, 0, 0, 0xff, 0xff, 0, 0, 0xff, 0xff, 0, 0];

/// The directives of an object, a library or a resource file, as options
/// (tokenized as the linker does). None when it isn't one of those, or its
/// contents can't be read as one.
pub fn read(path: &Path) -> std::io::Result<Option<Vec<String>>> {
    let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
    let size = f.get_ref().metadata()?.len();
    let mut magic = [0u8; 8];
    let n = read_up_to(&mut f, &mut magic)?;
    let mut raw = Vec::new();
    if n == ARCHIVE.len() && magic == *ARCHIVE {
        let mut pos = ARCHIVE.len() as u64;
        while pos + MEMBER_HEADER as u64 <= size {
            f.seek(SeekFrom::Start(pos))?;
            let mut header = [0u8; MEMBER_HEADER];
            f.read_exact(&mut header)?;
            if &header[58..] != b"`\n" {
                return Ok(None);
            }
            let Some(len) = std::str::from_utf8(&header[48..58]).ok().and_then(|s| s.trim().parse::<u64>().ok()) else { return Ok(None) };
            let start = pos + MEMBER_HEADER as u64;
            if start + len > size {
                return Ok(None);
            }
            // The symbol tables and the long names aren't objects.
            let name = &header[..16];
            if !name.starts_with(b"/ ") && !name.starts_with(b"// ") && !name.starts_with(b"/SYM64/") && !name.starts_with(b"/<") {
                match object(&mut f, start, len)? {
                    Some(d) => raw.extend(d),
                    None => return Ok(None),
                }
                raw.push(b' ');
            }
            pos = start + len + (len & 1);
        }
    } else {
        match object(&mut f, 0, size)? {
            Some(d) => raw = d,
            None => return Ok(None),
        }
    }
    Ok(Some(options(&raw)))
}

/// Reads as much of `buf` as there is.
fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The `.drectve` bytes of the object `len` bytes long at `start`: empty
/// for an import object or a resource file, None for anything else than a
/// COFF object.
fn object<R: Read + Seek>(f: &mut R, start: u64, len: u64) -> std::io::Result<Option<Vec<u8>>> {
    f.seek(SeekFrom::Start(start))?;
    let mut head = [0u8; 56];
    let n = read_up_to(f, &mut head[..len.min(56) as usize])?;
    let head = &head[..n];
    if n >= 16 && head[..16] == RES_HEADER {
        return Ok(Some(Vec::new()));
    }
    if n < 20 {
        return Ok(None);
    }
    let (sections, table) = if u16_at(head, 0) == 0 && u16_at(head, 2) == 0xffff {
        match u16_at(head, 4) {
            // A short import object.
            0 => return Ok(Some(Vec::new())),
            // `/bigobj`; any other class (`/GL`'s) can't be read.
            v if v >= 2 && n >= 56 && head[12..28] == BIGOBJ_CLASS => (u32_at(head, 44) as u64, 56u64),
            _ => return Ok(None),
        }
    } else {
        // Objects have no optional header. (Machine 0 with 0xffff sections
        // is the header above.)
        if !MACHINES.contains(&u16_at(head, 0)) || u16_at(head, 16) != 0 {
            return Ok(None);
        }
        (u16_at(head, 2) as u64, 20u64)
    };
    if table + sections * 40 > len {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(start + table))?;
    let mut out = Vec::new();
    let mut section = [0u8; 40];
    for _ in 0..sections {
        f.read_exact(&mut section)?;
        if &section[..8] != b".drectve" {
            continue;
        }
        let (size, at) = (u32_at(&section, 16) as u64, u32_at(&section, 20) as u64);
        if at + size > len {
            return Ok(None);
        }
        let here = f.stream_position()?;
        f.seek(SeekFrom::Start(start + at))?;
        let mut data = vec![0u8; size as usize];
        f.read_exact(&mut data)?;
        out.extend(data);
        out.push(b' ');
        f.seek(SeekFrom::Start(here))?;
    }
    Ok(Some(out))
}

/// Directives as options: split as a Windows command line, after the
/// UTF-8 mark they may start with (each section's).
fn options(raw: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(raw).replace(['\u{feff}', '\0'], " ");
    crate::cmdline::split_windows(&text, false)
}

/// The libraries directives ask for (`/DEFAULTLIB:`), as named there.
pub fn default_libs(options: &[String]) -> impl Iterator<Item = &str> {
    options.iter().filter_map(|o| {
        let (name, value) = o.strip_prefix(['/', '-'])?.split_once(':')?;
        name.eq_ignore_ascii_case("defaultlib").then_some(value.trim_matches('"')).filter(|v| !v.is_empty())
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A COFF object (x64) with these sections: (name, contents).
    pub(crate) fn coff(sections: &[(&str, &[u8])]) -> Vec<u8> {
        coff_for(0x8664, sections)
    }

    /// A COFF object for `machine` with these sections.
    pub(crate) fn coff_for(machine: u16, sections: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(machine.to_le_bytes());
        out.extend((sections.len() as u16).to_le_bytes());
        out.extend([0u8; 12]);
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        let mut data_at = 20 + 40 * sections.len();
        for (name, data) in sections {
            let mut header = [0u8; 40];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[16..20].copy_from_slice(&(data.len() as u32).to_le_bytes());
            header[20..24].copy_from_slice(&(data_at as u32).to_le_bytes());
            out.extend(header);
            data_at += data.len();
        }
        for (_, data) in sections {
            out.extend(*data);
        }
        out
    }

    /// An archive of these members: (name, contents), with a symbol table.
    pub(crate) fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = ARCHIVE.to_vec();
        let header = |name: &str, len: usize| format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{len:<10}`\n", 0, 0, 0, 644).into_bytes();
        let symbols = [0u8; 9];
        for (name, data) in std::iter::once(("/", &symbols[..])).chain(members.iter().copied()) {
            out.extend(header(name, data.len()));
            out.extend(data);
            if data.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    #[test]
    fn objects_and_libraries() {
        let dir = test_dirs::TestDir::new("ninja-rs-directives");
        let read_bytes = |name: &str, data: &[u8]| {
            std::fs::write(dir.join(name), data).unwrap();
            read(&dir.join(name)).unwrap()
        };
        let drectve: &[u8] = b"\xef\xbb\xbf /DEFAULTLIB:\"MSVCRT\" /DEFAULTLIB:\"OLDNAMES\" /FAILIFMISMATCH:\"_MSC_VER=1900\"";
        let obj = coff(&[(".text$mn", b"\xc3"), (".drectve", drectve)]);
        let options = read_bytes("a.obj", &obj).unwrap();
        assert_eq!(default_libs(&options).collect::<Vec<_>>(), ["MSVCRT", "OLDNAMES"]);
        // Without directives (`/Zl`).
        assert_eq!(read_bytes("b.obj", &coff(&[(".text$mn", b"\xc3")])), Some(vec![]));
        // A library: its objects' directives; import objects have none.
        let import = [0u8, 0, 0xff, 0xff, 0, 0, 0x64, 0x86, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let pragma = coff(&[(".drectve", b"-defaultlib:ws2_32.lib")]);
        let lib = archive(&[("kernel32.dll/", &import), ("a.obj/", &obj), ("p.obj/", &pragma)]);
        let options = read_bytes("x.lib", &lib).unwrap();
        assert_eq!(default_libs(&options).collect::<Vec<_>>(), ["MSVCRT", "OLDNAMES", "ws2_32.lib"]);
        // Objects for any machine (Machine 0), as msvcrt.lib's: their
        // directives too.
        let any = coff_for(0, &[(".drectve", b" /DEFAULTLIB:\"vcruntime\""), (".data", b"\0\0\0\0")]);
        let lib = archive(&[("msvcrt.dll/", &import), ("any.obj/", &any), ("a.obj/", &obj)]);
        let options = read_bytes("msvcrt.lib", &lib).unwrap();
        assert_eq!(default_libs(&options).collect::<Vec<_>>(), ["vcruntime", "MSVCRT", "OLDNAMES"]);
        assert_eq!(read_bytes("any.obj", &coff_for(0, &[(".data", b"\0")])), Some(vec![]));
        // Resources have none.
        assert_eq!(read_bytes("a.res", &[RES_HEADER.as_slice(), &[0u8; 16]].concat()), Some(vec![]));
        // What can't be read: LTO bitcode, other files, a broken object.
        assert_eq!(read_bytes("lto.obj", b"BC\xc0\xde\x35\x14\x00\x00\x05\x00\x00\x00\x62\x0c\x30\x24\x4a\x59\xbe\x66"), None);
        assert_eq!(read_bytes("notes.txt", b"just some text, long enough"), None);
        assert_eq!(read_bytes("short.obj", &obj[..30]), None);
        assert_eq!(read_bytes("y.lib", &archive(&[("lto.obj/", b"BC\xc0\xde\x35\x14\x00\x00\x05\x00\x00\x00\x62\x0c\x30\x24")])), None);
    }
}
