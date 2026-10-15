//! Windows: file identities and times from a handle, and who owns files
//! and processes (the daemon's directory and pipe, the configuration
//! files a build trusts).

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, ERROR_LOCK_VIOLATION, FILETIME, HANDLE, INVALID_HANDLE_VALUE, LocalFree, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce, GetTokenInformation, INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION, PSID, TOKEN_QUERY, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileBasicInfo, FileIdInfo, FileStandardInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, OPEN_EXISTING, READ_CONTROL, UnlockFile,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_ALLOWED_OBJECT_ACE_TYPE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    WaitForSingleObject,
};

/// The SID of everyone.
pub const EVERYONE: &str = "S-1-1-0";

/// A NUL-terminated wide string, as the `W` functions take paths and names.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain([0]).collect()
}

/// Opens a file or directory (symlinks followed) for its attributes only:
/// no sharing conflict with writers, nor with a directory in use.
pub fn open_attributes(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new().access_mode(FILE_READ_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)
}

/// A file's change and creation times, in 100 ns.
pub(crate) fn times(f: &File) -> Option<(u64, u64)> {
    let mut info = FILE_BASIC_INFO::default();
    // SAFETY: an open handle, and a buffer writable for its size.
    let ok = unsafe { GetFileInformationByHandleEx(f.as_raw_handle(), FileBasicInfo, (&raw mut info).cast(), size_of::<FILE_BASIC_INFO>() as u32) };
    (ok != 0).then_some((info.ChangeTime as u64, info.CreationTime as u64))
}

/// A file's identity: its volume's serial number and its file ID.
pub fn file_id(f: &File) -> Option<(u64, u128)> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: an open handle, and a buffer writable for its size.
    let ok = unsafe { GetFileInformationByHandleEx(f.as_raw_handle(), FileIdInfo, (&raw mut info).cast(), size_of::<FILE_ID_INFO>() as u32) };
    (ok != 0).then(|| (info.VolumeSerialNumber, u128::from_le_bytes(info.FileId.Identifier)))
}

/// A file's identity from its 64-bit file index, which every file system
/// has.
pub(crate) fn file_index(f: &File) -> io::Result<(u64, u128)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: an open handle, and a buffer writable for its size.
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(info.dwVolumeSerialNumber), (u128::from(info.nFileIndexHigh) << 32) | u128::from(info.nFileIndexLow)))
}

/// Whether the file a handle is open on is deleted (or no name links to
/// it any more), as the file another one was renamed over is.
pub(crate) fn deleted(f: &File) -> bool {
    let mut info = FILE_STANDARD_INFO::default();
    // SAFETY: an open handle, and a buffer writable for its size.
    let ok = unsafe { GetFileInformationByHandleEx(f.as_raw_handle(), FileStandardInfo, (&raw mut info).cast(), size_of::<FILE_STANDARD_INFO>() as u32) };
    ok != 0 && (info.DeletePending || info.NumberOfLinks == 0)
}

/// The byte [`lock`] locks: past any data a locked file will hold.
const LOCKED_BYTE: u64 = 1 << 62;

/// See `crate::lock`.
pub(crate) fn lock(f: &File, wait: bool) -> io::Result<bool> {
    let mut overlapped = OVERLAPPED::default();
    overlapped.Anonymous.Anonymous.Offset = LOCKED_BYTE as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (LOCKED_BYTE >> 32) as u32;
    let flags = LOCKFILE_EXCLUSIVE_LOCK | if wait { 0 } else { LOCKFILE_FAIL_IMMEDIATELY };
    // SAFETY: an open handle, for synchronous I/O (std's): the call
    // completes before `overlapped` goes away.
    if unsafe { LockFileEx(f.as_raw_handle(), flags, 0, 1, 0, &mut overlapped) } != 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if !wait && e.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) { Ok(false) } else { Err(e) }
}

/// See `crate::unlock`.
pub(crate) fn unlock(f: &File) -> io::Result<()> {
    // SAFETY: an open handle.
    if unsafe { UnlockFile(f.as_raw_handle(), LOCKED_BYTE as u32, (LOCKED_BYTE >> 32) as u32, 1, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// See `crate::process_info`: the parent from `NtQueryInformationProcess` (a
/// snapshot of every process, Toolhelp's, takes tens of milliseconds), the
/// start from `GetProcessTimes`.
pub(crate) fn process_info(pid: u32) -> Option<crate::ProcessInfo> {
    // SAFETY: plain system calls; the handle is owned (closed) below, and
    // the buffers are writable for the sizes given.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let h = OwnedHandle::from_raw_handle(h);
        let mut info = PROCESS_BASIC_INFORMATION::default();
        let size = size_of::<PROCESS_BASIC_INFORMATION>() as u32;
        if NtQueryInformationProcess(h.as_raw_handle(), ProcessBasicInformation, (&raw mut info).cast(), size, &mut 0) < 0 {
            return None;
        }
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        if GetProcessTimes(h.as_raw_handle(), &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
            return None;
        }
        let start = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        let parent = u32::try_from(info.InheritedFromUniqueProcessId).ok()?;
        Some(crate::ProcessInfo { identity: crate::ProcessIdentity { start, namespace: 0 }, parent })
    }
}

/// See `crate::process_running`.
pub(crate) fn process_running(pid: u32) -> Option<bool> {
    // SAFETY: plain system calls; the handle is owned (closed) below.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            // No such process.
            return (io::Error::last_os_error().raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32)).then_some(false);
        }
        let h = OwnedHandle::from_raw_handle(h);
        Some(WaitForSingleObject(h.as_raw_handle(), 0) == WAIT_TIMEOUT)
    }
}

/// The SID of the user `process` runs as, as a string (`S-1-5-21-...`).
pub fn process_user(process: BorrowedHandle<'_>) -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: valid out-parameter; the token is closed below.
    if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // TOKEN_USER: a pointer to the SID (in the buffer), then attributes.
    let mut buf = vec![0u64; 64];
    let mut len = 0;
    // SAFETY: the buffer is large enough for a TOKEN_USER and its SID.
    let ok = unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut len) };
    // SAFETY: opened above.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the SID points into `buf`, alive here.
    unsafe { sid_string(buf[0] as usize as PSID) }
}

/// The SID of the user this process runs as.
pub fn current_user() -> io::Result<String> {
    // SAFETY: the pseudo-handle of this process, always valid.
    process_user(unsafe { BorrowedHandle::borrow_raw(GetCurrentProcess()) })
}

/// `S-1-5-...`.
///
/// # Safety
/// `sid` is a valid SID.
unsafe fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: a valid SID; the string is freed below.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a NUL-terminated string from ConvertSidToStringSidW.
    let s = unsafe {
        let n = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, n))
    };
    // SAFETY: allocated by ConvertSidToStringSidW.
    unsafe { LocalFree(text.cast()) };
    Ok(s)
}

/// Who owns `path` (or what it links to, `follow`), and whom its DACL
/// allows rights that `wanted` accepts (given an entry's access mask):
/// everyone without a DACL, or for an entry whose SID isn't read.
pub fn file_security(path: &Path, follow: bool, wanted: impl Fn(u32) -> bool) -> io::Result<(String, Vec<String>)> {
    let wide = wide(path);
    let flags = FILE_FLAG_BACKUP_SEMANTICS | if follow { 0 } else { FILE_FLAG_OPEN_REPARSE_POINT };
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    // SAFETY: `wide` is NUL-terminated; the handle is closed below.
    let h = unsafe { CreateFileW(wide.as_ptr(), READ_CONTROL, share, std::ptr::null(), OPEN_EXISTING, flags, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let (mut owner, mut dacl, mut descriptor) = (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: an open handle and valid out-parameters; the descriptor,
    // which `owner` and `dacl` point into, is freed below.
    let status = unsafe {
        GetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    // SAFETY: opened above.
    unsafe { CloseHandle(h) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    // SAFETY: `owner` and `dacl` point into the descriptor, alive until
    // freed below.
    let result = unsafe { parse_security(owner, dacl, wanted) };
    // SAFETY: allocated by GetSecurityInfo.
    unsafe { LocalFree(descriptor) };
    result
}

/// # Safety
/// `owner` is a SID or null, `dacl` an ACL or null, alive during the
/// call.
unsafe fn parse_security(owner: PSID, dacl: *mut ACL, wanted: impl Fn(u32) -> bool) -> io::Result<(String, Vec<String>)> {
    // No owner: nobody vouches for it.
    // SAFETY: as the caller promises.
    let owner = if owner.is_null() { EVERYONE.to_string() } else { unsafe { sid_string(owner)? } };
    if dacl.is_null() {
        // No DACL: everyone has every right.
        return Ok((owner, vec![EVERYONE.into()]));
    }
    let mut allowed = Vec::new();
    // SAFETY: a valid ACL.
    let count = unsafe { (*dacl).AceCount };
    for i in 0..count {
        let mut ace = std::ptr::null_mut();
        // SAFETY: `i` is below the ACL's count.
        if unsafe { GetAce(dacl, u32::from(i), &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: an ACE starts with its header.
        let header = unsafe { ace.cast::<ACE_HEADER>().read_unaligned() };
        // Denied entries only take rights away; inherit-only ones are for
        // what the directory will hold.
        let kind = u32::from(header.AceType);
        let object = matches!(kind, ACCESS_ALLOWED_OBJECT_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE);
        if !(object || matches!(kind, ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE)) || u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        // SAFETY: the allowed kinds go on with the access mask, then (but
        // object ACEs) the SID.
        let mask = unsafe { ace.cast::<u8>().add(4).cast::<u32>().read_unaligned() };
        if !wanted(mask) {
            continue;
        }
        // Object entries aren't on files: their SID isn't read, whoever
        // it is.
        // SAFETY: as above.
        allowed.push(if object { EVERYONE.into() } else { unsafe { sid_string(ace.cast::<u8>().add(8).cast())? } });
    }
    Ok((owner, allowed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file this user made is owned by them, and the DACL names them.
    #[test]
    fn own_files() {
        let dir = test_dirs::TestDir::new("ninja_os_security");
        let f = dir.join("f");
        std::fs::write(&f, "x").unwrap();
        let user = current_user().unwrap();
        assert!(user.starts_with("S-1-5-"), "{user}");
        let (owner, allowed) = file_security(&f, true, |_| true).unwrap();
        // Or the Administrators group, for an elevated process.
        assert!(owner == user || owner == "S-1-5-32-544", "{owner}");
        assert!(!allowed.is_empty());
        assert!(file_security(&f, true, |_| false).unwrap().1.is_empty());
        assert!(file_security(&dir.join("missing"), true, |_| true).is_err());
    }

    #[test]
    fn identities() {
        let dir = test_dirs::TestDir::new("ninja_os_ids");
        std::fs::write(dir.join("a"), "a").unwrap();
        std::fs::write(dir.join("b"), "b").unwrap();
        let id = |p: &Path| file_id(&open_attributes(p).unwrap()).unwrap();
        assert_ne!(id(&dir.join("a")), id(&dir.join("b")));
        assert_eq!(id(&dir.join("a")), id(&dir.join("a")));
        assert_eq!(id(&dir.join("a")).0, id(&dir).0, "one volume");
    }
}
