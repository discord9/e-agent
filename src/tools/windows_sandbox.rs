//! Minimal Windows write sandbox: stable capability ACLs plus a restricted
//! primary token. This deliberately does not attempt read or network isolation.

use std::ffi::{OsStr, OsString, c_void};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Globalization::CompareStringOrdinal;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, MAXDWORD, SE_GROUP_LOGON_ID,
};
use windows_sys::Win32::System::Threading::*;

use crate::config::Sandbox;
use crate::tools::background::{OutputSlot, TaskSpool, slot_append};
use crate::workspace::Workspace;

use super::OUTPUT_LIMIT;
use super::bash::{Captured, Shell, format_output};

struct Handle(HANDLE);
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Search handle for `FindFirstFileNameW`/`FindNextFileNameW`. Must be
/// closed with `FindClose`, NOT `CloseHandle`, so the existing [`Handle`]
/// wrapper (whose drop calls `CloseHandle`) is unusable here.
struct FindHandle(HANDLE);
unsafe impl Send for FindHandle {}
impl Drop for FindHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { FindClose(self.0) };
        }
    }
}

struct LocalPtr(*mut c_void);
impl Drop for LocalPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

struct AttributeList(LPPROC_THREAD_ATTRIBUTE_LIST);
impl Drop for AttributeList {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
}

struct Spawned {
    process: Handle,
    stdout: Handle,
    stderr: Handle,
    pid: u32,
}

struct ProcessGuard(Option<Handle>);

fn confirm_duplicate_failure_cleanup(
    terminate: impl FnOnce() -> Result<(), String>,
    wait: impl FnOnce(u32) -> Result<bool, String>,
) -> Result<(), String> {
    match terminate() {
        Ok(()) => {
            if wait(INFINITE)? {
                Ok(())
            } else {
                Err("process termination was not signaled after TerminateProcess succeeded".into())
            }
        }
        Err(termination_error) => match wait(0) {
            Ok(true) => Ok(()), // The process exited before termination was requested.
            Ok(false) => Err(format!(
                "serious cleanup failure: {termination_error}; spawned process is still running"
            )),
            Err(wait_error) => Err(format!(
                "serious cleanup failure: {termination_error}; cannot confirm process exit: {wait_error}"
            )),
        },
    }
}

fn wait_signaled(process: HANDLE, timeout: u32) -> Result<bool, String> {
    match unsafe { WaitForSingleObject(process, timeout) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(last_error("WaitForSingleObject(process cleanup) failed")),
    }
}

impl ProcessGuard {
    fn duplicate(process: Handle) -> Result<(Self, Handle), String> {
        let mut duplicate = null_mut();
        if unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                process.0,
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            let duplicate_error = last_error("DuplicateHandle(process guard) failed");
            // Spawn already succeeded, so fail closed using the original exact
            // process handle. Keep it open until termination has either been
            // confirmed or reported as a serious cleanup failure.
            let cleanup = confirm_duplicate_failure_cleanup(
                || {
                    if unsafe { TerminateProcess(process.0, 1) } != 0 {
                        Ok(())
                    } else {
                        Err(last_error(
                            "TerminateProcess after duplicate failure failed",
                        ))
                    }
                },
                |timeout| wait_signaled(process.0, timeout),
            );
            return match cleanup {
                Ok(()) => Err(duplicate_error),
                Err(cleanup_error) => Err(format!("{duplicate_error}; {cleanup_error}")),
            };
        }
        Ok((Self(Some(Handle(duplicate))), process))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(process) = &self.0 {
            unsafe {
                TerminateProcess(process.0, 1);
            }
        }
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn last_error(context: &str) -> String {
    format!("{context}: {}", std::io::Error::last_os_error())
}

/// Version tag embedded in every capability SID. v4 splits the directory
/// capability into two explicit ACEs — the full capability mask plus an
/// inherit-only DELETE carrier — because FILE_DELETE_CHILD alone does not
/// cover deleting/renaming pre-existing files from the restricted token.
/// A new version makes the one-time ACL upgrade explicit and leaves v3 ACEs
/// inert (they never match the v4 structural check).
const CAPABILITY_VERSION: &[u8] = b"e-agent/windows-write-capability/v4";

fn capability_key(path: &Path, class: &str, version: &[u8]) -> Vec<u8> {
    let path: Vec<u16> = path.as_os_str().encode_wide().collect();
    let class = class.as_bytes();
    let mut key = version.to_vec();
    key.extend_from_slice(&(class.len() as u64).to_le_bytes());
    key.extend_from_slice(class);
    key.extend_from_slice(&(path.len() as u64).to_le_bytes());
    for unit in path {
        key.extend_from_slice(&unit.to_le_bytes());
    }
    key
}

fn stable_sid_key(path: &Path, class: &str) -> Vec<u8> {
    capability_key(path, class, CAPABILITY_VERSION)
}

fn sid_from_key(key: &[u8]) -> String {
    let digest = Sha256::digest(key);
    let mut words = [0u32; 4];
    for (word, bytes) in words.iter_mut().zip(digest.chunks_exact(4)) {
        *word = u32::from_le_bytes(bytes.try_into().expect("four bytes"));
    }
    format!(
        "S-1-5-21-{}-{}-{}-{}",
        words[0], words[1], words[2], words[3]
    )
}

/// Synthetic capability SID for `path`+`class` under the current version.
/// `pub(super)` so the sibling integration tests can verify the installed
/// root ACL against the same SID the sandbox would install.
pub(super) fn stable_sid(path: &Path, class: &str) -> String {
    sid_from_key(&stable_sid_key(path, class))
}

fn string_sid(value: &str) -> Result<LocalPtr, String> {
    let value = wide(OsStr::new(value));
    let mut sid = null_mut();
    if unsafe { ConvertStringSidToSidW(value.as_ptr(), &mut sid) } == 0 {
        return Err(last_error("ConvertStringSidToSidW failed"));
    }
    Ok(LocalPtr(sid))
}

fn everyone_sid() -> Result<Vec<usize>, String> {
    let mut size = SECURITY_MAX_SID_SIZE;
    let words = (size as usize).div_ceil(size_of::<usize>());
    let mut data = vec![0usize; words];
    if unsafe { CreateWellKnownSid(WinWorldSid, null_mut(), data.as_mut_ptr().cast(), &mut size) }
        == 0
    {
        return Err(last_error("CreateWellKnownSid(Everyone) failed"));
    }
    Ok(data)
}

fn token_groups(token: HANDLE) -> Result<Vec<usize>, String> {
    let mut size = 0;
    unsafe { GetTokenInformation(token, TokenGroups, null_mut(), 0, &mut size) };
    if size == 0 {
        return Err(last_error("GetTokenInformation(TokenGroups) size failed"));
    }
    let words = (size as usize).div_ceil(size_of::<usize>());
    let mut data = vec![0usize; words];
    if unsafe {
        GetTokenInformation(
            token,
            TokenGroups,
            data.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(last_error("GetTokenInformation(TokenGroups) failed"));
    }
    Ok(data)
}

unsafe fn find_logon_sid(groups: &[usize]) -> Result<PSID, String> {
    let groups = unsafe { &*groups.as_ptr().cast::<TOKEN_GROUPS>() };
    for index in 0..groups.GroupCount as usize {
        let item = unsafe { *groups.Groups.as_ptr().add(index) };
        if item.Attributes & SE_GROUP_LOGON_ID as u32 == SE_GROUP_LOGON_ID as u32 {
            return Ok(item.Sid);
        }
    }
    Err("current token has no logon SID".into())
}

fn explicit_entry(
    sid: PSID,
    mode: ACCESS_MODE,
    mask: u32,
    inheritance: ACE_FLAGS,
) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
        },
    }
}

const CAPABILITY_MASK: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | FILE_DELETE_CHILD;
const CAPABILITY_INHERITANCE: ACE_FLAGS = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
/// Inheritance flags of the second capability ACE. CONTAINER_INHERIT_ACE |
/// OBJECT_INHERIT_ACE propagate the ACE to every descendant object, and
/// INHERIT_ONLY_ACE keeps it off the root itself: child files receive DELETE
/// as an effective ACE, child directories receive it as an inherit-only
/// carrier that keeps propagating to their own descendants (per the documented
/// ACE inheritance rules). Directories at any depth therefore become deletable
/// — including recursive deletion — while the root itself never receives
/// DELETE.
const DELETE_ACE_INHERITANCE: u8 =
    (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE | INHERIT_ONLY_ACE) as u8;

/// Exact structural check for the current (v4) capability layout on a write
/// root: exactly two non-inherited ACCESS_ALLOWED_ACEs for `sid` — one with
/// the full capability mask and container/object inheritance, one inherit-only
/// DELETE carrier — and no other explicit allow/deny ACE for the same SID.
/// ACE order is deliberately not a matching condition; quantity and content
/// must match exactly. Inherited ACEs (INHERITED_ACE set) can never satisfy
/// the explicit install. `pub(super)` so the sibling integration tests can
/// verify a real installed ACL against the same check.
///
/// # Safety
/// `acl` must point to a valid, readable `ACL` (e.g. one returned by
/// `GetNamedSecurityInfoW` or built by `InitializeAcl`) and `sid` must be a
/// valid SID for the lifetime of the call.
pub(super) unsafe fn v4_ace_layout_matches(acl: *const ACL, sid: PSID) -> bool {
    let count = unsafe { (*acl).AceCount } as u32;
    let mut main = false;
    let mut delete = false;
    for index in 0..count {
        let mut raw = null_mut();
        if unsafe { GetAce(acl, index, &mut raw) } == 0 || raw.is_null() {
            continue;
        }
        let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
        if ace.Header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8
            || ace.Header.AceFlags & INHERITED_ACE as u8 != 0
        {
            continue;
        }
        let ace_sid = (&ace.SidStart as *const u32).cast_mut().cast();
        if unsafe { EqualSid(ace_sid, sid) } == 0 {
            continue;
        }
        if ace.Mask == CAPABILITY_MASK && ace.Header.AceFlags == CAPABILITY_INHERITANCE as u8 {
            if main {
                return false; // duplicate capability ACE
            }
            main = true;
        } else if ace.Mask == DELETE && ace.Header.AceFlags == DELETE_ACE_INHERITANCE {
            if delete {
                return false; // duplicate delete ACE
            }
            delete = true;
        } else {
            return false; // same SID with an unexpected explicit allow/deny
        }
    }
    main && delete
}

fn needs_install(acl: *const ACL, sid: PSID) -> bool {
    !unsafe { v4_ace_layout_matches(acl, sid) }
}

struct RootAcl {
    path: PathBuf,
    old_acl: *mut ACL,
    needs_install: bool,
    _descriptor: LocalPtr,
}

/// Open a descendant file and read both its link count and its volume/file
/// identity. One open keeps the two observations free of a TOCTOU window
/// between them.
fn file_identity(path: &Path) -> Result<(u32, FILE_ID_INFO), String> {
    let path_w = wide(path.as_os_str());
    let handle = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error(&format!(
            "cannot inspect link count for descendant {}",
            path.display()
        )));
    }
    let handle = Handle(handle);
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    if unsafe { GetFileInformationByHandle(handle.0, &mut information) } == 0 {
        return Err(last_error(&format!(
            "cannot inspect link count for descendant {}",
            path.display()
        )));
    }
    let mut identity: FILE_ID_INFO = unsafe { zeroed() };
    if unsafe {
        GetFileInformationByHandleEx(
            handle.0,
            FileIdInfo,
            (&mut identity as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(last_error(&format!(
            "cannot read file identity for descendant {}",
            path.display()
        )));
    }
    Ok((information.nNumberOfLinks, identity))
}

/// Enumerate every hard-link name of `path`. The names are volume-relative
/// (typically starting with `\`); callers join them onto the volume mount
/// point. The buffer grows dynamically on ERROR_MORE_DATA; every
/// FindNextFileNameW call resets the length; only ERROR_HANDLE_EOF ends the
/// enumeration and any other error fails closed.
fn enumerate_hard_link_names(path: &Path) -> Result<Vec<Vec<u16>>, String> {
    let path_w = wide(path.as_os_str());
    let mut buffer: Vec<u16> = vec![0; 256];
    let mut length = buffer.len() as u32;
    let mut search =
        unsafe { FindFirstFileNameW(path_w.as_ptr(), 0, &mut length, buffer.as_mut_ptr()) };
    if search == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
            return Err(last_error(&format!(
                "FindFirstFileNameW failed for {}",
                path.display()
            )));
        }
        buffer.resize(length as usize + 1, 0);
        length = buffer.len() as u32;
        search =
            unsafe { FindFirstFileNameW(path_w.as_ptr(), 0, &mut length, buffer.as_mut_ptr()) };
        if search == INVALID_HANDLE_VALUE {
            return Err(last_error(&format!(
                "FindFirstFileNameW failed for {}",
                path.display()
            )));
        }
    }
    let search = FindHandle(search);
    let mut names = Vec::new();
    loop {
        let mut name = buffer[..(length as usize).min(buffer.len())].to_vec();
        if name.last() == Some(&0) {
            name.pop();
        }
        names.push(name);
        length = buffer.len() as u32;
        let mut found = unsafe { FindNextFileNameW(search.0, &mut length, buffer.as_mut_ptr()) };
        if found == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
                break;
            }
            if error.raw_os_error() == Some(ERROR_MORE_DATA as i32) {
                buffer.resize(length as usize + 1, 0);
                length = buffer.len() as u32;
                found = unsafe { FindNextFileNameW(search.0, &mut length, buffer.as_mut_ptr()) };
                if found == 0 {
                    return Err(last_error(&format!(
                        "FindNextFileNameW failed for {}",
                        path.display()
                    )));
                }
                continue;
            }
            return Err(last_error(&format!(
                "FindNextFileNameW failed for {}",
                path.display()
            )));
        }
    }
    Ok(names)
}

/// Long form of `path` (also resolves short/8.3 components), or a
/// fail-closed error when the path no longer exists. Verbatim `\\?\` and
/// UNC prefixes are normalized to ordinary DOS spelling first so results
/// compare equal with configured roots.
fn long_path(path: &Path) -> Result<PathBuf, String> {
    let path = crate::strip_verbatim_prefix(path);
    let path_w = wide(path.as_os_str());
    let needed = unsafe { GetLongPathNameW(path_w.as_ptr(), null_mut(), 0) };
    if needed == 0 {
        return Err(last_error(&format!(
            "cannot resolve long path for {}",
            path.display()
        )));
    }
    let mut buffer = vec![0u16; needed as usize];
    let written = unsafe { GetLongPathNameW(path_w.as_ptr(), buffer.as_mut_ptr(), needed) };
    if written == 0 {
        return Err(last_error(&format!(
            "cannot resolve long path for {}",
            path.display()
        )));
    }
    buffer.truncate(written as usize);
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

/// Ordinal, Windows case-insensitive component comparison (not a locale
/// collation and never a lossy `to_lowercase`).
fn component_equal_ci(left: &OsStr, right: &OsStr) -> bool {
    let left: Vec<u16> = left.encode_wide().collect();
    let right: Vec<u16> = right.encode_wide().collect();
    const CSTR_EQUAL: i32 = 2;
    unsafe {
        CompareStringOrdinal(
            left.as_ptr(),
            left.len() as i32,
            right.as_ptr(),
            right.len() as i32,
            1, // TRUE: ignore case
        ) == CSTR_EQUAL
    }
}

/// Component-wise containment: `candidate` equals `root` or lies strictly
/// below it, comparing every component with [`component_equal_ci`] so
/// `C:\foo` never matches `C:\foobar`.
fn path_is_within(candidate: &Path, root: &Path) -> bool {
    let candidate: Vec<_> = candidate.components().collect();
    let root: Vec<_> = root.components().collect();
    if candidate.len() < root.len() {
        return false;
    }
    candidate
        .iter()
        .zip(&root)
        .all(|(candidate, root)| component_equal_ci(candidate.as_os_str(), root.as_os_str()))
}

fn hard_link_outside_error(path: &Path, names: &[PathBuf]) -> String {
    let mut message = format!(
        "Windows write-sandbox cannot install a write capability because this hard-linked file has a name outside the configured writable roots.\n\nScanned path:\n  {}\n\nAll hard-link names:\n",
        path.display()
    );
    for name in names {
        message.push_str(&format!("  {}\n", name.display()));
    }
    message.push_str(
        "\nHard links whose every name is inside the configured writable roots are supported.\nRemove the outside hard link, exclude this file by narrowing the writable root, or explicitly authorize the other location in the user-level sandbox configuration.\nTools such as Cargo may create hard links as part of link-or-copy operations.",
    );
    message
}

/// A descendant file with `link_count > 1` is acceptable only when EVERY one
/// of its hard-link names lies inside at least one configured write root.
/// Each enumerated name is rebuilt into a full path (volume mount point +
/// volume-relative name), re-opened and matched against the scanned inode's
/// volume serial + file ID, and only then judged for membership; the original
/// enumeration paths are kept for the error output while the long normalized
/// paths are used for the judgment.
fn verify_hard_links(
    path: &Path,
    link_count: u32,
    identity: &FILE_ID_INFO,
    allowed_roots: &[PathBuf],
) -> Result<(), String> {
    let mut volume = vec![0u16; 32768];
    let path_w = wide(path.as_os_str());
    if unsafe { GetVolumePathNameW(path_w.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) } == 0
    {
        return Err(last_error(&format!(
            "cannot identify volume for hard-linked descendant {}",
            path.display()
        )));
    }
    let volume_end = volume
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(volume.len());
    let volume = PathBuf::from(OsString::from_wide(&volume[..volume_end]));

    let names = enumerate_hard_link_names(path)?;
    if names.len() as u32 != link_count {
        return Err(format!(
            "cannot enumerate/verify all hard-link names for {}: the file reports {link_count} links but only {} names could be enumerated",
            path.display(),
            names.len()
        ));
    }

    let mut full_paths = Vec::with_capacity(names.len());
    for name in &names {
        // Volume-relative names typically start with '\'; strip it so the
        // join is a plain relative append even for mounted-volume roots.
        let relative = if name.first() == Some(&(b'\\' as u16)) {
            &name[1..]
        } else {
            &name[..]
        };
        let mut full = volume.clone();
        full.push(OsString::from_wide(relative));
        let (_, alias_identity) = file_identity(&full).map_err(|error| {
            format!(
                "cannot re-open hard-link name {} while verifying {}: {error}",
                full.display(),
                path.display()
            )
        })?;
        if alias_identity.VolumeSerialNumber != identity.VolumeSerialNumber
            || alias_identity.FileId.Identifier != identity.FileId.Identifier
        {
            return Err(format!(
                "hard-link name {} no longer refers to the same file as {} (volume serial or file ID changed); refusing to start",
                full.display(),
                path.display()
            ));
        }
        full_paths.push(full);
    }

    let roots_long: Vec<PathBuf> = allowed_roots
        .iter()
        .map(|root| long_path(root))
        .collect::<Result<_, _>>()?;
    for full in &full_paths {
        let alias_long = long_path(full)?;
        if !roots_long
            .iter()
            .any(|root| path_is_within(&alias_long, root))
        {
            return Err(hard_link_outside_error(path, &full_paths));
        }
    }
    Ok(())
}

fn scan_descendants(root: &Path, allowed_roots: &[PathBuf]) -> Result<(), String> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|error| {
            format!(
                "cannot read write-root directory {} while scanning descendants: {error}",
                directory.display()
            )
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "cannot enumerate descendants of write-root directory {}: {error}",
                    directory.display()
                )
            })?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                format!(
                    "cannot inspect write-root descendant {}: {error}",
                    path.display()
                )
            })?;
            if metadata.file_type().is_symlink()
                || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(format!(
                    "Windows write-sandbox does not support symlink/reparse-point descendants: {}",
                    path.display()
                ));
            }
            if metadata.is_dir() {
                directories.push(path);
            } else if metadata.is_file() {
                let (link_count, identity) = file_identity(&path)?;
                if link_count > 1 {
                    verify_hard_links(&path, link_count, &identity, allowed_roots)?;
                }
            }
        }
    }
    Ok(())
}

fn preflight_root(path: &Path, sid: PSID) -> Result<RootAcl, String> {
    use std::path::{Component, Prefix};

    if !matches!(path.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_)))
    {
        return Err(format!(
            "Windows write-sandbox supports only canonical local drive paths, not UNC/device paths: {}",
            path.display()
        ));
    }
    let canonical = crate::canonicalize_path(path)
        .map_err(|error| format!("cannot canonicalize write root {}: {error}", path.display()))?;
    if canonical != path {
        return Err(format!(
            "Windows write-sandbox write root is not in its canonical spelling: {}",
            path.display()
        ));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect write root {}: {error}", path.display()))?;
    if !metadata.is_dir() {
        return Err(format!(
            "Windows write-sandbox supports only directory write roots: {}",
            path.display()
        ));
    }
    if metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(format!(
            "Windows write-sandbox does not support a symlink/reparse-point write root: {}",
            path.display()
        ));
    }

    let path_w = wide(path.as_os_str());
    let mut volume = vec![0u16; 32768];
    if unsafe { GetVolumePathNameW(path_w.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) } == 0
    {
        return Err(last_error(&format!(
            "cannot identify volume for write root {}",
            path.display()
        )));
    }
    // GetDriveTypeW's documented DRIVE_FIXED value.
    const DRIVE_FIXED_TYPE: u32 = 3;
    if unsafe { GetDriveTypeW(volume.as_ptr()) } != DRIVE_FIXED_TYPE {
        return Err(format!(
            "Windows write-sandbox supports only fixed local NTFS volumes: {}",
            path.display()
        ));
    }
    let mut filesystem = [0u16; 32];
    if unsafe {
        GetVolumeInformationW(
            volume.as_ptr(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(last_error(&format!(
            "cannot identify filesystem for write root {}",
            path.display()
        )));
    }
    let filesystem = String::from_utf16_lossy(
        &filesystem[..filesystem
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(filesystem.len())],
    );
    if !filesystem.eq_ignore_ascii_case("NTFS") {
        return Err(format!(
            "Windows write-sandbox supports only NTFS write roots (found {filesystem}): {}",
            path.display()
        ));
    }

    let handle = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error(&format!(
            "cannot inspect case-sensitivity for write root {}",
            path.display()
        )));
    }
    let handle = Handle(handle);
    let mut case_info: FILE_CASE_SENSITIVE_INFO = unsafe { zeroed() };
    if unsafe {
        GetFileInformationByHandleEx(
            handle.0,
            FileCaseSensitiveInfo,
            (&mut case_info as *mut FILE_CASE_SENSITIVE_INFO).cast(),
            size_of::<FILE_CASE_SENSITIVE_INFO>() as u32,
        )
    } == 0
    {
        return Err(last_error(&format!(
            "cannot confirm case-sensitivity mode for write root {}",
            path.display()
        )));
    }
    if case_info.Flags & 1 != 0 {
        return Err(format!(
            "Windows write-sandbox does not support case-sensitive write roots: {}",
            path.display()
        ));
    }

    let mut path_w = wide(path.as_os_str());
    let mut old_acl = null_mut();
    let mut descriptor = null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old_acl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "cannot read ACL for {}: win32 error {status}",
            path.display()
        ));
    }
    let descriptor = LocalPtr(descriptor);
    reject_null_dacl(old_acl, path)?;
    let needs_install = needs_install(old_acl, sid);
    Ok(RootAcl {
        path: path.to_path_buf(),
        old_acl,
        needs_install,
        _descriptor: descriptor,
    })
}

fn reject_null_dacl(acl: *mut ACL, path: &Path) -> Result<(), String> {
    if acl.is_null() {
        return Err(format!(
            "Windows write-sandbox refuses NULL DACL write root: {}",
            path.display()
        ));
    }
    Ok(())
}

fn set_path_ace(root: &RootAcl, sid: PSID) -> Result<(), String> {
    if !root.needs_install {
        return Ok(());
    }
    // A directory capability needs two explicit ACEs, and they must be built
    // by hand: SetEntriesInAclW's SET_ACCESS semantics discard any earlier
    // info for the same trustee, so two same-SID entries cannot be relied on
    // to survive (worst case only one ACE remains). The first ACE grants the
    // full capability mask (including FILE_DELETE_CHILD, which propagates
    // through every descendant directory) with container+object inheritance.
    // The second ACE grants DELETE and is
    // CONTAINER_INHERIT_ACE|OBJECT_INHERIT_ACE|INHERIT_ONLY_ACE: child files
    // receive it as an effective ACE and child directories as an inherit-only
    // carrier that keeps propagating, so files and directories at any depth
    // eventually get DELETE (recursive directory removal included) while the
    // root itself never receives it — FILE_DELETE_CHILD on a directory alone
    // does not cover deleting/renaming files or deleting directories from the
    // restricted token.
    let old_acl = root.old_acl;
    let mut kept = Vec::new(); // (raw ACE pointer, ACE size) in original order
    let mut capacity = size_of::<ACL>();
    let count = unsafe { (*old_acl).AceCount } as u32;
    for index in 0..count {
        let mut raw = null_mut();
        if unsafe { GetAce(old_acl, index, &mut raw) } == 0 || raw.is_null() {
            return Err(format!(
                "cannot read existing ACE {index} of {}",
                root.path.display()
            ));
        }
        let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
        let ace_sid = (&ace.SidStart as *const u32).cast_mut().cast();
        if unsafe { EqualSid(ace_sid, sid) } != 0 {
            // Drop every existing ACE for the current capability SID: partial
            // installs, duplicates, or an accidental root-effective DELETE
            // must not survive into the rebuilt ACL.
            continue;
        }
        capacity += ace.Header.AceSize as usize;
        kept.push((raw, ace.Header.AceSize as u32));
    }
    let ace_size =
        size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + unsafe { GetLengthSid(sid) } as usize;
    capacity += 2 * ace_size;

    let mut buffer = vec![0u8; capacity];
    if unsafe { InitializeAcl(buffer.as_mut_ptr().cast(), capacity as u32, ACL_REVISION) } == 0 {
        return Err(last_error(&format!(
            "cannot initialize new ACL for {}",
            root.path.display()
        )));
    }
    for (raw, size) in &kept {
        if unsafe {
            AddAce(
                buffer.as_mut_ptr().cast(),
                ACL_REVISION,
                MAXDWORD,
                *raw,
                *size,
            )
        } == 0
        {
            return Err(last_error(&format!(
                "cannot copy existing ACE into new ACL for {}",
                root.path.display()
            )));
        }
    }
    if unsafe {
        AddAccessAllowedAceEx(
            buffer.as_mut_ptr().cast(),
            ACL_REVISION,
            CAPABILITY_INHERITANCE,
            CAPABILITY_MASK,
            sid,
        )
    } == 0
    {
        return Err(last_error(&format!(
            "cannot add capability ACE for {}",
            root.path.display()
        )));
    }
    if unsafe {
        AddAccessAllowedAceEx(
            buffer.as_mut_ptr().cast(),
            ACL_REVISION,
            DELETE_ACE_INHERITANCE as u32,
            DELETE,
            sid,
        )
    } == 0
    {
        return Err(last_error(&format!(
            "cannot add delete ACE for {}",
            root.path.display()
        )));
    }
    let mut path_w = wide(root.path.as_os_str());
    let status = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            buffer.as_mut_ptr().cast(),
            null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "cannot set ACL for {}: win32 error {status}; earlier capability ACEs may persist but are inert for ordinary tokens",
            root.path.display()
        ));
    }
    drop(buffer); // SetNamedSecurityInfoW consumed the DACL bytes
    verify_installed_acl(&root.path, sid)
}

/// Re-read the root DACL after installation and require the exact two-ACE
/// v4 layout before the sandbox is allowed to start a process.
fn verify_installed_acl(path: &Path, sid: PSID) -> Result<(), String> {
    let mut path_w = wide(path.as_os_str());
    let mut acl = null_mut();
    let mut descriptor = null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "cannot re-read ACL for {} after installation: win32 error {status}",
            path.display()
        ));
    }
    let _descriptor = LocalPtr(descriptor);
    if acl.is_null() || !unsafe { v4_ace_layout_matches(acl, sid) } {
        return Err(format!(
            "installed ACL for {} does not have the exact expected two-ACE layout; refusing to start with a partially installed write capability",
            path.display()
        ));
    }
    Ok(())
}

fn token_default_dacl(token: HANDLE) -> Result<Vec<usize>, String> {
    let mut size = 0;
    unsafe { GetTokenInformation(token, TokenDefaultDacl, null_mut(), 0, &mut size) };
    if size == 0 {
        return Err(last_error(
            "GetTokenInformation(TokenDefaultDacl) size failed",
        ));
    }
    let words = (size as usize).div_ceil(size_of::<usize>());
    let mut data = vec![0usize; words];
    if unsafe {
        GetTokenInformation(
            token,
            TokenDefaultDacl,
            data.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(last_error("GetTokenInformation(TokenDefaultDacl) failed"));
    }
    Ok(data)
}

fn default_dacl_entries(sids: &[PSID]) -> Vec<EXPLICIT_ACCESS_W> {
    sids.iter()
        .map(|sid| explicit_entry(*sid, GRANT_ACCESS, GENERIC_ALL, NO_INHERITANCE))
        .collect()
}

fn build_default_dacl(source: HANDLE, restricted: HANDLE, sids: &[PSID]) -> Result<(), String> {
    // This buffer owns the source ACL and must remain alive through both
    // SetEntriesInAclW and SetTokenInformation.
    let source_data = token_default_dacl(source)?;
    let source_dacl = unsafe { &*source_data.as_ptr().cast::<TOKEN_DEFAULT_DACL>() }.DefaultDacl;
    if source_dacl.is_null() {
        return Err("source token has a NULL default DACL; refusing to replace it".into());
    }
    let mut entries = default_dacl_entries(sids);
    let mut acl = null_mut();
    let status = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_mut_ptr(),
            source_dacl,
            &mut acl,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "cannot merge restricted token default DACL: win32 error {status}"
        ));
    }
    let _acl = LocalPtr(acl.cast());
    let dacl = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
    if unsafe {
        SetTokenInformation(
            restricted,
            TokenDefaultDacl,
            (&dacl as *const TOKEN_DEFAULT_DACL).cast(),
            size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    } == 0
    {
        return Err(last_error("SetTokenInformation(TokenDefaultDacl) failed"));
    }
    drop(source_data);
    Ok(())
}

fn enable_change_notify(token: HANDLE) -> Result<(), String> {
    let mut privilege: TOKEN_PRIVILEGES = unsafe { zeroed() };
    privilege.PrivilegeCount = 1;
    if unsafe {
        LookupPrivilegeValueW(
            null(),
            SE_CHANGE_NOTIFY_NAME,
            &mut privilege.Privileges[0].Luid,
        )
    } == 0
    {
        return Err(last_error(
            "LookupPrivilegeValueW(SeChangeNotifyPrivilege) failed",
        ));
    }
    privilege.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
    unsafe { SetLastError(ERROR_SUCCESS) };
    if unsafe { AdjustTokenPrivileges(token, 0, &privilege, 0, null_mut(), null_mut()) } == 0 {
        return Err(last_error(
            "AdjustTokenPrivileges(SeChangeNotifyPrivilege) failed",
        ));
    }
    if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
        return Err(
            "AdjustTokenPrivileges(SeChangeNotifyPrivilege) failed: privilege was not assigned"
                .into(),
        );
    }
    Ok(())
}

/// Serializes the entire read/scan/install transaction of [`prepare_token`]
/// across concurrent first launches (e.g. a foreground command racing a
/// background task through the same fresh write root). Two installers must
/// never both observe "not installed" and then interleave ACE writes; the
/// second caller re-preflights inside the lock and becomes a no-op. The
/// per-call preflight + ACL read outside the lock is intentionally NOT
/// cached (no "installed root" registry).
static INSTALL_LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();

fn prepare_token(workspace: &Workspace, policy: &Sandbox) -> Result<Handle, String> {
    let lock = INSTALL_LOCK.get_or_init(|| std::sync::Mutex::new(()));
    // Never panic on a poisoned lock (a tool error must not crash the agent
    // loop). Recovery is safe because the install is idempotent and verified
    // by re-reading the root DACL.
    let _guard = match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    prepare_token_locked(workspace, policy)
}

fn prepare_token_locked(workspace: &Workspace, policy: &Sandbox) -> Result<Handle, String> {
    let roots: Vec<(PathBuf, &'static str)> = policy
        .workspace_writable
        .then(|| (workspace.root().to_path_buf(), "workspace"))
        .into_iter()
        .chain(
            policy
                .writable_paths
                .iter()
                .map(|path| (PathBuf::from(path), "extra-write")),
        )
        .collect();

    // Derive the versioned SID first so ACL preflight can determine whether
    // this root actually needs propagation. Keep every root's ACL descriptor
    // alive through installation.
    let mut capability_storage = Vec::new();
    if roots.is_empty() {
        capability_storage.push(string_sid(&stable_sid(workspace.root(), "inert"))?);
    } else {
        for (path, class) in &roots {
            capability_storage.push(string_sid(&stable_sid(path, class))?);
        }
    }
    let capability_sids: Vec<PSID> = capability_storage.iter().map(|sid| sid.0).collect();

    // Finish validation and ACL reads for every root, then scan only roots
    // whose complete inheritable ACE must be installed. All required scans
    // complete before token work or the first ACL write.
    let root_acls: Vec<RootAcl> = roots
        .iter()
        .zip(&capability_sids)
        .map(|((path, _), sid)| preflight_root(path, *sid))
        .collect::<Result<_, _>>()?;
    let allowed_roots: Vec<PathBuf> = roots.iter().map(|(path, _)| path.clone()).collect();
    for root in &root_acls {
        if root.needs_install {
            scan_descendants(&root.path, &allowed_roots)?;
        }
    }

    let mut source = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut source) } == 0 {
        return Err(last_error("OpenProcessToken failed"));
    }
    let source = Handle(source);
    let groups_data = token_groups(source.0)?;
    let logon = unsafe { find_logon_sid(&groups_data)? };
    let everyone_data = everyone_sid()?;
    let everyone = everyone_data.as_ptr() as PSID;

    let mut restricting: Vec<SID_AND_ATTRIBUTES> = capability_sids
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: *sid,
            Attributes: 0,
        })
        .collect();
    // WRITE_RESTRICTED checks restricting SIDs for writes. Keep these existing
    // identities so locations already writable to Everyone/logon remain the
    // documented compatibility exception; do not grant either identity rights.
    restricting.push(SID_AND_ATTRIBUTES {
        Sid: logon,
        Attributes: 0,
    });
    restricting.push(SID_AND_ATTRIBUTES {
        Sid: everyone,
        Attributes: 0,
    });
    let mut restricted = null_mut();
    if unsafe {
        CreateRestrictedToken(
            source.0,
            DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
            0,
            null(),
            0,
            null(),
            restricting.len() as u32,
            restricting.as_ptr(),
            &mut restricted,
        )
    } == 0
    {
        return Err(last_error("CreateRestrictedToken failed"));
    }
    let restricted = Handle(restricted);

    // Preserve the source token's default DACL and grant GENERIC_ALL only to
    // synthetic capability SIDs. Do all token work before the first ACL write.
    build_default_dacl(source.0, restricted.0, &capability_sids)?;
    enable_change_notify(restricted.0)?;

    for (root, sid) in root_acls.iter().zip(&capability_sids) {
        if let Err(error) = set_path_ace(root, *sid) {
            return Err(format!(
                "{error}; earlier synthetic capability ACEs may persist but are inert for ordinary tokens; no process was started"
            ));
        }
    }
    Ok(restricted)
}

fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
        return arg.to_owned();
    }
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in arg.chars() {
        if ch == '\\' {
            slashes += 1;
        } else {
            if ch == '"' {
                result.push_str(&"\\".repeat(slashes * 2 + 1));
            } else {
                result.push_str(&"\\".repeat(slashes));
            }
            slashes = 0;
            result.push(ch);
        }
    }
    result.push_str(&"\\".repeat(slashes * 2));
    result.push('"');
    result
}

fn environment_block() -> Vec<u16> {
    const SECRET_KEYS: &[&str] = &[
        "EXA_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "DEEPSEEK_API_KEY",
        "MOONSHOT_API_KEY",
        "KIMI_API_KEY",
    ];
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| {
            !SECRET_KEYS
                .iter()
                .any(|secret| key.eq_ignore_ascii_case(secret))
        })
        .collect();
    env.retain(|(key, _)| !key.eq_ignore_ascii_case("LC_ALL") && !key.eq_ignore_ascii_case("LANG"));
    env.push(("LC_ALL".into(), "C.UTF-8".into()));
    env.push(("LANG".into(), "C.UTF-8".into()));
    env.sort_by_key(|a| a.0.to_lowercase());
    let mut block = Vec::new();
    for (key, value) in env {
        block.extend(OsStr::new(&format!("{key}={value}")).encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

fn duplicate_stdin() -> Result<Handle, String> {
    let stdin = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if stdin.is_null() || stdin == INVALID_HANDLE_VALUE {
        return Err(last_error("GetStdHandle(stdin) failed"));
    }
    let mut duplicate = null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            stdin,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(last_error("DuplicateHandle(stdin) failed"));
    }
    Ok(Handle(duplicate))
}

fn pipe() -> Result<(Handle, Handle), String> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let (mut read, mut write) = (null_mut(), null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
        return Err(last_error("CreatePipe failed"));
    }
    let read = Handle(read);
    let write = Handle(write);
    if unsafe { SetHandleInformation(read.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(last_error("SetHandleInformation(pipe) failed"));
    }
    Ok((read, write))
}

fn spawn(
    shell: &Shell,
    workspace: &Workspace,
    command: &str,
    policy: &Sandbox,
) -> Result<Spawned, String> {
    let token = prepare_token(workspace, policy)?;
    let stdin = duplicate_stdin()?;
    let (stdout_read, stdout_write) = pipe()?;
    let (stderr_read, stderr_write) = pipe()?;
    let mut handles = [stdin.0, stdout_write.0, stderr_write.0];

    let mut attribute_size = 0usize;
    unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attribute_size) };
    if attribute_size == 0 {
        return Err(last_error("InitializeProcThreadAttributeList size failed"));
    }
    let mut attribute_storage = vec![0u8; attribute_size];
    let attribute_ptr = attribute_storage.as_mut_ptr().cast();
    if unsafe { InitializeProcThreadAttributeList(attribute_ptr, 1, 0, &mut attribute_size) } == 0 {
        return Err(last_error("InitializeProcThreadAttributeList failed"));
    }
    let attributes = AttributeList(attribute_ptr);
    if unsafe {
        UpdateProcThreadAttribute(
            attributes.0,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_mut_ptr().cast(),
            size_of_val(&handles),
            null_mut(),
            null(),
        )
    } == 0
    {
        return Err(last_error("UpdateProcThreadAttribute(handle list) failed"));
    }

    let args = shell.command_args(command);
    let mut line = quote_arg(&shell.executable);
    for arg in &args {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    let mut line_w = wide(OsStr::new(&line));
    let app_w = wide(OsStr::new(&shell.executable));
    let cwd_w = wide(workspace.root().as_os_str());
    let mut desktop = wide(OsStr::new("Winsta0\\Default"));
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.lpDesktop = desktop.as_mut_ptr();
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin.0;
    startup.StartupInfo.hStdOutput = stdout_write.0;
    startup.StartupInfo.hStdError = stderr_write.0;
    startup.lpAttributeList = attributes.0;
    let environment = environment_block();
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    if unsafe {
        CreateProcessAsUserW(
            token.0,
            app_w.as_ptr(),
            line_w.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            cwd_w.as_ptr(),
            (&startup as *const STARTUPINFOEXW).cast(),
            &mut info,
        )
    } == 0
    {
        return Err(last_error("CreateProcessAsUserW(restricted token) failed"));
    }
    let process = Handle(info.hProcess);
    let thread = Handle(info.hThread);
    // Parent must close its pipe writers immediately so EOF is observable.
    drop(stdout_write);
    drop(stderr_write);
    drop(thread);
    Ok(Spawned {
        process,
        stdout: stdout_read,
        stderr: stderr_read,
        pid: info.dwProcessId,
    })
}

fn read_pipe(
    handle: Handle,
    slot: Option<OutputSlot>,
    spool: Option<Arc<TaskSpool>>,
) -> std::io::Result<Captured> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let mut count = 0u32;
        if unsafe {
            ReadFile(
                handle.0,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut count,
                null_mut(),
            )
        } == 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(Captured { bytes, truncated });
            }
            return Err(error);
        }
        if count == 0 {
            return Ok(Captured { bytes, truncated });
        }
        let data = &buffer[..count as usize];
        if let Some(slot) = &slot {
            slot_append(slot, data);
        }
        if let Some(spool) = &spool {
            spool.append(data);
        }
        let room = OUTPUT_LIMIT.saturating_sub(bytes.len());
        bytes.extend_from_slice(&data[..data.len().min(room)]);
        truncated |= data.len() > room;
    }
}

fn wait_process(process: Handle) -> std::io::Result<i32> {
    if unsafe { WaitForSingleObject(process.0, INFINITE) } != WAIT_OBJECT_0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(code as i32)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    shell: &Shell,
    workspace: &Workspace,
    command: &str,
    timeout: Option<Duration>,
    protect_git: bool,
    process_group_slot: Option<Arc<AtomicI32>>,
    output_slot: Option<OutputSlot>,
    spool: Option<Arc<TaskSpool>>,
    policy: &Sandbox,
) -> Result<String, String> {
    if protect_git {
        return Err(
            "Windows write-sandbox: this role sets protect_git=true, which the Windows MVP cannot enforce (there is no .git read-only bind, so running shell commands would silently leave .git writable). Add 'protect_git = false' to the role's frontmatter (e.g. the fixer role template), or disable the sandbox, to run shell commands."
                .into(),
        );
    }
    if !policy.network {
        return Err("Windows write-sandbox MVP does not implement network isolation".into());
    }
    let spawned = spawn(shell, workspace, command, policy)?;
    if let Some(slot) = &process_group_slot {
        slot.store(spawned.pid as i32, Ordering::Release);
    }
    // Duplicate the process handle before handing the original to the blocking
    // waiter. This guard remains valid until completion and kills the exact
    // process (never a later process that reused its PID) on future drop/timeout.
    let (mut guard, process) = ProcessGuard::duplicate(spawned.process)?;
    let stdout_task = tokio::task::spawn_blocking({
        let slot = output_slot.clone();
        let spool = spool.clone();
        move || read_pipe(spawned.stdout, slot, spool)
    });
    let stderr_task =
        tokio::task::spawn_blocking(move || read_pipe(spawned.stderr, output_slot, spool));
    let wait_task = tokio::task::spawn_blocking(move || wait_process(process));
    let joined = async {
        let (stdout, stderr, code) = tokio::join!(stdout_task, stderr_task, wait_task);
        Ok::<_, String>((
            stdout
                .map_err(|e| format!("stdout worker failed: {e}"))?
                .map_err(|e| format!("stdout read failed: {e}"))?,
            stderr
                .map_err(|e| format!("stderr worker failed: {e}"))?
                .map_err(|e| format!("stderr read failed: {e}"))?,
            code.map_err(|e| format!("wait worker failed: {e}"))?
                .map_err(|e| format!("wait failed: {e}"))?,
        ))
    };
    let result = match timeout {
        Some(duration) => match tokio::time::timeout(duration, joined).await {
            Ok(result) => result,
            Err(_) => {
                if let Some(slot) = &process_group_slot {
                    slot.store(0, Ordering::Release);
                }
                return Err(format!(
                    "exit code: signal\nstdout:\n\nstderr:\n\n[command timed out after {} seconds]",
                    duration.as_secs_f64()
                ));
            }
        },
        None => joined.await,
    };
    let (stdout, stderr, code) = result?;
    guard.disarm();
    if let Some(slot) = &process_group_slot {
        slot.store(0, Ordering::Release);
    }
    let text = format_output(Some(code), &stdout, &stderr);
    if code == 0 { Ok(text) } else { Err(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_capabilities_use_utf16_path_and_class() {
        use std::os::windows::ffi::OsStringExt;

        let path = Path::new(r"C:\work\游戏");
        assert_eq!(stable_sid(path, "workspace"), stable_sid(path, "workspace"));
        assert_ne!(
            stable_sid(path, "workspace"),
            stable_sid(path, "extra-write")
        );
        assert_ne!(
            stable_sid(path, "workspace"),
            stable_sid(Path::new(r"C:\work\other"), "workspace")
        );
        assert_ne!(
            stable_sid(Path::new(r"C:\work\Game"), "workspace"),
            stable_sid(Path::new(r"C:\work\game"), "workspace")
        );
        let unpaired =
            std::ffi::OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0xd800]);
        assert_ne!(
            stable_sid(Path::new(&unpaired), "workspace"),
            stable_sid(Path::new(r"C:\�"), "workspace")
        );
    }

    #[test]
    fn null_dacl_is_rejected_by_helper() {
        let error = reject_null_dacl(null_mut(), Path::new(r"C:\root")).unwrap_err();
        assert!(error.contains("NULL DACL"), "{error}");
    }

    #[test]
    fn default_dacl_helper_adds_only_supplied_capabilities() {
        let first = string_sid("S-1-5-21-1-2-3-4").unwrap();
        let second = string_sid("S-1-5-21-5-6-7-8").unwrap();
        let supplied = [first.0, second.0];
        let entries = default_dacl_entries(&supplied);
        assert_eq!(entries.len(), supplied.len());
        assert!(entries.iter().zip(supplied).all(|(entry, sid)| {
            entry.grfAccessPermissions == GENERIC_ALL && entry.Trustee.ptstrName == sid.cast()
        }));
    }

    /// Builds a DACL exactly the way the sandbox does (InitializeAcl +
    /// AddAccessAllowedAceEx, not SetEntriesInAclW, whose same-trustee
    /// SET_ACCESS merge behavior is unreliable). `entries` is (mask, flags).
    struct TestAcl(Vec<u8>);
    impl TestAcl {
        fn build(sid: PSID, entries: &[(u32, u32)]) -> TestAcl {
            let ace_size = size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>()
                + unsafe { GetLengthSid(sid) } as usize;
            let mut buffer = vec![0u8; size_of::<ACL>() + entries.len() * ace_size];
            assert_ne!(
                unsafe {
                    InitializeAcl(
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        ACL_REVISION,
                    )
                },
                0
            );
            for (mask, flags) in entries {
                assert_ne!(
                    unsafe {
                        AddAccessAllowedAceEx(
                            buffer.as_mut_ptr().cast(),
                            ACL_REVISION,
                            *flags,
                            *mask,
                            sid,
                        )
                    },
                    0
                );
            }
            TestAcl(buffer)
        }
        fn acl(&self) -> *const ACL {
            self.0.as_ptr().cast()
        }
    }

    #[test]
    fn v4_sid_is_stable_and_differs_from_v3() {
        let path = Path::new(r"C:\work\游戏");
        assert_eq!(stable_sid(path, "workspace"), stable_sid(path, "workspace"));
        let v3 = sid_from_key(&capability_key(
            path,
            "workspace",
            b"e-agent/windows-write-capability/v3",
        ));
        assert_ne!(
            v3,
            stable_sid(path, "workspace"),
            "v4 must produce a SID distinct from v3 so v3 ACEs stay inert"
        );
    }

    #[test]
    fn v4_layout_requires_exactly_two_matching_aces_in_any_order() {
        let sid = string_sid("S-1-5-21-1-2-3-4").unwrap();
        let main = (CAPABILITY_MASK, CAPABILITY_INHERITANCE);
        let delete = (DELETE, DELETE_ACE_INHERITANCE as u32);

        // Both ACEs present, main first: installed.
        let acl = TestAcl::build(sid.0, &[main, delete]);
        assert!(unsafe { v4_ace_layout_matches(acl.acl(), sid.0) });

        // ACE order must not matter.
        let acl = TestAcl::build(sid.0, &[delete, main]);
        assert!(unsafe { v4_ace_layout_matches(acl.acl(), sid.0) });

        // A single ACE (either one) is not installed.
        let acl = TestAcl::build(sid.0, &[main]);
        assert!(!unsafe { v4_ace_layout_matches(acl.acl(), sid.0) });
        let acl = TestAcl::build(sid.0, &[delete]);
        assert!(!unsafe { v4_ace_layout_matches(acl.acl(), sid.0) });
        let acl = TestAcl::build(sid.0, &[]);
        assert!(!unsafe { v4_ace_layout_matches(acl.acl(), sid.0) });

        // Wrong DELETE flags: missing INHERIT_ONLY would make DELETE
        // root-effective, which the model forbids.
        let root_effective_delete = (DELETE, OBJECT_INHERIT_ACE);
        let acl = TestAcl::build(sid.0, &[main, root_effective_delete]);
        assert!(!unsafe { v4_ace_layout_matches(acl.acl(), sid.0) });

        // Wrong DELETE flags: the 0.1.1 layout (no CONTAINER_INHERIT) never
        // propagated DELETE to directories, so directory deletion stayed
        // Access denied; the current layout must require container inherit so
        // child directories carry DELETE onward.
        let files_only_delete = (DELETE, OBJECT_INHERIT_ACE | INHERIT_ONLY_ACE);
        let acl = TestAcl::build(sid.0, &[main, files_only_delete]);
        assert!(!unsafe { v4_ace_layout_matches(acl.acl(), sid.0) });

        // Wrong main mask: a legacy capability ACE without FILE_DELETE_CHILD.
        let legacy = (CAPABILITY_MASK & !FILE_DELETE_CHILD, CAPABILITY_INHERITANCE);
        let acl = TestAcl::build(sid.0, &[legacy, delete]);
        assert!(!unsafe { v4_ace_layout_matches(acl.acl(), sid.0) });

        // Duplicates are rejected.
        let acl = TestAcl::build(sid.0, &[main, main, delete]);
        assert!(!unsafe { v4_ace_layout_matches(acl.acl(), sid.0) });
        let acl = TestAcl::build(sid.0, &[main, delete, delete]);
        assert!(!unsafe { v4_ace_layout_matches(acl.acl(), sid.0) });

        // Any other explicit ACE for the same SID (stray allow or deny)
        // invalidates the layout.
        let stray = (FILE_READ_DATA, 0);
        let acl = TestAcl::build(sid.0, &[main, delete, stray]);
        assert!(!unsafe { v4_ace_layout_matches(acl.acl(), sid.0) });
    }

    #[test]
    fn inherited_aces_cannot_satisfy_root_install() {
        let sid = string_sid("S-1-5-21-1-2-3-4").unwrap();
        let main = (CAPABILITY_MASK, CAPABILITY_INHERITANCE);
        let delete = (DELETE, DELETE_ACE_INHERITANCE as u32);

        // Even the complete two-ACE layout fails once the ACEs are inherited:
        // only an explicit root install counts.
        let acl = TestAcl::build(sid.0, &[main, delete]);
        unsafe {
            for index in 0..2 {
                let mut raw = null_mut();
                assert_ne!(GetAce(acl.acl(), index, &mut raw), 0);
                let ace = &mut *raw.cast::<ACCESS_ALLOWED_ACE>();
                ace.Header.AceFlags |= INHERITED_ACE as u8;
            }
        }
        assert!(needs_install(acl.acl(), sid.0));
    }

    #[test]
    fn duplicate_failure_cleanup_waits_for_confirmed_termination() {
        let waited = std::cell::Cell::new(false);
        confirm_duplicate_failure_cleanup(
            || Ok(()),
            |timeout| {
                assert_eq!(timeout, INFINITE);
                waited.set(true);
                Ok(true)
            },
        )
        .unwrap();
        assert!(waited.get());
    }

    #[test]
    fn duplicate_failure_cleanup_reports_live_process_after_terminate_failure() {
        let error = confirm_duplicate_failure_cleanup(
            || Err("injected termination failure".into()),
            |timeout| {
                assert_eq!(timeout, 0);
                Ok(false)
            },
        )
        .unwrap_err();
        assert!(error.contains("termination failure"), "{error}");
        assert!(error.contains("still running"), "{error}");
    }

    #[test]
    fn quoting_matches_command_line_to_argv_rules() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("two words"), "\"two words\"");
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quote_arg(r"ends with \"), r#""ends with \\""#);
    }
}
