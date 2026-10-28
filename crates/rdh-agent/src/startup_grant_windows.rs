//! Administrator grants are public-readable authorization, never user-owned secrets.
//! Every ancestor stays open without delete sharing while the grant is inspected.
use super::*;
use crate::startup::Mode;
use std::path::PathBuf;
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::System::SystemServices::ACCESS_DENIED_ACE_TYPE;

struct Trusted {
    system: Sid,
    administrators: Sid,
    installer: LocalMemory,
}
impl Trusted {
    fn new() -> Result<Self> {
        let text: Vec<u16> = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut installer = null_mut();
        // SAFETY: terminated SID text and a writable LocalAlloc output.
        if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut installer) } == 0
            || installer.is_null()
        {
            return Err(last_error());
        }
        let installer = LocalMemory(installer);
        Ok(Self {
            system: Sid::well_known(WinLocalSystemSid)?,
            administrators: Sid::well_known(WinBuiltinAdministratorsSid)?,
            installer,
        })
    }
    fn contains(&self, sid: PSID) -> bool {
        !sid.is_null()
            && unsafe { IsValidSid(sid) } != 0
            && [
                self.system.as_ptr(),
                self.administrators.as_ptr(),
                self.installer.0,
            ]
            .into_iter()
            .any(|trusted| unsafe { EqualSid(sid, trusted) } != 0)
    }
}
fn untrusted() -> Error {
    Error::message("Managed startup grant owner, ACL or path cannot be verified")
}

fn validate(trusted: &Trusted, owner: PSID, acl: *mut ACL, boundary: bool) -> Result<()> {
    if !trusted.contains(owner) || acl.is_null() || unsafe { IsValidAcl(acl) } == 0 {
        return Err(untrusted());
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    if unsafe {
        GetAclInformation(
            acl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
        || info.AceCount > 256
    {
        return Err(untrusted());
    }
    let start = acl as usize;
    let end = start
        .checked_add(unsafe { (*acl).AclSize } as usize)
        .ok_or_else(untrusted)?;
    // Boundary files/directories may not be writable by an ordinary user.
    // Earlier ancestors may allow new children but not ownership/DACL changes
    // or deletion of the retained next component, matching the old grant rule.
    let forbidden = if boundary { 0x000d_0156 } else { 0x000c_0040 };
    for index in 0..info.AceCount {
        let mut raw = null_mut();
        if unsafe { GetAce(acl, index, &mut raw) } == 0 || raw.is_null() {
            return Err(untrusted());
        }
        let offset = raw as usize;
        if offset < start
            || offset
                .checked_add(size_of::<ACE_HEADER>())
                .is_none_or(|value| value > end)
        {
            return Err(untrusted());
        }
        let header = unsafe { std::ptr::read_unaligned(raw.cast::<ACE_HEADER>()) };
        let length = usize::from(header.AceSize);
        if offset.checked_add(length).is_none_or(|value| value > end) || length < 16 {
            return Err(untrusted());
        }
        if u32::from(header.AceType) == ACCESS_DENIED_ACE_TYPE {
            continue;
        }
        if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE {
            return Err(untrusted());
        }
        let bytes = unsafe { std::slice::from_raw_parts(raw.cast::<u8>(), length) };
        if 16 + usize::from(bytes[9]) * 4 > length {
            return Err(untrusted());
        }
        let sid = unsafe { raw.cast::<u8>().add(8).cast::<c_void>() };
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(untrusted());
        }
        let mask = u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| untrusted())?);
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE == 0
            && !trusted.contains(sid)
            && mask & (forbidden | 0x4000_0000 | 0x1000_0000) != 0
        {
            return Err(untrusted());
        }
    }
    Ok(())
}
fn check(file: &File, boundary: bool) -> Result<()> {
    let trusted = Trusted::new()?;
    let mut owner = null_mut();
    let mut acl = null_mut();
    let mut descriptor = null_mut();
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
    let _descriptor = LocalMemory(descriptor);
    validate(&trusted, owner, acl, boundary)
}
fn descriptor(directory: bool) -> Result<LocalMemory> {
    let inherit = if directory { "OICI" } else { "" };
    let text: Vec<u16> =
        format!("O:BAG:BAD:P(A;{inherit};FA;;;SY)(A;{inherit};FA;;;BA)(A;{inherit};0x1200a9;;;AU)")
            .encode_utf16()
            .chain(Some(0))
            .collect();
    let mut raw = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), 1, &mut raw, null_mut())
    } == 0
        || raw.is_null()
    {
        return Err(last_error());
    }
    Ok(LocalMemory(raw))
}

fn config_text(config: &Path) -> Result<String> {
    let text = config
        .to_str()
        .filter(|_| config.is_absolute())
        .ok_or_else(|| Error::message("Invalid startup grant config path"))?;
    // Rust canonicalization adds the extended drive prefix; the legacy Agent
    // hashes and stores the ordinary, already-canonical Windows spelling.
    Ok(text.strip_prefix(r"\\?\").unwrap_or(text).to_owned())
}
fn grant_path(config: &Path) -> Result<PathBuf> {
    let base = std::env::var_os("ProgramData")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    Ok(base.join("RemoteDeviceHubStartupGrants").join(format!(
        "{}.json",
        crate::config::sha256_hex(config_text(config)?.as_bytes())
    )))
}

fn parent(path: &Path, create: bool) -> Result<(Vec<File>, Vec<u16>)> {
    let (root, parts) = path_parts(path)?;
    let mut chain = vec![open_root(&root, true)?];
    check(&chain[0], parts.len() == 1)?;
    for (index, name) in parts[..parts.len() - 1].iter().enumerate() {
        let boundary = index + 2 == parts.len();
        let directory = chain.last().expect("drive exists");
        let opened = match open_child(directory, name, true, true) {
            Ok(file) => file,
            Err(Error::Io(error)) if boundary && create && definitely_not_found(&error) => {
                let descriptor = descriptor(true)?;
                match create_child_with_descriptor(directory, name, true, &descriptor, |file| {
                    check(file, true)
                }) {
                    Ok(file) => file,
                    Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                        open_child(directory, name, true, true)?
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        check(&opened, boundary)?;
        chain.push(opened);
    }
    Ok((chain, parts.last().expect("named path").clone()))
}
fn absent(error: &Error) -> bool {
    matches!(error,Error::Io(error) if definitely_not_found(error))
}

fn read_at(path: &Path, config: &Path, mode: Mode) -> Result<bool> {
    // Absence does not grant authority. Preserve local approval on machines
    // whose unused ProgramData path cannot satisfy the grant trust policy.
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let (chain, name) = match parent(path, false) {
        Ok(value) => value,
        Err(error) if absent(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    let file = match open_child(chain.last().expect("parent exists"), &name, false, true) {
        Ok(file) => file,
        Err(error) if absent(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    check(&file, true)?;
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    crate::startup_grant::permits(&bytes, Path::new(&config_text(config)?), mode)
}
pub(crate) fn managed_grant(config: &Path, mode: Mode) -> Result<bool> {
    read_at(&grant_path(config)?, config, mode)
}

fn set_at(path: &Path, config: &Path, mode: Mode) -> Result<()> {
    let (chain, name) = match parent(path, mode != Mode::None) {
        Ok(value) => value,
        Err(error) if mode == Mode::None && absent(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    let parent = chain.last().expect("parent exists");
    if mode == Mode::None {
        let file = match open_child_access(parent, &name, false, true, true) {
            Ok(file) => file,
            Err(error) if absent(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        check(&file, true)?;
        delete_open_object(&file)?;
        drop(file);
        return require_child_absent(parent, &name, false);
    }
    let existing = match open_child_request(
        parent,
        &name,
        false,
        GENERIC_READ | READ_CONTROL | SYNCHRONIZE,
        FILE_SHARE_READ | FILE_SHARE_DELETE,
    ) {
        Ok(file) => {
            check(&file, true)?;
            let identity = file_identity(&file)?;
            Some((file, identity))
        }
        Err(error) if absent(&error) => None,
        Err(error) => return Err(error),
    };
    let temp_name: Vec<u16> = format!(".rdh-grant-{}.tmp", crate::random::hex_token::<16>()?)
        .encode_utf16()
        .collect();
    let descriptor = descriptor(false)?;
    let mut temporary = Temporary {
        file: create_child_with_descriptor(parent, &temp_name, false, &descriptor, |file| {
            check(file, true)
        })?,
        installed_or_deleted: false,
    };
    let staged = (|| {
        serde_json::to_writer(
            &mut temporary.file,
            &serde_json::json!({"version":1,"config":config_text(config)?,"modes":[mode]}),
        )?;
        temporary.file.write_all(b"\n")?;
        temporary.file.sync_all()?;
        check(&temporary.file, true)?;
        check(parent, true)?;
        if let Some((file, identity)) = &existing {
            check(file, true)?;
            if named_identity(parent, &name)?.as_ref() != Some(identity) {
                return Err(Error::message("Managed grant changed during update"));
            }
        }
        Ok(())
    })();
    if let Err(error) = staged {
        return failed_publication(temporary, parent, &temp_name, false, error);
    }
    temporary.installed_or_deleted = true;
    match rename_file(&temporary.file, parent, &name, existing.is_some()) {
        Ok(()) => Ok(()),
        Err(error) => failed_publication(temporary, parent, &temp_name, true, error),
    }
}
pub(crate) fn set_grant(config: &Path, mode: Mode) -> Result<()> {
    if !crate::startup_native::windows::administrator() {
        return Err(Error::message(
            "Administrator privileges are required to grant managed startup",
        ));
    }
    set_at(&grant_path(config)?, config, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{GetSecurityDescriptorDacl, GetSecurityDescriptorOwner};
    fn accepts(sddl: &str, boundary: bool) -> bool {
        let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut descriptor,
                    null_mut(),
                )
            },
            0
        );
        let descriptor = LocalMemory(descriptor);
        let mut owner = null_mut();
        let mut acl = null_mut();
        let mut defaulted = 0;
        let mut present = 0;
        assert_ne!(
            unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted) },
            0
        );
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
            },
            0
        );
        validate(&Trusted::new().unwrap(), owner, acl, boundary).is_ok()
    }
    #[test]
    fn grants_allow_public_reads_but_only_admins_may_change_authority() {
        assert!(accepts("O:BAD:P(A;;FA;;;BA)(A;;FR;;;AU)", true));
        assert!(accepts("O:SYD:P(A;;FA;;;SY)(A;;0x0002;;;AU)", false));
        assert!(!accepts("O:SYD:P(A;;FA;;;SY)(A;;0x0002;;;AU)", true));
        assert!(!accepts("O:AUD:P(A;;FA;;;BA)(A;;FR;;;AU)", true));
        for mask in ["GW", "GA", "WD", "WO", "DC"] {
            assert!(!accepts(
                &format!("O:BAD:P(A;;FA;;;BA)(A;;{mask};;;AU)"),
                true
            ));
        }
        assert!(!accepts("O:BAD:NO_ACCESS_CONTROL", true));
    }
    #[test]
    fn canonical_config_uses_the_legacy_hash_spelling() {
        assert_eq!(
            config_text(Path::new(r"\\?\C:\Users\fixture\config.json")).unwrap(),
            r"C:\Users\fixture\config.json"
        );
    }

    #[test]
    #[ignore = "requires an elevated native Windows fixture under ProgramData"]
    fn native_windows_grant_creation_replacement_denial_and_revocation() {
        assert!(
            crate::startup_native::windows::administrator(),
            "Grant fixture requires administrator authority"
        );
        let base =
            PathBuf::from(std::env::var_os("ProgramData").expect("ProgramData")).join(format!(
                "RDH-Test-Grant-{}",
                crate::random::hex_token::<12>().unwrap()
            ));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let (ancestors, directory_name) = open_parent(&base, false, false).unwrap();
        for ancestor in &ancestors {
            check(ancestor, false).unwrap();
        }
        let security = descriptor(true).unwrap();
        drop(
            create_child_with_descriptor(
                ancestors.last().unwrap(),
                &directory_name,
                true,
                &security,
                |file| check(file, true),
            )
            .unwrap(),
        );
        drop(ancestors);
        let _cleanup = Cleanup(base.clone());
        let config = Path::new(r"C:\rdh-test-grant-config.json");
        let path = base.join(format!(
            "{}.json",
            crate::config::sha256_hex(config_text(config).unwrap().as_bytes())
        ));
        assert!(!read_at(&path, config, Mode::User).unwrap());
        set_at(&path, config, Mode::User).unwrap();
        assert!(read_at(&path, config, Mode::User).unwrap());
        assert!(!read_at(&path, config, Mode::Service).unwrap());
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            stored,
            serde_json::json!({"version":1,"config":config.to_str(),"modes":["user"]})
        );
        set_at(&path, config, Mode::Service).unwrap();
        assert!(read_at(&path, config, Mode::Service).unwrap());
        let alias = base.join("alias.json");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(read_at(&path, config, Mode::Service).is_err());
        std::fs::remove_file(alias).unwrap();
        set_at(&path, config, Mode::None).unwrap();
        assert!(!path.exists());
        assert!(!read_at(&path, config, Mode::Service).unwrap());

        set_at(&path, config, Mode::User).unwrap();
        let (chain, name) = parent(&path, false).unwrap();
        let file = open_child_request(
            chain.last().unwrap(),
            &name,
            false,
            READ_CONTROL | windows_sys::Win32::Storage::FileSystem::WRITE_DAC | SYNCHRONIZE,
            FILE_SHARE_READ,
        )
        .unwrap();
        let text: Vec<u16> = "D:P(A;;FA;;;BA)(A;;FA;;;SY)(A;;FW;;;AU)"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut raw = null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut raw,
                    null_mut(),
                )
            },
            0
        );
        let descriptor = LocalMemory(raw);
        let mut acl = null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
            },
            0
        );
        assert_eq!(
            unsafe {
                windows_sys::Win32::Security::Authorization::SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION
                        | windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    acl,
                    null_mut(),
                )
            },
            ERROR_SUCCESS
        );
        drop(file);
        drop(chain);
        assert!(read_at(&path, config, Mode::User).is_err());
        assert!(set_at(&path, config, Mode::Service).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
