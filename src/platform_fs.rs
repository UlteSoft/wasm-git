//! Filesystem operations absent from Rust's WASIp1 std implementation.

#[cfg(not(target_os = "wasi"))]
pub(crate) use std::fs::canonicalize;

/// Serialize service requests without preventing ordinary reads of the format
/// inode. Windows byte-range locks are mandatory even for another handle in
/// this process, so reserve one byte beyond EOF instead of locking its contents.
pub(crate) fn try_service_lock(file: &std::fs::File) -> Result<(), std::fs::TryLockError> {
    #[cfg(not(windows))]
    {
        file.try_lock()
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
        use windows_sys::Win32::Storage::FileSystem::{
            LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
        };
        use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
        let mut position = OVERLAPPED {
            Anonymous: OVERLAPPED_0 {
                Anonymous: OVERLAPPED_0_0 {
                    Offset: 0,
                    OffsetHigh: 1,
                },
            },
            ..Default::default()
        };
        // SAFETY: the live File owns this synchronous handle. FAIL_IMMEDIATELY
        // prevents a pending asynchronous operation referencing stack memory.
        // The lock is released when the returned service guard File is closed.
        let locked = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut position,
            )
        };
        if locked != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Err(std::fs::TryLockError::WouldBlock)
        } else {
            Err(std::fs::TryLockError::Error(error))
        }
    }
}

/// WASIp1 has no process identifier. This diagnostic field is never a lock
/// ownership proof; exclusive file creation, timestamps and counters arbitrate
/// staging names. Zero records the absence of a host PID without panicking.
pub(crate) fn process_id() -> u32 {
    #[cfg(target_os = "wasi")]
    {
        0
    }
    #[cfg(not(target_os = "wasi"))]
    {
        std::process::id()
    }
}

/// wasi-libc resolves symlinks and checks path existence within the runtime's
/// preopened directories. Its virtual cwd is `/`; callers must preopen the
/// workspace there (for example, Wasmtime `--dir .::/`). No host path is exposed.
#[cfg(target_os = "wasi")]
pub(crate) fn canonicalize(
    path: impl AsRef<std::path::Path>,
) -> std::io::Result<std::path::PathBuf> {
    use std::ffi::{c_char, c_void, CStr, CString, OsStr};
    use std::os::wasi::ffi::OsStrExt;
    unsafe extern "C" {
        fn realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char;
        fn free(ptr: *mut c_void);
    }
    let path = CString::new(path.as_ref().as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: input is NUL-terminated. realpath(NULL) allocates a NUL-terminated
    // result with libc malloc, copied before freeing with the matching allocator.
    unsafe {
        let result = realpath(path.as_ptr(), std::ptr::null_mut());
        if result.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let path = std::path::PathBuf::from(OsStr::from_bytes(CStr::from_ptr(result).to_bytes()));
        free(result.cast());
        Ok(path)
    }
}
