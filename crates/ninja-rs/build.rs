//! Embeds a Windows application manifest declaring long path support in the
//! ninja-rs binary.
//!
//! Even with long paths enabled system-wide, Windows only lifts the 260
//! character `MAX_PATH` limit for processes whose manifest declares
//! `longPathAware`, e.g. for the working directory of the commands ninja-rs
//! starts. Upstream ninja embeds the same setting.
//!
//! The manifest is linked as a resource written here directly, so that no
//! resource compiler or mt.exe is needed (the hermetic LLVM toolchain has no
//! working mt.exe): a COFF object with the `.rsrc` section windres would
//! make, which every Windows linker takes (link.exe and lld-link for MSVC
//! targets, the MinGW ones and LLVM's gnullvm for GNU targets).

use std::path::PathBuf;

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly manifestVersion="1.0" xmlns="urn:schemas-microsoft-com:asm.v1">
  <application>
    <windowsSettings>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
</assembly>
"#;

/// `RT_MANIFEST`, and the resource id the loader reads for an executable.
const RT_MANIFEST: u16 = 24;
const CREATEPROCESS_MANIFEST_RESOURCE_ID: u16 = 1;
/// en-US, as rc and llvm-rc use by default.
const LANG_EN_US: u16 = 0x0409;

/// A COFF object for `machine` whose `.rsrc` section holds the manifest: a
/// resource directory (type, then name, then language), the data entry, and
/// the data, whose address the linker sets (an image-relative relocation of
/// type `addr32nb` against the section).
fn coff_object(machine: u16, addr32nb: u16) -> Vec<u8> {
    let u16le = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_le_bytes());
    let u32le = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    // Directories of one entry each: a table (16 bytes), then the entry.
    let mut rsrc = Vec::new();
    for (id, target) in [(RT_MANIFEST, 0x8000_0000 | 24), (CREATEPROCESS_MANIFEST_RESOURCE_ID, 0x8000_0000 | 48), (LANG_EN_US, 72)] {
        rsrc.extend_from_slice(&[0; 14]);
        u16le(&mut rsrc, 1); // NumberOfIdEntries
        u32le(&mut rsrc, u32::from(id));
        u32le(&mut rsrc, target);
    }
    const DATA_ENTRY: u32 = 72;
    const DATA: u32 = 88;
    u32le(&mut rsrc, DATA); // OffsetToData, relocated
    u32le(&mut rsrc, MANIFEST.len() as u32);
    u32le(&mut rsrc, 0); // CodePage
    u32le(&mut rsrc, 0);
    assert_eq!(rsrc.len() as u32, DATA);
    rsrc.extend_from_slice(MANIFEST.as_bytes());
    while !rsrc.len().is_multiple_of(4) {
        rsrc.push(0);
    }

    const HEADERS: u32 = 20 + 40;
    let relocations = HEADERS + rsrc.len() as u32;
    let mut obj = Vec::new();
    u16le(&mut obj, machine);
    u16le(&mut obj, 1); // NumberOfSections
    u32le(&mut obj, 0); // TimeDateStamp
    u32le(&mut obj, relocations + 10); // PointerToSymbolTable
    u32le(&mut obj, 1); // NumberOfSymbols
    u32le(&mut obj, 0); // SizeOfOptionalHeader, Characteristics
    obj.extend_from_slice(b".rsrc\0\0\0");
    u32le(&mut obj, 0); // VirtualSize
    u32le(&mut obj, 0); // VirtualAddress
    u32le(&mut obj, rsrc.len() as u32);
    u32le(&mut obj, HEADERS); // PointerToRawData
    u32le(&mut obj, relocations);
    u32le(&mut obj, 0); // PointerToLinenumbers
    u16le(&mut obj, 1); // NumberOfRelocations
    u16le(&mut obj, 0); // NumberOfLinenumbers
    // INITIALIZED_DATA | ALIGN_4BYTES | MEM_READ.
    u32le(&mut obj, 0x40 | 0x0030_0000 | 0x4000_0000);
    obj.extend_from_slice(&rsrc);
    u32le(&mut obj, DATA_ENTRY);
    u32le(&mut obj, 0); // the section's symbol
    u16le(&mut obj, addr32nb);
    // The section's symbol: static, no auxiliary record.
    obj.extend_from_slice(b".rsrc\0\0\0");
    u32le(&mut obj, 0); // Value
    u16le(&mut obj, 1); // SectionNumber
    u16le(&mut obj, 0); // Type
    obj.extend_from_slice(&[3, 0]); // StorageClass, NumberOfAuxSymbols
    u32le(&mut obj, 4); // an empty string table
    obj
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" {
        return;
    }
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    // IMAGE_FILE_MACHINE_*, and IMAGE_REL_*_ADDR32NB (DIR32NB on x86).
    let (machine, addr32nb) = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default().as_str() {
        "x86_64" => (0x8664, 3),
        "x86" => (0x14c, 7),
        "aarch64" => (0xaa64, 2),
        other => {
            println!("cargo:warning=no long path manifest for {other}");
            return;
        }
    };
    std::fs::write(out_dir.join("ninja-rs-manifest.o"), coff_object(machine, addr32nb)).expect("writing the manifest object");
    // Found in the library path, not named by its path: Bazel's sandboxed
    // builds move OUT_DIR between the script and the link, and fix up
    // library paths only. GNU linkers look there for `-l:file`; link.exe
    // and lld-link for any input named without a directory.
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    if target_env == "msvc" {
        println!("cargo:rustc-link-arg=ninja-rs-manifest.o");
    } else {
        println!("cargo:rustc-link-arg=-l:ninja-rs-manifest.o");
    }
}
