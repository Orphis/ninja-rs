//! Embeds a Windows application manifest declaring long path support in the
//! ninja-rs binary (MSVC targets).
//!
//! Even with long paths enabled system-wide, Windows only lifts the 260
//! character `MAX_PATH` limit for processes whose manifest declares
//! `longPathAware`, e.g. for the working directory of the commands ninja-rs
//! starts. Upstream ninja embeds the same setting.
//!
//! The manifest is linked as a compiled resource file (`.res`), written here
//! directly: link.exe and lld-link both accept `.res` inputs, so no resource
//! compiler or mt.exe is needed (the hermetic LLVM toolchain has no working
//! mt.exe).

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
/// MOVEABLE | PURE.
const MEMORY_FLAGS: u16 = 0x0030;

/// Appends one resource to a `.res` file: a 32-byte header (numeric type
/// and name) followed by the data, padded to a 4-byte boundary.
fn push_resource(res: &mut Vec<u8>, type_id: u16, name_id: u16, flags: u16, language: u16, data: &[u8]) {
    let data_size = u32::try_from(data.len()).expect("resource too large");
    res.extend_from_slice(&data_size.to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
    for id in [type_id, name_id] {
        res.extend_from_slice(&0xFFFFu16.to_le_bytes());
        res.extend_from_slice(&id.to_le_bytes());
    }
    res.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    res.extend_from_slice(&flags.to_le_bytes());
    res.extend_from_slice(&language.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // Version
    res.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    res.extend_from_slice(data);
    while res.len() % 4 != 0 {
        res.push(0);
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }

    // A .res file starts with an empty resource that marks the format.
    let mut res = Vec::new();
    push_resource(&mut res, 0, 0, 0, 0, &[]);
    push_resource(&mut res, RT_MANIFEST, CREATEPROCESS_MANIFEST_RESOURCE_ID, MEMORY_FLAGS, LANG_EN_US, MANIFEST.as_bytes());

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("ninja-rs-manifest.res");
    std::fs::write(&out, res).expect("writing the manifest resource");
    println!("cargo:rustc-link-arg={}", out.display());
}
