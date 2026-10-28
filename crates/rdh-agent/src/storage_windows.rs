//! Experimental private Windows configuration reads and exclusive creation.
//!
//! Replacement remains unsupported. These helpers and experimental profile
//! locking are disabled by default and require native Windows validation.
use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::{offset_of, size_of, size_of_val};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, Prefix};
use std::ptr::{null, null_mut};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, FILE_WRITE_THROUGH, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    LocalFree, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, CopySid, CreateWellKnownSid,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetLengthSid,
    GetSecurityDescriptorControl, GetTokenInformation, INHERIT_ONLY_ACE, IsValidAcl, IsValidSid,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, TokenUser, WELL_KNOWN_SID_TYPE,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, DELETE, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_RENAME_INFO, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, FileDispositionInfo,
    FileRenameInfo, GetDriveTypeW, GetFileInformationByHandle, GetFileType,
    GetVolumeInformationByHandleW, OPEN_EXISTING, READ_CONTROL, SYNCHRONIZE,
    SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, FILE_PERSISTENT_ACLS};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOVABLE};

use super::MAX_PRIVATE_FILE_BYTES;
use crate::error::{Error, Result};

fn unsafe_storage() -> Error {
    Error::message("Windows private storage identity, ACL, or path cannot be verified")
}

fn last_error() -> Error {
    io::Error::last_os_error().into()
}

/// An owned SID, with alignment suitable for the Windows SID APIs.
struct Sid(Vec<u32>);

impl Sid {
    fn as_ptr(&self) -> PSID {
        // The APIs using this pointer only inspect the SID.
        self.0.as_ptr().cast_mut().cast()
    }

    fn current() -> Result<Self> {
        let mut raw: HANDLE = null_mut();
        // SAFETY: GetCurrentProcess is a pseudo-handle and raw is a writable output.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
            return Err(last_error());
        }
        // SAFETY: OpenProcessToken returned a uniquely owned real handle.
        let token = unsafe { File::from_raw_handle(raw) };
        let mut bytes = 0u32;
        // The first call obtains the required buffer size.
        unsafe {
            GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut bytes);
        }
        if bytes < size_of::<TOKEN_USER>() as u32 || bytes > 64 * 1024 {
            return Err(unsafe_storage());
        }
        let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        // SAFETY: buffer is aligned and at least bytes long, and token is live.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        } == 0
        {
            return Err(last_error());
        }
        // SAFETY: a successful TokenUser query initialized the TOKEN_USER.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        if user.User.Sid.is_null() || unsafe { IsValidSid(user.User.Sid) } == 0 {
            return Err(unsafe_storage());
        }
        // SAFETY: the token query returned a valid SID in buffer.
        let length = unsafe { GetLengthSid(user.User.Sid) };
        if length == 0 || length > SECURITY_MAX_SID_SIZE {
            return Err(unsafe_storage());
        }
        let mut sid = vec![0u32; (length as usize).div_ceil(size_of::<u32>())];
        // SAFETY: sid is aligned, writable, and at least length bytes long.
        if unsafe { CopySid(length, sid.as_mut_ptr().cast(), user.User.Sid) } == 0 {
            return Err(last_error());
        }
        Ok(Self(sid))
    }

    fn well_known(kind: WELL_KNOWN_SID_TYPE) -> Result<Self> {
        let mut sid = vec![0u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<u32>())];
        let mut length = size_of_val(sid.as_slice()) as u32;
        // SAFETY: sid is an aligned writable buffer of length bytes.
        if unsafe { CreateWellKnownSid(kind, null_mut(), sid.as_mut_ptr().cast(), &mut length) }
            == 0
        {
            return Err(last_error());
        }
        Ok(Self(sid))
    }
}

struct Identity {
    user: Sid,
    system: Sid,
    administrators: Sid,
}

impl Identity {
    fn current() -> Result<Self> {
        Ok(Self {
            user: Sid::current()?,
            system: Sid::well_known(WinLocalSystemSid)?,
            administrators: Sid::well_known(WinBuiltinAdministratorsSid)?,
        })
    }
}

struct LocalMemory(PSECURITY_DESCRIPTOR);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the security API allocated this block with LocalAlloc.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// Use the same protected, user-only ACL as the legacy Agent's new objects.
/// The descriptor is attached at creation, before any secret is written.
fn private_descriptor(directory: bool) -> Result<LocalMemory> {
    let user = Sid::current()?;
    let mut raw = null_mut();
    // SAFETY: user is a valid live SID and raw is a writable output.
    if unsafe { ConvertSidToStringSidW(user.as_ptr(), &mut raw) } == 0 || raw.is_null() {
        return Err(last_error());
    }
    let string_memory = LocalMemory(raw.cast());
    let mut length = 0;
    // ConvertSidToStringSidW returns a NUL-terminated textual SID. Its maximum
    // representation is smaller than this conservative bound.
    while length < 256 && unsafe { *raw.add(length) } != 0 {
        length += 1;
    }
    if length == 256 {
        return Err(unsafe_storage());
    }
    // SAFETY: the preceding bounded scan found the end of this live string.
    let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(raw, length) })
        .map_err(|_| unsafe_storage())?;
    drop(string_memory);
    let inherit = if directory { "OICI" } else { "" };
    let sddl = format!("O:{sid}D:P(A;{inherit};FA;;;{sid})");
    let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = null_mut();
    // SAFETY: wide is NUL terminated and descriptor is a writable output.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
        || descriptor.is_null()
    {
        return Err(last_error());
    }
    Ok(LocalMemory(descriptor))
}

/// Validate the same owner and protected allow-list used by the legacy Agent.
/// The descriptor, owner and DACL must all belong to one live API result.
fn validate_acl(
    identity: &Identity,
    descriptor: PSECURITY_DESCRIPTOR,
    owner: PSID,
    acl: *mut ACL,
) -> Result<()> {
    if descriptor.is_null() || owner.is_null() || acl.is_null() {
        return Err(unsafe_storage());
    }
    // SAFETY: owner and acl are returned by the security API with descriptor.
    if unsafe { IsValidSid(owner) } == 0
        || unsafe { EqualSid(owner, identity.user.as_ptr()) } == 0
        || unsafe { IsValidAcl(acl) } == 0
    {
        return Err(unsafe_storage());
    }
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: descriptor is live and both outputs point to initialized storage.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
        || control & SE_DACL_PROTECTED == 0
    {
        return Err(unsafe_storage());
    }
    let mut information = ACL_SIZE_INFORMATION::default();
    // SAFETY: acl was validated and information has the requested size.
    if unsafe {
        GetAclInformation(
            acl,
            (&mut information as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
        || information.AceCount > 256
    {
        return Err(unsafe_storage());
    }
    // IsValidAcl checks the ACL structure; additionally bound each SID before
    // passing it to an API, including its variable sub-authority array.
    let start = acl as usize;
    let length = unsafe { (*acl).AclSize } as usize;
    let end = start.checked_add(length).ok_or_else(unsafe_storage)?;
    let mut user_control = false;
    for index in 0..information.AceCount {
        let mut raw: *mut c_void = null_mut();
        // SAFETY: acl is valid and raw is a writable output pointer.
        if unsafe { GetAce(acl, index, &mut raw) } == 0 || raw.is_null() {
            return Err(unsafe_storage());
        }
        let offset = raw as usize;
        if offset < start
            || offset
                .checked_add(size_of::<ACE_HEADER>())
                .is_none_or(|v| v > end)
        {
            return Err(unsafe_storage());
        }
        // SAFETY: the header is wholly within the validated ACL.
        let header = unsafe { std::ptr::read_unaligned(raw.cast::<ACE_HEADER>()) };
        let ace_end = offset
            .checked_add(header.AceSize as usize)
            .filter(|end_of_ace| *end_of_ace <= end)
            .ok_or_else(unsafe_storage)?;
        const SID_OFFSET: usize = size_of::<ACE_HEADER>() + size_of::<u32>();
        if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE
            || offset
                .checked_add(SID_OFFSET + 8)
                .is_none_or(|v| v > ace_end)
        {
            return Err(unsafe_storage());
        }
        // SAFETY: the checked ACE has room for its mask and fixed SID header.
        let bytes =
            unsafe { std::slice::from_raw_parts(raw.cast::<u8>(), header.AceSize as usize) };
        let count = usize::from(bytes[SID_OFFSET + 1]);
        if SID_OFFSET + 8 + count * size_of::<u32>() > bytes.len() {
            return Err(unsafe_storage());
        }
        // SAFETY: the SID fits inside this ACE; Windows ACLs align ACEs to u32.
        let sid = unsafe { raw.cast::<u8>().add(SID_OFFSET).cast::<c_void>() };
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(unsafe_storage());
        }
        let is_user = unsafe { EqualSid(sid, identity.user.as_ptr()) } != 0;
        if !is_user
            && unsafe { EqualSid(sid, identity.system.as_ptr()) } == 0
            && unsafe { EqualSid(sid, identity.administrators.as_ptr()) } == 0
        {
            return Err(unsafe_storage());
        }
        let mask = u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| unsafe_storage())?);
        if is_user
            && u32::from(header.AceFlags) & INHERIT_ONLY_ACE == 0
            && mask & FILE_ALL_ACCESS == FILE_ALL_ACCESS
        {
            user_control = true;
        }
    }
    if !user_control {
        return Err(unsafe_storage());
    }
    Ok(())
}

fn check_file_acl(file: &File) -> Result<()> {
    let identity = Identity::current()?;
    let mut owner: PSID = null_mut();
    let mut acl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: file is live and all output pointers are writable.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32).into());
    }
    let descriptor = LocalMemory(descriptor);
    validate_acl(&identity, descriptor.0, owner, acl)
        .map_err(|_| Error::message("Windows private storage owner or ACL cannot be verified"))
}

fn information(file: &File, directory: bool) -> Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file is live and info is a writable output structure.
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK
        || unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        || (!directory && info.nNumberOfLinks != 1)
    {
        return Err(unsafe_storage());
    }
    Ok(info)
}

fn path_parts(path: &Path) -> Result<(Vec<u16>, Vec<Vec<u16>>)> {
    let mut components = path.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
            _ => return Err(unsafe_storage()),
        },
        _ => return Err(unsafe_storage()),
    };
    if components.next() != Some(Component::RootDir) || !drive.is_ascii_alphabetic() {
        return Err(unsafe_storage());
    }
    // Extended syntax avoids Win32's trailing-dot/space rewriting and device
    // name interpretation. Remote and object-manager prefixes are rejected.
    let root = vec![
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        u16::from(drive),
        b':' as u16,
        b'\\' as u16,
        0,
    ];
    let mut parts = Vec::new();
    let mut total = root.len();
    for component in components {
        let Component::Normal(name) = component else {
            return Err(unsafe_storage());
        };
        let value: Vec<u16> = OsStr::encode_wide(name).collect();
        if value.is_empty()
            || value.len() > 255
            || value.iter().any(|unit| matches!(*unit, 0 | 47 | 58 | 92))
        {
            return Err(unsafe_storage());
        }
        total += value.len() + 1;
        if total > 32_000 || parts.len() >= 256 {
            return Err(unsafe_storage());
        }
        parts.push(value);
    }
    if parts.is_empty() {
        return Err(unsafe_storage());
    }
    Ok((root, parts))
}

fn open_root(root: &[u16], read_acl: bool) -> Result<File> {
    // SAFETY: root is NUL terminated and represents a drive root.
    if !matches!(
        unsafe { GetDriveTypeW(root.as_ptr()) },
        DRIVE_FIXED | DRIVE_REMOVABLE
    ) {
        return Err(Error::message(
            "Windows private storage requires a local drive",
        ));
    }
    // Omit delete sharing so another process cannot rename a held directory.
    // SAFETY: root is a live NUL-terminated path, with no borrowed handles.
    let handle = unsafe {
        CreateFileW(
            root.as_ptr(),
            FILE_READ_ATTRIBUTES | SYNCHRONIZE | if read_acl { READ_CONTROL } else { 0 },
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    // SAFETY: CreateFileW returned a fresh owned handle.
    let file = unsafe { File::from_raw_handle(handle) };
    information(&file, true)?;
    let mut flags = 0u32;
    // SAFETY: file is a live directory handle and flags is writable.
    if unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            null_mut(),
            0,
        )
    } == 0
        || flags & FILE_PERSISTENT_ACLS == 0
    {
        return Err(Error::message(
            "Windows private storage requires persistent ACLs on its volume",
        ));
    }
    Ok(file)
}

fn open_child(parent: &File, name: &[u16], directory: bool, read_acl: bool) -> Result<File> {
    open_child_access(parent, name, directory, read_acl, false)
}

fn open_child_access(
    parent: &File,
    name: &[u16],
    directory: bool,
    read_acl: bool,
    delete: bool,
) -> Result<File> {
    let bytes = name.len().checked_mul(2).ok_or_else(unsafe_storage)?;
    let length = u16::try_from(bytes).map_err(|_| unsafe_storage())?;
    let text = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: name.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &text,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null_mut(),
        SecurityQualityOfService: null_mut(),
    };
    let mut result: HANDLE = null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    let desired = (if directory {
        FILE_READ_ATTRIBUTES | SYNCHRONIZE | if read_acl { READ_CONTROL } else { 0 }
    } else {
        GENERIC_READ | READ_CONTROL | SYNCHRONIZE
    }) | if delete { DELETE } else { 0 };
    let sharing = if directory {
        FILE_SHARE_READ | FILE_SHARE_WRITE
    } else {
        FILE_SHARE_READ
    };
    // Each open is a single name relative to an already-held directory handle.
    // The final reparse point is opened itself and rejected by information().
    // SAFETY: all pointers remain valid through the synchronous NtCreateFile.
    let status = unsafe {
        NtCreateFile(
            &mut result,
            desired,
            &attributes,
            &mut status_block,
            null(),
            0,
            sharing,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            null(),
            0,
        )
    };
    if status != 0 {
        // SAFETY: RtlNtStatusToDosError converts a returned NTSTATUS value.
        return Err(
            io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32).into(),
        );
    }
    if result.is_null() || result == INVALID_HANDLE_VALUE {
        return Err(unsafe_storage());
    }
    // SAFETY: successful NtCreateFile returned a unique handle.
    let file = unsafe { File::from_raw_handle(result) };
    information(&file, directory)?;
    Ok(file)
}

pub(crate) fn open_private_file_for_deletion(parent: &File, name: &[u16]) -> Result<File> {
    let file = open_child_access(parent, name, false, true, true)?;
    check_file_acl(&file)?;
    Ok(file)
}

pub(crate) fn open_private_directory_for_deletion(parent: &File, name: &[u16]) -> Result<File> {
    let directory = open_child_access(parent, name, true, true, true)?;
    check_file_acl(&directory)?;
    Ok(directory)
}

/// Deletion can remain pending while another process holds a compatible
/// handle. Only a definite not-found result confirms the name has gone.
pub(crate) fn require_child_absent(parent: &File, name: &[u16], directory: bool) -> Result<()> {
    match open_child(parent, name, directory, false) {
        Err(Error::Io(error)) if definitely_not_found(&error) => Ok(()),
        Ok(_) => Err(Error::message(
            "Windows storage name still exists after deletion",
        )),
        Err(error) => Err(Error::message(format!(
            "Windows storage deletion could not be confirmed: {error}"
        ))),
    }
}

pub(crate) fn definitely_not_found(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32)
}

pub(crate) fn delete_open_object(file: &File) -> Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: file is an owned live handle with DELETE access; the structure
    // remains valid for the synchronous call. Deletion occurs on close.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(last_error());
    }
    Ok(())
}

pub(crate) fn create_child(parent: &File, name: &[u16], directory: bool) -> Result<File> {
    let length = u16::try_from(name.len().checked_mul(2).ok_or_else(unsafe_storage)?)
        .map_err(|_| unsafe_storage())?;
    let text = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: name.as_ptr().cast_mut(),
    };
    let descriptor = private_descriptor(directory)?;
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &text,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: descriptor.0.cast(),
        SecurityQualityOfService: null_mut(),
    };
    let mut result: HANDLE = null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    let desired = FILE_READ_ATTRIBUTES
        | READ_CONTROL
        | DELETE
        | SYNCHRONIZE
        | if directory { 0 } else { GENERIC_WRITE };
    let sharing = if directory {
        FILE_SHARE_READ | FILE_SHARE_WRITE
    } else {
        0
    };
    let options = FILE_SYNCHRONOUS_IO_NONALERT
        | if directory {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_WRITE_THROUGH
        };
    // FILE_CREATE is exclusive. OBJ_DONT_REPARSE requests rejection of a
    // redirected name; its use with a relative root remains a native Windows
    // acceptance requirement. The verified parent also prevents untrusted
    // principals from inserting names. All pointers outlive the call.
    // SAFETY: parent is live, text is one bounded component, and all outputs
    // and the creation descriptor are valid for the duration of the call.
    let status = unsafe {
        NtCreateFile(
            &mut result,
            desired,
            &attributes,
            &mut status_block,
            null(),
            if directory {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
            sharing,
            FILE_CREATE,
            options,
            null(),
            0,
        )
    };
    if status != 0 {
        // SAFETY: RtlNtStatusToDosError converts a returned NTSTATUS value.
        return Err(
            io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32).into(),
        );
    }
    if result.is_null() || result == INVALID_HANDLE_VALUE {
        return Err(unsafe_storage());
    }
    // SAFETY: successful NtCreateFile returned a fresh owned handle.
    let file = unsafe { File::from_raw_handle(result) };
    if let Err(error) = information(&file, directory).and_then(|_| check_file_acl(&file)) {
        // No secret has been written yet. Remove only the object this handle
        // owns; report cleanup failure rather than hiding a leftover object.
        if let Err(cleanup) = delete_open_object(&file) {
            return Err(Error::message(format!(
                "New private object failed validation ({error}); cleanup also failed ({cleanup})"
            )));
        }
        return Err(error);
    }
    Ok(file)
}

/// Keep the drive and each ancestor pinned until the caller is finished. When
/// requested, only missing directories beneath verified private parents are
/// created. Existing directories are never repaired, and the immediate parent
/// must have a private ACL.
pub(crate) fn open_parent(
    path: &Path,
    create_missing: bool,
    private_parent: bool,
) -> Result<(Vec<File>, Vec<u16>)> {
    let (root, parts) = path_parts(path)?;
    let mut directories = vec![open_root(
        &root,
        create_missing || (private_parent && parts.len() == 1),
    )?];
    for (index, name) in parts[..parts.len() - 1].iter().enumerate() {
        let parent = directories.last().expect("root exists");
        let read_acl = create_missing || (private_parent && index + 2 == parts.len());
        let next = match open_child(parent, name, true, read_acl) {
            Ok(file) => file,
            Err(Error::Io(error)) if create_missing && error.kind() == io::ErrorKind::NotFound => {
                // Without a private parent, another principal could insert or
                // redirect the missing name between the open and the create.
                check_file_acl(parent)?;
                match create_child(parent, name, true) {
                    Ok(file) => file,
                    // Another creator won the name. Open and verify its actual
                    // object rather than changing its permissions.
                    Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                        open_child(parent, name, true, read_acl)?
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        directories.push(next);
    }
    if private_parent {
        check_file_acl(directories.last().expect("root exists"))?;
    }
    Ok((directories, parts.last().expect("path has a name").clone()))
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    open_parent(path, true, true).map(|_| ())
}

pub fn read_private(path: &Path) -> Result<Vec<u8>> {
    let (directories, name) = open_parent(path, false, false)?;
    let file = open_child(directories.last().expect("root exists"), &name, false, true)?;
    check_file_acl(&file)?;
    let metadata = file.metadata()?;
    if metadata.len() > MAX_PRIVATE_FILE_BYTES {
        return Err(Error::message(
            "Private storage file exceeds the size limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PRIVATE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PRIVATE_FILE_BYTES {
        return Err(Error::message(
            "Private storage file exceeds the size limit",
        ));
    }
    // Keep every ancestor pinned until the last read completes.
    drop(directories);
    Ok(bytes)
}

/// Mark an uninstalled temporary file for deletion on close, including during
/// unwinding. The handle always refers to the file created by this call.
struct Temporary {
    file: File,
    installed_or_deleted: bool,
}

impl Temporary {
    fn discard(&mut self) -> Result<()> {
        delete_open_object(&self.file)?;
        self.installed_or_deleted = true;
        Ok(())
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.installed_or_deleted {
            let _ = delete_open_object(&self.file);
        }
    }
}

fn rename_new(file: &File, parent: &File, name: &[u16]) -> Result<()> {
    let name_bytes = name.len().checked_mul(2).ok_or_else(unsafe_storage)?;
    let length = offset_of!(FILE_RENAME_INFO, FileName)
        .checked_add(name_bytes)
        .and_then(|value| value.checked_add(size_of::<u16>()))
        .ok_or_else(unsafe_storage)?
        .max(size_of::<FILE_RENAME_INFO>());
    // usize provides sufficient alignment for FILE_RENAME_INFO on Windows.
    let mut buffer = vec![0usize; length.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: the aligned buffer has room for the structure, filename, and NUL.
    // parent and file remain open throughout the synchronous API call.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = parent.as_raw_handle();
        (*info).FileNameLength = u32::try_from(name_bytes).map_err(|_| unsafe_storage())?;
        let destination = buffer
            .as_mut_ptr()
            .cast::<u8>()
            .add(offset_of!(FILE_RENAME_INFO, FileName))
            .cast::<u16>();
        std::ptr::copy_nonoverlapping(name.as_ptr(), destination, name.len());
        if SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfo,
            info.cast(),
            u32::try_from(length).map_err(|_| unsafe_storage())?,
        ) == 0
        {
            return Err(last_error());
        }
    }
    Ok(())
}

/// Install a new secret without overwriting an existing configuration. The
/// parent and temporary object must both have validated protected ACLs. The
/// file is flushed before the handle-relative rename; Windows crash durability
/// of directory metadata still needs native filesystem verification.
pub fn write_private_new(path: &Path, data: &[u8]) -> Result<()> {
    if data.len() as u64 > MAX_PRIVATE_FILE_BYTES {
        return Err(Error::message(
            "Private storage file exceeds the size limit",
        ));
    }
    let (directories, name) = open_parent(path, false, true)?;
    let parent = directories.last().expect("root exists");
    let temporary_name: Vec<u16> = format!(".rdh-{}.tmp", crate::random::hex_token::<16>()?)
        .encode_utf16()
        .collect();
    let mut temporary = Temporary {
        file: create_child(parent, &temporary_name, false)?,
        installed_or_deleted: false,
    };
    let result = (|| {
        temporary.file.write_all(data)?;
        temporary.file.sync_all()?;
        rename_new(&temporary.file, parent, &name)
    })();
    match result {
        Ok(()) => {
            temporary.installed_or_deleted = true;
            Ok(())
        }
        Err(error) => {
            if let Err(cleanup) = temporary.discard() {
                return Err(Error::message(format!(
                    "Private write failed ({error}); temporary cleanup also failed ({cleanup})"
                )));
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, SECURITY_ATTRIBUTES,
    };
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateDirectoryW};

    fn user_text(identity: &Identity) -> String {
        let mut raw = null_mut();
        // SAFETY: the identity owns a valid SID and raw is a writable output.
        assert_ne!(
            unsafe { ConvertSidToStringSidW(identity.user.as_ptr(), &mut raw) },
            0
        );
        let memory = LocalMemory(raw.cast());
        let mut length = 0;
        // Windows SIDs have a bounded textual representation.
        while length < 256 && unsafe { *raw.add(length) } != 0 {
            length += 1;
        }
        assert!(length < 256);
        // SAFETY: ConvertSidToStringSidW returned a NUL-terminated UTF-16 string.
        let text = String::from_utf16(unsafe { std::slice::from_raw_parts(raw, length) }).unwrap();
        drop(memory);
        text
    }

    fn descriptor(text: &str) -> LocalMemory {
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let mut raw = null_mut();
        // SAFETY: wide is NUL terminated and raw is a writable output.
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    1,
                    &mut raw,
                    null_mut(),
                )
            },
            0,
            "invalid test SDDL"
        );
        LocalMemory(raw)
    }

    fn accepted(identity: &Identity, descriptor: &LocalMemory) -> bool {
        let mut owner = null_mut();
        let mut owner_defaulted = 0;
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = null_mut();
        // SAFETY: descriptor is live and outputs are writable.
        assert_ne!(
            unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut owner_defaulted) },
            0
        );
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
            },
            0
        );
        present != 0 && validate_acl(identity, descriptor.0, owner, acl).is_ok()
    }

    #[test]
    fn private_acl_accepts_only_the_user_and_trusted_administrators() {
        let identity = Identity::current().unwrap();
        let user = user_text(&identity);
        let check =
            |owner: &str, dacl: &str| accepted(&identity, &descriptor(&format!("O:{owner}{dacl}")));
        assert!(check(
            &user,
            &format!("D:P(A;;FA;;;{user})(A;;FA;;;SY)(A;;FA;;;BA)")
        ));
        assert!(!check("WD", &format!("D:P(A;;FA;;;{user})")));
        assert!(!check(&user, &format!("D:(A;;FA;;;{user})")));
        assert!(!check(&user, &format!("D:P(A;;FA;;;{user})(A;;FR;;;WD)")));
        assert!(!check(&user, &format!("D:P(D;;FR;;;WD)(A;;FA;;;{user})")));
        assert!(!check(&user, &format!("D:P(A;;FR;;;{user})")));
        assert!(!check(&user, &format!("D:P(A;OICIIO;FA;;;{user})")));
    }

    #[test]
    fn generated_descriptors_have_a_protected_user_only_acl() {
        let identity = Identity::current().unwrap();
        assert!(accepted(&identity, &private_descriptor(false).unwrap()));
        assert!(accepted(&identity, &private_descriptor(true).unwrap()));
    }

    #[test]
    fn paths_reject_redirected_names_and_alternate_streams() {
        for bad in [
            r"relative\file.json",
            r"C:relative\file.json",
            r"C:\work\..\file.json",
            r"C:\work\file.json:stream",
            r"\\server\share\file.json",
            r"\\.\C:\file.json",
        ] {
            assert!(path_parts(Path::new(bad)).is_err(), "{bad}");
        }
        assert!(path_parts(Path::new(r"C:\work\file.json")).is_ok());
        assert!(path_parts(Path::new(r"\\?\C:\work\file.json")).is_ok());
    }

    #[test]
    fn deletion_confirmation_accepts_only_definite_name_absence() {
        use windows_sys::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_DELETE_PENDING, ERROR_INVALID_DRIVE, ERROR_PATH_NOT_FOUND,
        };

        assert!(definitely_not_found(&io::Error::from_raw_os_error(
            ERROR_FILE_NOT_FOUND as i32
        )));
        for code in [
            ERROR_ACCESS_DENIED,
            ERROR_DELETE_PENDING,
            ERROR_INVALID_DRIVE,
            ERROR_PATH_NOT_FOUND,
        ] {
            assert!(!definitely_not_found(&io::Error::from_raw_os_error(
                code as i32
            )));
        }
    }

    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Fixture {
        fn new() -> Self {
            let fixture = Self(std::env::temp_dir().join(format!(
                "rdh-private-windows-{}",
                crate::random::hex_token::<8>().unwrap()
            )));
            std::fs::create_dir(&fixture.0).unwrap();
            fixture
        }

        fn write_with_acl(&self, protected: bool) -> std::path::PathBuf {
            let path = self.0.join("device.json");
            let identity = Identity::current().unwrap();
            let user = user_text(&identity);
            let flag = if protected { "P" } else { "" };
            let descriptor = descriptor(&format!("O:{user}D:{flag}(A;;FA;;;{user})"));
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            };
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: the name and security attributes remain valid for the call.
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    &attributes,
                    CREATE_NEW,
                    FILE_ATTRIBUTE_NORMAL,
                    null_mut(),
                )
            };
            assert_ne!(
                handle,
                INVALID_HANDLE_VALUE,
                "{}",
                io::Error::last_os_error()
            );
            // SAFETY: CreateFileW returned a unique handle.
            let mut file = unsafe { File::from_raw_handle(handle) };
            file.write_all(b"fixture-secret").unwrap();
            file.sync_all().unwrap();
            path
        }

        fn private_directory(&self, name: &str) -> std::path::PathBuf {
            let path = self.0.join(name);
            let descriptor = private_descriptor(true).unwrap();
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            };
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: wide and attributes are valid through the synchronous call.
            assert_ne!(unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) }, 0);
            path
        }
    }

    #[test]
    fn created_file_round_trip_with_protected_acl() {
        let fixture = Fixture::new();
        let path = fixture.write_with_acl(true);
        assert_eq!(read_private(&path).unwrap(), b"fixture-secret");
    }

    #[test]
    fn exclusive_private_creation_is_atomic_and_does_not_replace() {
        let fixture = Fixture::new();
        let path = fixture
            .private_directory("private")
            .join("nested")
            .join("device.json");
        ensure_parent(&path).unwrap();
        write_private_new(&path, b"first-secret").unwrap();
        assert_eq!(read_private(&path).unwrap(), b"first-secret");
        assert!(write_private_new(&path, b"second-secret").is_err());
        assert_eq!(read_private(&path).unwrap(), b"first-secret");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [OsStr::new("device.json")]);
    }

    #[test]
    fn concurrent_private_creation_has_exactly_one_winner() {
        let fixture = Fixture::new();
        let path = fixture.private_directory("private").join("device.json");
        ensure_parent(&path).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let mut threads = Vec::new();
        for number in 0..4u8 {
            let path = path.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                (number, write_private_new(&path, &[number]))
            }));
        }
        let winners: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter_map(|(number, result)| result.is_ok().then_some(number))
            .collect();
        assert_eq!(winners.len(), 1);
        assert_eq!(read_private(&path).unwrap(), winners);
    }

    #[test]
    fn profile_lock_uses_legacy_names_and_refuses_a_changed_owner() {
        use crate::profile_lock::ProfileLocks;

        let fixture = Fixture::new();
        let config = fixture.private_directory("private").join("device.json");
        let lock = config.with_file_name("device.json.node-lock");
        let guard = config.with_file_name("device.json.node-lock.reclaiming");
        let mut held = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
        let owner_path = lock.join("owner.json");
        let owner = std::fs::read(&owner_path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&owner).unwrap();
        assert_eq!(value["pid"], std::process::id());
        assert!(value["nonce"].as_str().unwrap().len() >= 32);
        assert!(!guard.exists());
        assert!(ProfileLocks::acquire(std::slice::from_ref(&config)).is_err());
        std::fs::write(
            &owner_path,
            format!(
                "{{\"pid\":{},\"nonce\":\"{}\"}}",
                std::process::id(),
                "x".repeat(40)
            ),
        )
        .unwrap();
        assert!(held.release().is_err());
        assert!(lock.is_dir());
        std::fs::write(&owner_path, owner).unwrap();
        held.release().unwrap();
        assert!(!lock.exists());
        assert!(!guard.exists());
    }

    #[test]
    fn profile_lock_does_not_reclaim_an_existing_guard_or_retained_lock() {
        use crate::profile_lock::ProfileLocks;

        let fixture = Fixture::new();
        let config = fixture.private_directory("private").join("device.json");
        let lock = config.with_file_name("device.json.node-lock");
        let guard = config.with_file_name("device.json.node-lock.reclaiming");
        std::fs::create_dir(&guard).unwrap();
        assert!(ProfileLocks::acquire(std::slice::from_ref(&config)).is_err());
        assert!(ProfileLocks::recover(std::slice::from_ref(&config)).is_err());
        assert!(guard.is_dir());
        assert!(!lock.exists());
        std::fs::remove_dir(&guard).unwrap();
        let mut held = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
        held.retain_on_drop();
        drop(held);
        assert!(ProfileLocks::recover(std::slice::from_ref(&config)).is_err());
        assert!(ProfileLocks::acquire(std::slice::from_ref(&config)).is_err());
        assert!(lock.join("owner.json").is_file());
    }

    #[test]
    fn profile_recovery_validates_all_owners_before_removing_any_lock() {
        use crate::profile_lock::ProfileLocks;

        let fixture = Fixture::new();
        let directory = fixture.private_directory("private");
        let first = directory.join("first.json");
        let second = directory.join("second.json");
        let first_lock = directory.join("first.json.node-lock");
        let second_lock = directory.join("second.json.node-lock");
        let mut held = ProfileLocks::acquire(&[first.clone(), second.clone()]).unwrap();
        held.retain_on_drop();
        drop(held);
        let first_owner = first_lock.join("owner.json");
        let second_owner = second_lock.join("owner.json");
        let mut first_record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&first_owner).unwrap()).unwrap();
        let mut second_record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&second_owner).unwrap()).unwrap();
        first_record["pid"] = serde_json::json!(u32::MAX);
        std::fs::write(&first_owner, serde_json::to_vec(&first_record).unwrap()).unwrap();
        assert!(ProfileLocks::recover(&[first.clone(), second.clone()]).is_err());
        assert!(first_owner.is_file() && second_owner.is_file());

        // A malformed owner and a hard-linked owner each leave both locks
        // untouched, including the already verified first one.
        std::fs::write(&second_owner, b"{}").unwrap();
        assert!(ProfileLocks::recover(&[first.clone(), second.clone()]).is_err());
        assert!(first_owner.is_file() && second_owner.is_file());
        second_record["pid"] = serde_json::json!(u32::MAX);
        std::fs::write(&second_owner, serde_json::to_vec(&second_record).unwrap()).unwrap();
        let alias = directory.join("owner-hardlink.json");
        std::fs::hard_link(&second_owner, &alias).unwrap();
        assert!(ProfileLocks::recover(&[first.clone(), second.clone()]).is_err());
        assert!(first_owner.is_file() && second_owner.is_file());
        std::fs::remove_file(alias).unwrap();

        ProfileLocks::recover(&[first.clone(), second.clone()]).unwrap();
        assert!(!first_lock.exists() && !second_lock.exists());
        assert!(!directory.join("first.json.node-lock.reclaiming").exists());
        assert!(!directory.join("second.json.node-lock.reclaiming").exists());
        let mut next = ProfileLocks::acquire(&[first, second]).unwrap();
        next.release().unwrap();
    }

    #[test]
    fn profile_recovery_keeps_unexpected_directory_contents_and_fails_closed() {
        use crate::profile_lock::ProfileLocks;

        let fixture = Fixture::new();
        let directory = fixture.private_directory("private");
        let config = directory.join("device.json");
        let lock = directory.join("device.json.node-lock");
        let mut held = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
        held.retain_on_drop();
        drop(held);
        let owner = lock.join("owner.json");
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&owner).unwrap()).unwrap();
        record["pid"] = serde_json::json!(u32::MAX);
        std::fs::write(&owner, serde_json::to_vec(&record).unwrap()).unwrap();
        let unexpected = lock.join("do-not-delete.txt");
        std::fs::write(&unexpected, b"preserve this file").unwrap();
        assert!(ProfileLocks::recover(std::slice::from_ref(&config)).is_err());
        assert_eq!(std::fs::read(&unexpected).unwrap(), b"preserve this file");
        assert!(ProfileLocks::acquire(std::slice::from_ref(&config)).is_err());
    }

    #[test]
    fn profile_lock_release_does_not_claim_a_pending_owner_deletion() {
        use crate::profile_lock::ProfileLocks;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;

        let fixture = Fixture::new();
        let directory = fixture.private_directory("private");
        let config = directory.join("device.json");
        let lock = directory.join("device.json.node-lock");
        let mut held = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
        let owner = lock.join("owner.json");
        let wide: Vec<u16> = owner.as_os_str().encode_wide().chain(Some(0)).collect();
        // The extra reader permits deletion but keeps the marked file alive.
        // SAFETY: the path is NUL terminated and success returns an owned handle.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        assert_ne!(raw, INVALID_HANDLE_VALUE);
        // SAFETY: CreateFileW returned a fresh owned handle.
        let extra = unsafe { File::from_raw_handle(raw) };
        assert!(held.release().is_err());
        assert!(lock.is_dir());
        drop(extra);
        assert!(ProfileLocks::acquire(std::slice::from_ref(&config)).is_err());
        held.retain_on_drop();
        drop(held);
        // The explicitly created fixture directory is still owned by the test.
        std::fs::remove_dir(lock).unwrap();
    }

    #[test]
    fn unsafe_existing_parent_is_never_repaired_or_written() {
        let fixture = Fixture::new();
        let directory = fixture.0.join("unsafe-parent");
        let identity = Identity::current().unwrap();
        let user = user_text(&identity);
        let descriptor = descriptor(&format!("O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FR;;;WD)"));
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let wide: Vec<u16> = directory.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: wide and attributes are valid throughout the synchronous call.
        assert_ne!(unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) }, 0);
        let path = directory.join("device.json");
        assert!(ensure_parent(&path).is_err());
        assert!(write_private_new(&path, b"must-not-be-written").is_err());
        assert!(ensure_parent(&directory.join("missing").join("device.json")).is_err());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    }

    #[test]
    fn oversized_private_write_fails_before_creating_a_file() {
        let fixture = Fixture::new();
        let path = fixture.0.join("device.json");
        let data = vec![0; MAX_PRIVATE_FILE_BYTES as usize + 1];
        let error = write_private_new(&path, &data).unwrap_err().to_string();
        assert!(error.contains("size limit"));
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 0);
    }

    #[test]
    fn failed_rename_preserves_target_and_discards_only_its_temporary_file() {
        let fixture = Fixture::new();
        let target = fixture.write_with_acl(false);
        let temporary_path = fixture.0.join("temporary");
        let wide: Vec<u16> = temporary_path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // This fixture deliberately bypasses ACL checks to test handle-based
        // cleanup independently, including on filesystems without ACL support.
        // SAFETY: wide is NUL terminated and CreateFileW returns an owned handle.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_WRITE | DELETE,
                0,
                null(),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        let mut temporary = Temporary {
            // SAFETY: CreateFileW returned a new owned handle.
            file: unsafe { File::from_raw_handle(handle) },
            installed_or_deleted: false,
        };
        temporary.file.write_all(b"temporary-data").unwrap();
        let (directories, target_name) = open_parent(&target, false, false).unwrap();
        assert!(rename_new(&temporary.file, directories.last().unwrap(), &target_name).is_err());
        temporary.discard().unwrap();
        drop(temporary);
        assert!(!temporary_path.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"fixture-secret");
    }

    #[test]
    fn unprotected_file_is_rejected_before_its_contents_are_read() {
        let fixture = Fixture::new();
        let path = fixture.write_with_acl(false);
        let error = read_private(&path).unwrap_err().to_string();
        assert!(error.contains("owner or ACL"), "{error}");
    }

    #[test]
    fn relative_handle_opens_the_named_file_and_refuses_reparse_points() {
        let fixture = Fixture::new();
        let path = fixture.write_with_acl(false);
        let (root, parts) = path_parts(&path).unwrap();
        let mut directories = vec![open_root(&root, false).unwrap()];
        for name in &parts[..parts.len() - 1] {
            directories.push(open_child(directories.last().unwrap(), name, true, false).unwrap());
        }
        let directory = directories.last().unwrap();
        let name = parts.last().unwrap();
        let mut file = open_child(directory, name, false, true).unwrap();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"fixture-secret");
        drop(file);

        let alias = fixture.0.join("hardlink.json");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(open_child(directory, name, false, true).is_err());
        std::fs::remove_file(alias).unwrap();

        let link = fixture.0.join("link.json");
        match std::os::windows::fs::symlink_file(&path, &link) {
            Ok(()) => {
                let name: Vec<u16> = link.file_name().unwrap().encode_wide().collect();
                assert!(open_child(directory, &name, false, true).is_err());
                let target = fixture.0.join("directory");
                let link = fixture.0.join("directory-link");
                std::fs::create_dir(&target).unwrap();
                std::os::windows::fs::symlink_dir(&target, &link).unwrap();
                let name: Vec<u16> = link.file_name().unwrap().encode_wide().collect();
                assert!(open_child(directory, &name, true, false).is_err());

                let missing_target = fixture.0.join("missing-target");
                let link = fixture.0.join("creation-link");
                std::os::windows::fs::symlink_dir(&missing_target, &link).unwrap();
                let name: Vec<u16> = link.file_name().unwrap().encode_wide().collect();
                assert!(create_child(directory, &name, true).is_err());
                assert!(!missing_target.exists());
            }
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || matches!(error.raw_os_error(), Some(50 | 1314)) =>
            {
                eprintln!("Windows symbolic-link creation unavailable; reparse test not exercised");
            }
            Err(error) => panic!("cannot create reparse fixture: {error}"),
        }
    }

    #[test]
    fn held_directory_cannot_be_renamed() {
        let fixture = Fixture::new();
        let path = fixture.write_with_acl(false);
        let (root, parts) = path_parts(&path).unwrap();
        let mut directories = vec![open_root(&root, false).unwrap()];
        for name in &parts[..parts.len() - 1] {
            directories.push(open_child(directories.last().unwrap(), name, true, false).unwrap());
        }
        let renamed = fixture.0.with_extension("moved");
        let attempt = std::fs::rename(&fixture.0, &renamed);
        if attempt.is_ok() {
            drop(directories);
            std::fs::rename(&renamed, &fixture.0).unwrap();
            panic!("a held directory was renamed");
        }
    }
}
