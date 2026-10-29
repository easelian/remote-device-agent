//! Handle-relative Windows device files. Ordinary files may have hard links;
//! configuration-storage ACL and single-link restrictions do not apply here.
//! Ancestor handles deny delete sharing and remain owned through each operation.
use super::*;
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Write};
use std::mem::{offset_of, size_of};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::ptr::{null, null_mut};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_RENAME_POSIX_SEMANTICS,
    FILE_RENAME_REPLACE_IF_EXISTS, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation,
    FileRenameInformationEx, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES,
    ERROR_NOT_SAME_DEVICE, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, DELETE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
    FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_BOTH_DIR_INFO, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_TYPE_DISK,
    FileBasicInfo, FileDispositionInfo, FileIdBothDirectoryInfo, FileIdInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFileType, OPEN_EXISTING,
    SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};

const DIRECTORY_ACCESS: u32 = FILE_READ_ATTRIBUTES | FILE_TRAVERSE | SYNCHRONIZE;
const ALL_SHARING: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

fn invalid() -> String {
    "Windows file identity, type, or path cannot be verified".into()
}
fn last_error() -> String {
    io_error(io::Error::last_os_error())
}
fn nt_error(status: i32) -> io::Error {
    // SAFETY: converts a returned NTSTATUS; no pointers or handles are borrowed.
    io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
}

pub(super) fn validate_component(part: &str) -> Result<(), String> {
    if part == "." || part.is_empty() {
        return Ok(());
    }
    let base = part
        .split(['.', ' '])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let reserved = matches!(
        base.as_str(),
        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$"
    ) || ["com", "lpt"].iter().any(|prefix| {
        base.strip_prefix(prefix).is_some_and(|suffix| {
            let mut chars = suffix.chars();
            matches!(chars.next(), Some('1'..='9' | '¹' | '²' | '³')) && chars.next().is_none()
        })
    });
    if part.ends_with(['.', ' '])
        || part
            .chars()
            .any(|c| c <= '\u{1f}' || matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'))
        || reserved
    {
        return Err("Path contains an invalid Windows name".into());
    }
    Ok(())
}

fn wide_name(name: &str) -> Result<Vec<u16>, String> {
    validate_component(name)?;
    let wide: Vec<u16> = name.encode_utf16().collect();
    if wide.is_empty() || wide.len() > 255 || matches!(name, "." | "..") || name.contains('/') {
        return Err(invalid());
    }
    Ok(wide)
}

/// Accept exactly drive-absolute or complete UNC roots, never user-supplied
/// device/object-manager namespaces. Extended spelling is constructed internally.
fn root_parts(path: &str) -> Result<(Vec<u16>, Vec<String>), String> {
    if path.encode_utf16().count() > 4096 || path.contains('\0') {
        return Err("Root must be an absolute directory path".into());
    }
    let path = path.replace('\\', "/");
    let bytes = path.as_bytes();
    let (base, remainder) =
        if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":/" {
            (format!("\\\\?\\{}:\\", char::from(bytes[0])), &path[3..])
        } else if let Some(unc) = path.strip_prefix("//") {
            let mut parts = unc.splitn(3, '/');
            let host = parts.next().unwrap_or("");
            let share = parts.next().unwrap_or("");
            for part in [host, share] {
                if part.is_empty()
                    || matches!(part, "." | ".." | "?" | "??")
                    || part.encode_utf16().count() > 255
                    || part.chars().any(|c| {
                        c <= '\u{1f}' || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')
                    })
                {
                    return Err("Root must be an absolute directory path".into());
                }
            }
            (
                format!("\\\\?\\UNC\\{host}\\{share}\\"),
                parts.next().unwrap_or(""),
            )
        } else {
            return Err("Root must be an absolute directory path".into());
        };
    let relative = normalize(remainder)?;
    let parts = if relative == "." {
        Vec::new()
    } else {
        relative.split('/').map(str::to_owned).collect()
    };
    Ok((base.encode_utf16().chain(Some(0)).collect(), parts))
}

#[derive(Clone, Copy)]
struct Snapshot {
    identity: FileIdentity,
    dev: u64,
    ino: u64,
    size: u64,
    attributes: u32,
    modified: i64,
    changed: i64,
}
impl Snapshot {
    fn directory(self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }
    fn facts(self) -> MetadataFacts {
        // Node 22/libuv splits FILETIME into seconds and nanoseconds. Its
        // FillStatsArray then casts Windows seconds to unsigned long (32 bit)
        // before constructing mtimeMs, including pre-epoch/2106 wraparound.
        // Preserve that wire/fingerprint contract without changing the raw
        // timestamps used internally for detecting concurrent file changes.
        let ticks = i128::from(self.modified) - 116_444_736_000_000_000i128;
        let seconds = ticks.div_euclid(10_000_000) as u32;
        let nanoseconds = ticks.rem_euclid(10_000_000) * 100;
        MetadataFacts {
            identity: self.identity,
            dev: self.dev,
            ino: self.ino,
            size: if self.directory() { 0 } else { self.size },
            mode: if self.attributes & FILE_ATTRIBUTE_READONLY != 0 {
                0o444
            } else {
                0o666
            },
            modified_ms: seconds as f64 * 1000.0 + nanoseconds as f64 / 1_000_000.0,
            is_file: !self.directory(),
            is_directory: self.directory(),
        }
    }
    fn same_version(self, other: Self) -> bool {
        self.identity == other.identity
            && self.size == other.size
            && self.attributes == other.attributes
            && self.modified == other.modified
            && self.changed == other.changed
    }
}
fn snapshot(file: &File) -> Result<Snapshot, String> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let mut id = FILE_ID_INFO::default();
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: every call borrows the same live disk handle and a correctly sized,
    // aligned output. Reparse points are inspected as objects and never followed.
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
        return Err(invalid());
    }
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0
        || unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&mut id as *mut FILE_ID_INFO).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        } == 0
        || unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
    {
        return Err(last_error());
    }
    if (info.dwFileAttributes | basic.FileAttributes) & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes ^ basic.FileAttributes) & FILE_ATTRIBUTE_DIRECTORY != 0
    {
        return Err(invalid());
    }
    Ok(Snapshot {
        identity: FileIdentity {
            volume: id.VolumeSerialNumber,
            id: u128::from_le_bytes(id.FileId.Identifier),
        },
        dev: u64::from(info.dwVolumeSerialNumber),
        ino: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        attributes: basic.FileAttributes,
        modified: basic.LastWriteTime,
        changed: basic.ChangeTime,
    })
}

fn open_base(path: &[u16], access: u32) -> Result<File, String> {
    // SAFETY: root_parts constructs one terminated drive/share path.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
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
    // SAFETY: a fresh owned handle was returned above.
    let file = unsafe { File::from_raw_handle(handle) };
    if !snapshot(&file)?.directory() {
        return Err(invalid());
    }
    Ok(file)
}

fn raw_child(
    parent: &File,
    name: &str,
    access: u32,
    sharing: u32,
    disposition: u32,
    directory: bool,
) -> Result<File, io::Error> {
    let name = wide_name(name).map_err(io::Error::other)?;
    let text = UNICODE_STRING {
        Length: (name.len() * 2) as u16,
        MaximumLength: (name.len() * 2) as u16,
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
    let mut handle: HANDLE = null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: all buffers live through this synchronous open of exactly one
    // component. No path string can redirect traversal away from parent.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            null(),
            if directory {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
            sharing,
            disposition,
            FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | if directory {
                    FILE_DIRECTORY_FILE
                } else if disposition == FILE_CREATE {
                    FILE_NON_DIRECTORY_FILE
                } else {
                    0
                },
            null(),
            0,
        )
    };
    if status != 0 {
        return Err(nt_error(status));
    }
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::other(invalid()));
    }
    // SAFETY: successful NtCreateFile returned a unique owned handle.
    let file = unsafe { File::from_raw_handle(handle) };
    Ok(file)
}

fn child(
    parent: &File,
    name: &str,
    access: u32,
    sharing: u32,
    disposition: u32,
    directory: bool,
) -> Result<File, io::Error> {
    let file = raw_child(parent, name, access, sharing, disposition, directory)?;
    let facts = snapshot(&file).map_err(io::Error::other)?;
    if directory && !facts.directory() {
        return Err(io::Error::other(invalid()));
    }
    Ok(file)
}

pub(in crate::files) struct Root {
    base: Vec<u16>,
    parts: Vec<String>,
    chain: Vec<File>,
}
impl Root {
    pub(in crate::files) fn new(path: &str) -> Result<Self, String> {
        let (base, parts) = root_parts(path)?;
        let mut chain = vec![open_base(&base, DIRECTORY_ACCESS)?];
        for name in &parts {
            chain.push(
                child(
                    chain.last().unwrap(),
                    name,
                    DIRECTORY_ACCESS,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    FILE_OPEN,
                    true,
                )
                .map_err(io_error)?,
            );
        }
        Ok(Self { base, parts, chain })
    }
    fn file(&self) -> &File {
        self.chain.last().unwrap()
    }
    pub(super) fn verify(&self) -> Result<(), String> {
        let mut named = open_base(&self.base, DIRECTORY_ACCESS)?;
        if snapshot(&named)?.identity != snapshot(&self.chain[0])?.identity {
            return Err("Configured root directory changed".into());
        }
        for (index, name) in self.parts.iter().enumerate() {
            named = child(
                &named,
                name,
                DIRECTORY_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_OPEN,
                true,
            )
            .map_err(io_error)?;
            if snapshot(&named)?.identity != snapshot(&self.chain[index + 1])?.identity {
                return Err("Configured root directory changed".into());
            }
        }
        Ok(())
    }
    fn open_access(&self, path: &str, access: u32, directory: bool) -> Result<Opened, String> {
        self.verify()?;
        let normalized = normalize(path)?;
        if normalized == "." {
            if !directory {
                return Err("A file name is required".into());
            }
            // Open the retained root name again for an independent directory
            // cursor. Duplicating a handle would share its enumeration state.
            let file = if let Some(name) = self.parts.last() {
                child(
                    &self.chain[self.chain.len() - 2],
                    name,
                    access,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    FILE_OPEN,
                    true,
                )
                .map_err(io_error)?
            } else {
                open_base(&self.base, access)?
            };
            if snapshot(&file)?.identity != snapshot(self.file())?.identity {
                return Err("Configured root directory changed".into());
            }
            return Ok(Opened {
                file,
                _ancestors: Vec::new(),
            });
        }
        let mut names = normalized.split('/').peekable();
        let mut ancestors: Vec<File> = Vec::new();
        while let Some(name) = names.next() {
            let parent = ancestors.last().unwrap_or_else(|| self.file());
            if names.peek().is_none() {
                let file = child(
                    parent,
                    name,
                    access,
                    if directory {
                        FILE_SHARE_READ | FILE_SHARE_WRITE
                    } else {
                        ALL_SHARING
                    },
                    FILE_OPEN,
                    directory,
                )
                .map_err(io_error)?;
                return Ok(Opened {
                    file,
                    _ancestors: ancestors,
                });
            }
            ancestors.push(
                child(
                    parent,
                    name,
                    DIRECTORY_ACCESS,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    FILE_OPEN,
                    true,
                )
                .map_err(io_error)?,
            );
        }
        Err(invalid())
    }
    #[cfg(feature = "experimental-windows-processes")]
    pub(super) fn working_directory(&self, path: &str) -> Result<Opened, String> {
        self.open_access(path, DIRECTORY_ACCESS, true)
    }
    pub(super) fn open(&self, path: &str, directory: bool) -> Result<Opened, String> {
        self.open_access(
            path,
            if directory {
                DIRECTORY_ACCESS | FILE_LIST_DIRECTORY
            } else {
                GENERIC_READ
            },
            directory,
        )
    }
    pub(super) fn metadata(&self, path: &str) -> Result<MetadataFacts, String> {
        if normalize(path)? == "." {
            self.verify()?;
            return snapshot(self.file()).map(Snapshot::facts);
        }
        self.open_access(path, FILE_READ_ATTRIBUTES, false)
            .and_then(|file| metadata(&file))
    }
}

pub(in crate::files) struct Opened {
    file: File,
    _ancestors: Vec<File>,
}
impl Read for Opened {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}
impl Seek for Opened {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}
pub(super) fn metadata(file: &Opened) -> Result<MetadataFacts, String> {
    snapshot(&file.file).map(Snapshot::facts)
}

fn reparse_target_size(data: &[u8]) -> Result<Option<u64>, String> {
    fn word(data: &[u8], offset: usize) -> Result<u16, String> {
        let bytes: [u8; 2] = data
            .get(offset..offset + 2)
            .ok_or_else(invalid)?
            .try_into()
            .unwrap();
        Ok(u16::from_le_bytes(bytes))
    }
    fn dword(data: &[u8], offset: usize) -> Result<u32, String> {
        let bytes: [u8; 4] = data
            .get(offset..offset + 4)
            .ok_or_else(invalid)?
            .try_into()
            .unwrap();
        Ok(u32::from_le_bytes(bytes))
    }
    fn units(data: &[u8], start: usize, length: usize) -> Result<Vec<u16>, String> {
        if !length.is_multiple_of(2) {
            return Err(invalid());
        }
        data.get(start..start.checked_add(length).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        (start..start + length)
            .step_by(2)
            .map(|offset| word(data, offset))
            .collect()
    }
    let tag = dword(data, 0)?;
    let end = 8 + usize::from(word(data, 4)?);
    let data = data.get(..end).ok_or_else(invalid)?;
    let mut target = match tag {
        0xa000000c | 0xa0000003 => {
            let base = if tag == 0xa000000c { 20 } else { 16 };
            units(
                data,
                base + usize::from(word(data, 8)?),
                usize::from(word(data, 10)?),
            )?
        }
        0x8000001b => {
            if dword(data, 8)? < 3 {
                return Ok(None);
            }
            let all = units(data, 12, data.len().checked_sub(12).ok_or_else(invalid)?)?;
            let mut strings = all.split(|value| *value == 0);
            if strings.next().is_none_or(|value| value.is_empty())
                || strings.next().is_none_or(|value| value.is_empty())
            {
                return Ok(None);
            }
            strings.next().ok_or_else(invalid)?.to_vec()
        }
        _ => return Ok(None),
    };
    let nt_prefix = target.starts_with(&[92, 63, 63, 92]);
    let drive = |value: &[u16]| {
        value.len() >= 2
            && u8::try_from(value[0]).is_ok_and(|letter| letter.is_ascii_alphabetic())
            && value[1] == 58
            && (value.len() == 2 || value[2] == 92)
    };
    if tag == 0xa0000003 && !(nt_prefix && drive(&target[4..])) {
        return Ok(None);
    }
    if tag == 0x8000001b && !(target.len() >= 3 && drive(&target)) {
        return Ok(None);
    }
    if nt_prefix {
        if drive(&target[4..]) {
            target.drain(..4);
        } else if target.len() >= 8
            && target[4..8]
                .iter()
                .zip([85, 78, 67, 92])
                .all(|(actual, expected)| {
                    u8::try_from(*actual)
                        .is_ok_and(|actual| actual.to_ascii_uppercase() == expected)
                })
        {
            target.drain(..6);
            target[0] = 92;
        }
    }
    // libuv reports WTF-8 byte length; an isolated UTF-16 surrogate takes three
    // bytes, just as the replacement character does for this length calculation.
    Ok(Some(
        char::decode_utf16(target)
            .map(|value| value.map_or(3, char::len_utf8) as u64)
            .sum(),
    ))
}

fn reparse_entry(directory: &File, name: &str) -> Result<(&'static str, u64), String> {
    let file = raw_child(
        directory,
        name,
        FILE_READ_ATTRIBUTES,
        ALL_SHARING,
        FILE_OPEN,
        false,
    )
    .map_err(io_error)?;
    // SAFETY: inspect only a live disk handle opened with OPEN_REPARSE_POINT.
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
        return Err(invalid());
    }
    let mut data = [0u8; 16 * 1024];
    let mut returned = 0u32;
    // FSCTL_GET_REPARSE_POINT reads the link object, never its target. The
    // returned byte count and all variable offsets are checked before decoding.
    // SAFETY: both output pointers refer to live writable bounded storage.
    if unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            0x000900a8,
            null(),
            0,
            data.as_mut_ptr().cast(),
            data.len() as u32,
            &mut returned,
            null_mut(),
        )
    } == 0
    {
        return Err(last_error());
    }
    let data = data.get(..returned as usize).ok_or_else(invalid)?;
    match reparse_target_size(data)? {
        Some(size) => Ok(("symlink", size)),
        // Unknown reparse types are not traversable device directories.
        None => Ok(("other", 0)),
    }
}

pub(super) fn entries_bounded(
    file: Opened,
    limit: usize,
    control: Option<(&std::sync::atomic::AtomicBool, Instant)>,
) -> Result<(Vec<Entry>, bool), String> {
    let mut entries = Vec::new();
    let mut buffer = vec![0u64; 8192];
    loop {
        if control.is_some_and(|(cancelled, deadline)| {
            cancelled.load(std::sync::atomic::Ordering::Acquire) || Instant::now() >= deadline
        }) {
            return Err("Search cancelled or expired".into());
        }
        buffer.fill(0);
        // SAFETY: the aligned buffer is 64 KiB and the independent directory
        // cursor is owned exclusively by this call; no filename is opened.
        if unsafe {
            GetFileInformationByHandleEx(
                file.file.as_raw_handle(),
                FileIdBothDirectoryInfo,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                return Ok((entries, false));
            }
            return Err(io_error(error));
        }
        let bytes = buffer.len() * 8;
        let mut offset = 0usize;
        loop {
            if offset
                .checked_add(size_of::<FILE_ID_BOTH_DIR_INFO>())
                .is_none_or(|end| end > bytes)
            {
                return Err(invalid());
            }
            // SAFETY: the full fixed structure is within the allocated buffer;
            // read_unaligned also handles an unexpected kernel entry alignment.
            let info = unsafe {
                buffer
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FILE_ID_BOTH_DIR_INFO>()
                    .read_unaligned()
            };
            let end = if info.NextEntryOffset == 0 {
                bytes
            } else {
                offset
                    .checked_add(info.NextEntryOffset as usize)
                    .ok_or_else(invalid)?
            };
            let start = offset + offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            let name_end = start
                .checked_add(info.FileNameLength as usize)
                .ok_or_else(invalid)?;
            if end > bytes
                || end <= offset
                || name_end > end
                || info.FileNameLength == 0
                || info.FileNameLength % 2 != 0
            {
                return Err(invalid());
            }
            // Copy UTF-16 units individually, without assuming filename alignment.
            let units: Vec<u16> = (start..name_end)
                .step_by(2)
                .map(|index| unsafe {
                    buffer
                        .as_ptr()
                        .cast::<u8>()
                        .add(index)
                        .cast::<u16>()
                        .read_unaligned()
                })
                .collect();
            let name = String::from_utf16(&units)
                .map_err(|_| "Directory contains a filename not representable as UTF-8")?;
            if name != "." && name != ".." {
                if control.is_some_and(|(cancelled, deadline)| {
                    cancelled.load(std::sync::atomic::Ordering::Acquire)
                        || Instant::now() >= deadline
                }) {
                    return Err("Search cancelled or expired".into());
                }
                if entries.len() >= limit {
                    return Ok((entries, true));
                }
                let (kind, size) = if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    reparse_entry(&file.file, &name)?
                } else if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    ("directory", 0)
                } else {
                    ("file", info.EndOfFile.max(0) as u64)
                };
                entries.push(Entry { name, kind, size });
            }
            if info.NextEntryOffset == 0 {
                break;
            }
            offset = end;
        }
    }
}

pub(super) struct Parent<'a> {
    root: &'a Root,
    relative: String,
    directory: Opened,
}
impl<'a> Parent<'a> {
    fn directory(root: &'a Root, relative: &str) -> Result<Self, String> {
        let relative = normalize(relative)?;
        // A mutation needs a pinned parent, not permission to list its contents.
        // Requesting LIST_DIRECTORY here would reject write-only drop folders.
        let directory = root.open_access(&relative, DIRECTORY_ACCESS, true)?;
        Ok(Self {
            root,
            relative,
            directory,
        })
    }
    pub(super) fn file(root: &'a Root, path: &str, create: bool) -> Result<(Self, String), String> {
        let path = normalize(path)?;
        if path == "." {
            return Err("A file name is required".into());
        }
        let (relative, name) = path.rsplit_once('/').unwrap_or((".", &path));
        if create {
            make_directories(root, relative)?;
        }
        Ok((Self::directory(root, relative)?, name.into()))
    }
    fn verify(&self) -> Result<(), String> {
        let named = self
            .root
            .open_access(&self.relative, DIRECTORY_ACCESS, true)?;
        if metadata(&named)?.identity != metadata(&self.directory)?.identity {
            return Err("Directory path changed while performing file operation".into());
        }
        Ok(())
    }
    fn file_handle(&self) -> &File {
        &self.directory.file
    }
}
fn named(parent: &File, name: &str) -> Result<Option<Snapshot>, String> {
    match child(
        parent,
        name,
        FILE_READ_ATTRIBUTES,
        ALL_SHARING,
        FILE_OPEN,
        false,
    ) {
        Ok(file) => snapshot(&file).map(Some),
        Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}
fn verify_file(parent: &Parent<'_>, name: &str, expected: Snapshot) -> Result<(), String> {
    parent.verify()?;
    if !named(parent.file_handle(), name)?.is_some_and(|current| current.same_version(expected)) {
        return Err("File changed while performing file operation".into());
    }
    Ok(())
}

pub(super) fn make_directories(root: &Root, path: &str) -> Result<(), String> {
    let normalized = normalize(path)?;
    let parts: Vec<_> = if normalized == "." {
        Vec::new()
    } else {
        normalized.split('/').collect()
    };
    if parts.len() > 128 {
        return Err("Directory creation exceeds 128 path components".into());
    }
    let mut relative = String::from(".");
    for name in parts {
        let parent = Parent::directory(root, &relative)?;
        parent.verify()?;
        let directory = child(
            parent.file_handle(),
            name,
            DIRECTORY_ACCESS,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN_IF,
            true,
        )
        .map_err(io_error)?;
        if !snapshot(&directory)?.directory() {
            return Err(invalid());
        }
        parent.verify()?;
        relative = if relative == "." {
            name.into()
        } else {
            format!("{relative}/{name}")
        };
    }
    // Windows has no supported directory FlushFileBuffers guarantee. File data
    // is flushed below; this backend does not claim directory power-loss durability.
    root.verify()
}

pub(super) struct Existing {
    pub(super) bytes: Vec<u8>,
    snapshot: Snapshot,
    _file: File,
}
pub(super) fn read_existing(parent: &Parent<'_>, name: &str) -> Result<Option<Existing>, String> {
    parent.verify()?;
    // Hold read access without write sharing through the final rename. A
    // concurrent writer is rejected, while deletion sharing permits replacement.
    let file = match child(
        parent.file_handle(),
        name,
        GENERIC_READ,
        FILE_SHARE_READ | FILE_SHARE_DELETE,
        FILE_OPEN,
        false,
    ) {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let before = snapshot(&file)?;
    let mut opened = Opened {
        file,
        _ancestors: Vec::new(),
    };
    let bytes = read_full(&mut opened)?;
    verify_file(parent, name, before)?;
    Ok(Some(Existing {
        bytes,
        snapshot: before,
        _file: opened.file,
    }))
}

fn rename_file(file: &File, parent: &File, name: &str, replace: bool) -> Result<(), io::Error> {
    let name = wide_name(name).map_err(io::Error::other)?;
    let length = size_of::<FILE_RENAME_INFORMATION>() + name.len() * 2;
    let mut buffer = vec![0usize; length.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut block = IO_STATUS_BLOCK::default();
    // SAFETY: buffer is aligned and includes the fixed structure and filename;
    // both handles and all pointers remain live through the synchronous rename.
    let status = unsafe {
        (*info).Anonymous.Flags = if replace {
            FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS
        } else {
            0
        };
        (*info).RootDirectory = parent.as_raw_handle();
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            buffer
                .as_mut_ptr()
                .cast::<u8>()
                .add(offset_of!(FILE_RENAME_INFORMATION, FileName))
                .cast::<u16>(),
            name.len(),
        );
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut block,
            info.cast(),
            length as u32,
            if replace {
                FileRenameInformationEx
            } else {
                FileRenameInformation
            },
        )
    };
    if status != 0 {
        Err(nt_error(status))
    } else {
        Ok(())
    }
}
struct Temporary<'a> {
    parent: &'a Parent<'a>,
    name: String,
    file: File,
    may_cleanup: bool,
    rename_attempted: bool,
}
impl<'a> Temporary<'a> {
    fn create(parent: &'a Parent<'a>) -> Result<Self, String> {
        let name = crate::random::hex_token::<16>()
            .map(|value| format!(".rdh-{value}"))
            .map_err(|_| "Secure randomness is unavailable")?;
        let file = raw_child(
            parent.file_handle(),
            &name,
            GENERIC_READ | GENERIC_WRITE | DELETE,
            FILE_SHARE_READ,
            FILE_CREATE,
            false,
        )
        .map_err(io_error)?;
        let mut temporary = Self {
            parent,
            name,
            file,
            may_cleanup: true,
            rename_attempted: false,
        };
        if let Err(error) = snapshot(&temporary.file) {
            let cleanup = temporary.remove();
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; temporary cleanup failed: {cleanup}"),
            });
        }
        Ok(temporary)
    }
    fn remove(&mut self) -> Result<(), String> {
        if !self.may_cleanup {
            return Ok(());
        }
        // Handle deletion is safe only while the held object still has its
        // unique temporary name. A failed rename can have an uncertain outcome.
        self.may_cleanup = false;
        if self.rename_attempted {
            let Some(current) = named(self.parent.file_handle(), &self.name)? else {
                return Ok(());
            };
            if current.identity != snapshot(&self.file)?.identity {
                return Err("Temporary file changed".into());
            }
        }
        let mut basic = FILE_BASIC_INFO::default();
        // A pre-publication failure can occur before the filesystem provides a
        // usable file ID. The exclusive creation handle still identifies our
        // object, so basic attributes suffice for that cleanup path.
        // SAFETY: the owned handle and aligned, sized output are live.
        if unsafe {
            GetFileInformationByHandleEx(
                self.file.as_raw_handle(),
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(last_error());
        }
        if basic.FileAttributes & FILE_ATTRIBUTE_READONLY != 0 {
            let attributes = basic.FileAttributes & !FILE_ATTRIBUTE_READONLY;
            let clear = FILE_BASIC_INFO {
                FileAttributes: if attributes == 0 {
                    FILE_ATTRIBUTE_NORMAL
                } else {
                    attributes
                },
                ..Default::default()
            };
            // SAFETY: only our still-owned temporary has its readonly bit cleared.
            if unsafe {
                SetFileInformationByHandle(
                    self.file.as_raw_handle(),
                    FileBasicInfo,
                    (&clear as *const FILE_BASIC_INFO).cast(),
                    size_of::<FILE_BASIC_INFO>() as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
        }
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: a live owned temporary handle and correctly sized input.
        if unsafe {
            SetFileInformationByHandle(
                self.file.as_raw_handle(),
                FileDispositionInfo,
                (&info as *const FILE_DISPOSITION_INFO).cast::<c_void>(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(last_error());
        }
        Ok(())
    }
}
impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

pub(super) fn write_atomic(
    parent: &Parent<'_>,
    name: &str,
    bytes: &[u8],
    existing: Option<&Existing>,
) -> Result<(), String> {
    parent.verify()?;
    let mut temporary = Temporary::create(parent)?;
    let result = (|| {
        temporary.file.write_all(bytes).map_err(io_error)?;
        if existing.is_some_and(|old| old.snapshot.attributes & FILE_ATTRIBUTE_READONLY != 0) {
            let info = FILE_BASIC_INFO {
                FileAttributes: FILE_ATTRIBUTE_READONLY,
                ..Default::default()
            };
            // SAFETY: the owned file and fixed-size attribute structure are live.
            if unsafe {
                SetFileInformationByHandle(
                    temporary.file.as_raw_handle(),
                    FileBasicInfo,
                    (&info as *const FILE_BASIC_INFO).cast(),
                    size_of::<FILE_BASIC_INFO>() as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
        }
        temporary.file.sync_all().map_err(io_error)?;
        parent.verify()?;
        let own = snapshot(&temporary.file)?;
        if !named(parent.file_handle(), &temporary.name)?
            .is_some_and(|value| value.same_version(own))
        {
            return Err("Temporary file changed".into());
        }
        if let Some(old) = existing {
            verify_file(parent, name, old.snapshot)?;
        } else if named(parent.file_handle(), name)?.is_some() {
            return Err("File already exists".into());
        }
        // There is no cross-process digest compare-and-swap. As on POSIX,
        // observed changes are rejected before the atomic namespace operation.
        temporary.rename_attempted = true;
        rename_file(
            &temporary.file,
            parent.file_handle(),
            name,
            existing.is_some(),
        )
        .map_err(io_error)?;
        temporary.may_cleanup = false;
        parent.verify()?;
        if !named(parent.file_handle(), name)?.is_some_and(|value| value.identity == own.identity) {
            return Err("Committed file changed".into());
        }
        Ok(())
    })();
    let cleanup = temporary.remove();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(format!("{error}; temporary cleanup also failed: {cleanup}"))
        }
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

pub(super) fn move_file(root: &Root, source: &str, destination: &str) -> Result<(), String> {
    let (source_parent, source_name) = Parent::file(root, source, false)?;
    let (target_parent, target_name) = Parent::file(root, destination, false)?;
    let source_file = child(
        source_parent.file_handle(),
        &source_name,
        GENERIC_READ | DELETE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        FILE_OPEN,
        false,
    )
    .map_err(io_error)?;
    let before = snapshot(&source_file)?;
    if before.directory() {
        return Err("Only regular files can be moved".into());
    }
    verify_file(&source_parent, &source_name, before)?;
    target_parent.verify()?;
    if named(target_parent.file_handle(), &target_name)?.is_some() {
        return Err("Destination already exists".into());
    }
    if before.identity.volume != snapshot(target_parent.file_handle())?.identity.volume {
        return Err("Moving files across filesystems is disabled".into());
    }
    rename_file(
        &source_file,
        target_parent.file_handle(),
        &target_name,
        false,
    )
    .map_err(|error| match error.raw_os_error().map(|code| code as u32) {
        Some(ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS) => "Destination already exists".into(),
        Some(ERROR_NOT_SAME_DEVICE) => "Moving files across filesystems is disabled".into(),
        _ => io_error(error),
    })?;
    source_parent.verify()?;
    target_parent.verify()?;
    if !named(target_parent.file_handle(), &target_name)?
        .is_some_and(|value| value.identity == before.identity && !value.directory())
    {
        return Err("Moved file changed".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    struct Fixture {
        path: PathBuf,
        files: Files,
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rdh-win-files-{}",
                crate::random::hex_token::<12>().unwrap()
            ));
            fs::create_dir(&path).unwrap();
            let roots = crate::config::local_roots(&[format!("work={}", path.display())]).unwrap();
            let files = Files::new(&roots, true).unwrap();
            Self { path, files }
        }
        fn call(&self, operation: &str, mut params: Value) -> Result<Value, String> {
            params["root"] = json!("work");
            self.files.dispatch(operation, &params)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.files.roots.clear();
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn windows_names_and_drive_unc_roots_keep_the_protocol_boundary() {
        for bad in [
            "CON",
            "con.txt",
            "AUX.",
            "NUL x",
            "COM¹",
            "LPT9.log",
            "CONIN$",
            "CONOUT$.log",
            "a:b",
            "bad\\name",
            "bad\u{1f}",
            "name.",
            "name ",
            "a?b",
            "a*b",
        ] {
            assert!(normalize(bad).is_err(), "accepted {bad:?}");
        }
        for valid in [
            "", ".", "a//./b", "COM0", "COM10", "console", "file😀", " leading",
        ] {
            assert!(normalize(valid).is_ok(), "rejected {valid:?}");
        }
        for root in [r"C:\work", "C:/", r"\\server\share\work", "//server/share"] {
            assert!(root_parts(root).is_ok(), "{root:?}");
        }
        for root in [
            "C:relative",
            "/work",
            "//server",
            "//server/",
            r"\\?\C:\work",
            r"\\.\C:\work",
            r"\??\C:\work",
            "//?/UNC/server/share",
            "C:/a/../b",
        ] {
            assert!(root_parts(root).is_err(), "{root:?}");
        }
        let fixture = Fixture::new();
        for bad in [
            "../escape",
            "/absolute",
            "sub/../../escape",
            "bad\0name",
            "file:stream",
            "dir\\file",
            "NUL",
            "CON.txt",
            "trailing.",
        ] {
            assert!(fixture.call("read_file", json!({"path":bad})).is_err());
            assert!(
                fixture
                    .call("write_file", json!({"path":bad,"content":"must not exist"}))
                    .is_err()
            );
        }
        assert_eq!(fs::read_dir(&fixture.path).unwrap().count(), 0);
    }

    #[test]
    fn directory_handles_pin_roots_and_all_intermediate_names() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.path.join("a/b")).unwrap();
        fs::write(fixture.path.join("a/b/file"), b"inside").unwrap();
        let root = &fixture.files.roots["work"];
        let mut file = root.open("a/b/file", false).unwrap();
        assert!(fs::rename(&fixture.path, fixture.path.with_extension("moved")).is_err());
        assert!(fs::rename(fixture.path.join("a"), fixture.path.join("other")).is_err());
        assert!(fs::rename(fixture.path.join("a/b"), fixture.path.join("a/other")).is_err());
        assert_eq!(read_full(&mut file).unwrap(), b"inside");
        drop(file);
        fs::rename(fixture.path.join("a"), fixture.path.join("other")).unwrap();
        assert!(root.open("a/b/file", false).is_err());
        assert_eq!(
            read_full(&mut root.open("other/b/file", false).unwrap()).unwrap(),
            b"inside"
        );
    }

    #[test]
    fn reparse_leaves_ancestors_and_root_aliases_are_rejected() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        fs::write(outside.path.join("secret"), b"outside").unwrap();
        std::os::windows::fs::symlink_file(outside.path.join("secret"), fixture.path.join("link"))
            .unwrap();
        std::os::windows::fs::symlink_dir(&outside.path, fixture.path.join("directory-link"))
            .unwrap();
        for name in ["link", "directory-link"] {
            assert!(
                fs::symlink_metadata(fixture.path.join(name))
                    .expect("the native test fixture must create a real symbolic link")
                    .is_symlink()
            );
        }
        for path in ["link", "directory-link/secret"] {
            assert!(fixture.call("read_file", json!({"path":path})).is_err());
            assert!(fixture.call("file_info", json!({"path":path})).is_err());
            assert!(
                fixture
                    .call("write_file", json!({"path":path,"content":"no"}))
                    .is_err()
            );
            assert!(
                fixture
                    .call("move_file", json!({"source":path,"destination":"moved"}))
                    .is_err()
            );
        }
        assert!(
            fixture
                .call("list_directory", json!({"path":"directory-link"}))
                .is_err()
        );
        assert!(Root::new(fixture.path.join("directory-link").to_str().unwrap()).is_err());
        let listing = fixture.call("list_directory", json!({})).unwrap();
        assert!(
            listing["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["type"] == "symlink")
        );
        assert_eq!(fs::read(outside.path.join("secret")).unwrap(), b"outside");
    }

    #[test]
    fn create_update_move_and_readonly_failure_preserve_files_and_cleanup() {
        let fixture = Fixture::new();
        let first = fixture
            .call(
                "write_file",
                json!({"path":"nested/file","content":"before"}),
            )
            .unwrap();
        assert!(
            fixture
                .call("write_file", json!({"path":"nested/file","content":"bad"}))
                .is_err()
        );
        assert!(
            fixture
                .call(
                    "write_file",
                    json!({"path":"nested/file","content":"bad","expected_sha256":"incorrect"})
                )
                .is_err()
        );
        let after = fixture
            .call(
                "write_file",
                json!({"path":"nested/file","content":"after😀","expected_sha256":first["sha256"]}),
            )
            .unwrap();
        assert_eq!(
            fixture
                .call("file_info", json!({"path":"nested/file"}))
                .unwrap()["mode"],
            0o666
        );
        fixture
            .call(
                "move_file",
                json!({"source":"nested/file","destination":"nested/moved"}),
            )
            .unwrap();
        fixture
            .call(
                "move_file",
                json!({"path":"nested/moved","destination_root":"work","destination_path":"final"}),
            )
            .unwrap();
        fs::write(fixture.path.join("occupied"), b"keep").unwrap();
        assert!(
            fixture
                .call(
                    "move_file",
                    json!({"source":"final","destination":"occupied"})
                )
                .is_err()
        );
        assert_eq!(fs::read(fixture.path.join("occupied")).unwrap(), b"keep");
        assert_eq!(
            fs::read_to_string(fixture.path.join("final")).unwrap(),
            "after😀"
        );
        let path = fixture.path.join("final");
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();
        assert_eq!(
            fixture.call("file_info", json!({"path":"final"})).unwrap()["mode"],
            0o444
        );
        let result = fixture.call(
            "write_file",
            json!({"path":"final","content":"must fail","expected_sha256":after["sha256"]}),
        );
        // Restore fixture permissions before asserting so unwinding can clean up.
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        // Windows changes only FILE_ATTRIBUTE_READONLY here.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&path, permissions).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "after😀");
        assert!(fs::read_dir(&fixture.path).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".rdh-")
        }));
    }

    #[test]
    fn mutation_snapshot_excludes_writers_until_publication_finishes() {
        let fixture = Fixture::new();
        fs::write(fixture.path.join("file"), b"before").unwrap();
        let root = &fixture.files.roots["work"];
        let (parent, name) = Parent::file(root, "file", false).unwrap();
        let existing = read_existing(&parent, &name).unwrap().unwrap();
        assert!(fs::write(fixture.path.join("file"), b"concurrent").is_err());
        write_atomic(&parent, &name, b"after", Some(&existing)).unwrap();
        assert_eq!(fs::read(fixture.path.join("file")).unwrap(), b"after");
        drop(existing);
        fs::write(fixture.path.join("file"), b"later").unwrap();
    }

    #[test]
    fn mutations_do_not_require_directory_listing_permission() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
        };
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
            OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
            UNPROTECTED_DACL_SECURITY_INFORMATION,
        };
        use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC};

        struct Local(*mut c_void);
        impl Drop for Local {
            fn drop(&mut self) {
                // SAFETY: the conversion/security APIs allocate these blocks with LocalAlloc.
                unsafe { LocalFree(self.0) };
            }
        }
        fn set_dacl(file: &File, descriptor: &Local) -> io::Result<()> {
            let mut present = 0;
            let mut defaulted = 0;
            let mut acl = null_mut();
            let mut control = 0;
            let mut revision = 0;
            // SAFETY: the descriptor owns a valid API result; all outputs are live.
            if unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
            } == 0
                || unsafe {
                    GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision)
                } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if present == 0 || acl.is_null() {
                return Err(io::Error::other("Fixture DACL is missing"));
            }
            let information = DACL_SECURITY_INFORMATION
                | if control & SE_DACL_PROTECTED != 0 {
                    PROTECTED_DACL_SECURITY_INFORMATION
                } else {
                    UNPROTECTED_DACL_SECURITY_INFORMATION
                };
            // SAFETY: file retains WRITE_DAC even while the fixture ACL changes;
            // acl is borrowed from descriptor for this synchronous call.
            let status = unsafe {
                SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    information,
                    null_mut(),
                    null_mut(),
                    acl,
                    null_mut(),
                )
            };
            if status == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(status as i32))
            }
        }
        struct Restore {
            file: File,
            original: Local,
            changed: bool,
        }
        impl Restore {
            fn restore(&mut self) -> io::Result<()> {
                if self.changed {
                    set_dacl(&self.file, &self.original)?;
                    self.changed = false;
                }
                Ok(())
            }
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Err(error) = self.restore() {
                    eprintln!("Could not restore the temporary directory ACL: {error}");
                }
            }
        }

        let fixture = Fixture::new();
        fs::create_dir(fixture.path.join("drop")).unwrap();
        let file = fs::OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .share_mode(ALL_SHARING)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(fixture.path.join("drop"))
            .unwrap();
        let mut original = null_mut();
        let mut owner = null_mut();
        // SAFETY: file is a live temporary directory; the returned owner is
        // borrowed from the owned descriptor until LocalFree.
        assert_eq!(
            unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut owner,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    &mut original,
                )
            },
            0
        );
        assert!(!original.is_null());
        let mut restore = Restore {
            file,
            original: Local(original),
            changed: false,
        };
        let mut owner_text = null_mut();
        // SAFETY: owner belongs to the still-live original descriptor.
        assert_ne!(unsafe { ConvertSidToStringSidW(owner, &mut owner_text) }, 0);
        assert!(!owner_text.is_null());
        let owner_memory = Local(owner_text.cast());
        let mut length = 0;
        // SAFETY: the API returns a terminated textual SID, bounded below.
        while length < 256 && unsafe { *owner_text.add(length) } != 0 {
            length += 1;
        }
        assert!(length < 256);
        let owner =
            String::from_utf16(unsafe { std::slice::from_raw_parts(owner_text, length) }).unwrap();
        drop(owner_memory);
        // Only the directory denies listing; new children inherit the owner's
        // ordinary full access, so file reads/writes remain independently valid.
        let sddl: Vec<u16> = format!("D:P(D;;0x1;;;{owner})(A;OICI;FA;;;{owner})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut restricted = null_mut();
        // SAFETY: terminated SDDL and writable output remain live for conversion.
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut restricted,
                    null_mut(),
                )
            },
            0
        );
        let restricted = Local(restricted);
        restore.changed = true;
        set_dacl(&restore.file, &restricted).unwrap();

        assert!(
            fixture
                .call("list_directory", json!({"path":"drop"}))
                .is_err()
        );
        let created = fixture
            .call("write_file", json!({"path":"drop/file","content":"before"}))
            .unwrap();
        fixture
            .call(
                "write_file",
                json!({"path":"drop/file","content":"after","expected_sha256":created["sha256"]}),
            )
            .unwrap();
        fixture
            .call("make_directory", json!({"path":"drop/nested"}))
            .unwrap();
        fixture
            .call(
                "move_file",
                json!({"source":"drop/file","destination":"drop/nested/moved"}),
            )
            .unwrap();
        assert_eq!(
            fs::read(fixture.path.join("drop/nested/moved")).unwrap(),
            b"after"
        );
        assert!(
            fixture
                .call("list_directory", json!({"path":"drop"}))
                .is_err()
        );
        restore.restore().unwrap();
        assert_eq!(
            fixture
                .call("list_directory", json!({"path":"drop"}))
                .unwrap()["entries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn uncertain_publication_cleanup_never_deletes_the_published_object() {
        let fixture = Fixture::new();
        let root = &fixture.files.roots["work"];
        let (parent, name) = Parent::file(root, "published", false).unwrap();
        let mut temporary = Temporary::create(&parent).unwrap();
        temporary.file.write_all(b"published").unwrap();
        // Simulate an OS rename which committed before its success was lost.
        temporary.rename_attempted = true;
        rename_file(&temporary.file, parent.file_handle(), &name, false).unwrap();
        fs::write(fixture.path.join(&temporary.name), b"different object").unwrap();
        assert!(temporary.remove().is_err());
        drop(temporary);
        assert_eq!(
            fs::read(fixture.path.join("published")).unwrap(),
            b"published"
        );
    }

    #[test]
    fn independent_directory_cursors_and_bounded_reads_keep_results_stable() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.path.join("folder")).unwrap();
        for index in 0..350 {
            fs::write(fixture.path.join(format!("file-{index:03}")), b"data").unwrap();
        }
        let root = &fixture.files.roots["work"];
        let (small, truncated) = entries_bounded(root.open(".", true).unwrap(), 2, None).unwrap();
        assert_eq!(small.len(), 2);
        assert!(truncated);
        let full = entries_bounded(root.open(".", true).unwrap(), 1000, None).unwrap();
        assert_eq!(full.0.len(), 351);
        assert!(!full.1);
        assert_eq!(
            full.0
                .iter()
                .find(|entry| entry.name == "folder")
                .unwrap()
                .size,
            0
        );
        let large = fs::File::create(fixture.path.join("large")).unwrap();
        large.set_len(1024 * 1024 + 1).unwrap();
        assert!(
            fixture
                .call("read_file", json!({"path":"large"}))
                .unwrap_err()
                .contains("1 MiB")
        );
        assert_eq!(
            fixture
                .call("read_file_chunk", json!({"path":"large","length":1}))
                .unwrap()["size_bytes"],
            1024 * 1024 + 1
        );
        large.set_len(64 * 1024 * 1024 + 1).unwrap();
        assert!(
            fixture
                .call("read_file_chunk", json!({"path":"large","length":1}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn background_search_reads_nested_files_and_closes_its_workers() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.path.join("nested")).unwrap();
        fs::write(fixture.path.join("nested/target"), "Straße 😀 needle").unwrap();
        let roots =
            crate::config::local_roots(&[format!("work={}", fixture.path.display())]).unwrap();
        let manager = crate::searches::Searches::new(Arc::new(Files::new(&roots, false).unwrap()));
        let start = manager
            .start(
                &json!({"root":"work","query":"Straße","content":true}),
                Instant::now() + Duration::from_secs(10),
                &AtomicBool::new(false),
            )
            .unwrap();
        let id = start["search_id"].as_str().unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let result = manager.get(&json!({"search_id":id})).unwrap();
                if result["status"] != "queued" && result["status"] != "running" {
                    break result;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["results"][0]["text"], "Straße 😀 needle");
        assert!(
            result["results"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("nested/target")
        );
        assert!(manager.close(Duration::from_secs(3)).await);
    }
}
