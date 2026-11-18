//! Operating-system identities for local administrative actions.

use crate::protocol::AuditOperator;
use std::path::Path;

#[cfg(unix)]
fn unix_operator(uid: libc::uid_t) -> Result<AuditOperator, String> {
    let operator = AuditOperator {
        version: 1,
        identity: format!("unix:uid:{uid}"),
    };
    operator.validate()?;
    Ok(operator)
}

#[cfg(unix)]
pub(super) fn current_operator() -> Result<AuditOperator, String> {
    // SAFETY: geteuid has no preconditions and reports the process's effective UID.
    unix_operator(unsafe { libc::geteuid() })
}

#[cfg(windows)]
pub(super) fn current_operator() -> Result<AuditOperator, String> {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr;
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_INSUFFICIENT_BUFFER};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct LocalWide(*mut u16);
    impl Drop for LocalWide {
        fn drop(&mut self) {
            // SAFETY: ConvertSidToStringSidW allocated this buffer with LocalAlloc.
            unsafe { LocalFree(self.0.cast()) };
        }
    }

    let mut raw_token = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle and raw_token is writable.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(format!(
            "cannot open process token: {}",
            io::Error::last_os_error()
        ));
    }
    if raw_token.is_null() {
        return Err("process token handle is null".into());
    }
    // SAFETY: successful OpenProcessToken returns an owned, non-null handle.
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };

    let mut needed = 0u32;
    // SAFETY: the initial zero-length query only writes the required size.
    let query = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    let cause = io::Error::last_os_error();
    if query != 0 {
        return Err("process token user size query unexpectedly succeeded without a buffer".into());
    }
    if cause.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(format!("cannot query process token user size: {cause}"));
    }
    if needed < std::mem::size_of::<TOKEN_USER>() as u32 || needed > 64 * 1024 {
        return Err("invalid process token user size".into());
    }
    // TOKEN_USER contains pointers, so allocate with pointer alignment.
    let words = (needed as usize).div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0usize; words];
    // SAFETY: buffer is aligned and contains at least `needed` writable bytes.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(format!(
            "cannot read process token user: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: the successful query populated a TOKEN_USER and its SID in buffer.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = ptr::null_mut();
    // SAFETY: user.User.Sid is the SID supplied by the successful token query.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(format!(
            "cannot convert process token SID: {}",
            io::Error::last_os_error()
        ));
    }
    if sid.is_null() {
        return Err("process token SID string is null".into());
    }
    let sid = LocalWide(sid);
    let mut length = 0usize;
    // SAFETY: ConvertSidToStringSidW returned a null-terminated UTF-16 string.
    while unsafe { *sid.0.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: length was measured up to the string terminator in that allocation.
    let wide = unsafe { std::slice::from_raw_parts(sid.0, length) };
    let text =
        String::from_utf16(wide).map_err(|error| format!("invalid process token SID: {error}"))?;
    let operator = AuditOperator {
        version: 1,
        identity: format!("windows:sid:{text}"),
    };
    operator.validate()?;
    Ok(operator)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn current_operator() -> Result<AuditOperator, String> {
    Err("local operator identity is unavailable on this platform".into())
}

#[cfg(target_os = "linux")]
fn check_recovery_owner(uid: u32, owner: u32) -> Result<(), String> {
    if uid != 0 && uid != owner {
        return Err("recovery requires root or the server root owner".into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn authorize_recovery_root(root: &Path) -> Result<AuditOperator, String> {
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;

    if !root.is_absolute() || root.components().any(|part| part == Component::ParentDir) {
        return Err("recovery root must be an absolute path without '..'".into());
    }
    let inspect = |path: &Path| {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect recovery path {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "recovery path is not a real directory: {}",
                path.display()
            ));
        }
        Ok(metadata)
    };
    // Capture the UID once so the authorization and audit identity agree.
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let metadata = inspect(root)?;
    let owner = metadata.uid();
    check_recovery_owner(uid, owner)?;
    if metadata.mode() & 0o022 != 0 {
        return Err("recovery root must not be group- or other-writable".into());
    }
    for ancestor in root.ancestors().skip(1) {
        let metadata = inspect(ancestor)?;
        if metadata.uid() != 0 && metadata.uid() != uid && metadata.uid() != owner {
            return Err(format!(
                "recovery ancestor has an untrusted owner: {}",
                ancestor.display()
            ));
        }
        // A sticky directory protects each trusted-owned child entry against
        // replacement by other users; each child is inspected in this loop.
        if metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0 {
            return Err(format!(
                "recovery ancestor is writable by other users: {}",
                ancestor.display()
            ));
        }
    }
    unix_operator(uid)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn authorize_recovery_root(_root: &Path) -> Result<AuditOperator, String> {
    Err("local recovery authorization is only available on Linux".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn operator_is_the_effective_uid() {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        assert_eq!(
            current_operator().unwrap().identity,
            format!("unix:uid:{uid}")
        );
    }

    #[cfg(windows)]
    #[test]
    fn operator_is_a_windows_sid() {
        let operator = current_operator().unwrap();
        assert!(operator.identity.starts_with("windows:sid:S-"));
        operator.validate().unwrap();
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn recovery_is_unavailable_on_other_platforms() {
        assert!(authorize_recovery_root(Path::new(".")).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_requires_root_or_directory_owner() {
        assert!(check_recovery_owner(0, 1000).is_ok());
        assert!(check_recovery_owner(1000, 1000).is_ok());
        assert!(check_recovery_owner(1000, 1001).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_checks_root_ancestors_and_symlinks() {
        use std::fs;
        use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        struct Temp(std::path::PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        let base = std::env::temp_dir().join(format!(
            "wasm-git-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        let temp = Temp(base);
        let parent = temp.0.join("parent");
        let root = parent.join("root");
        fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();

        assert_eq!(
            authorize_recovery_root(&root).unwrap().identity,
            current_operator().unwrap().identity
        );
        fs::set_permissions(&root, fs::Permissions::from_mode(0o720)).unwrap();
        assert!(authorize_recovery_root(&root).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o702)).unwrap();
        assert!(authorize_recovery_root(&root).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(authorize_recovery_root(&root).is_err());
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(authorize_recovery_root(&root).is_ok());
        symlink(&root, temp.0.join("link")).unwrap();
        assert!(authorize_recovery_root(&temp.0.join("link")).is_err());
    }
}
