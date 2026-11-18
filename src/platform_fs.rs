//! Filesystem operations absent from Rust's WASIp1 std implementation.

#[cfg(not(target_os = "wasi"))]
pub(crate) use std::fs::canonicalize;

pub(crate) const NATIVE_LOCK_FORMAT: &[u8] = b"WGTLCK\0\x02";

pub(crate) struct ResourceLock {
    _file: std::fs::File,
    #[cfg(unix)]
    lock_path: std::path::PathBuf,
    #[cfg(target_os = "wasi")]
    legacy_path: std::path::PathBuf,
}

impl ResourceLock {
    pub(crate) fn relocated(&mut self, path: &std::path::Path) {
        #[cfg(target_os = "wasi")]
        {
            self.legacy_path = path.to_owned();
        }
        #[cfg(unix)]
        {
            self.lock_path = path.to_owned();
        }
        #[cfg(not(any(unix, target_os = "wasi")))]
        let _ = path;
    }
}

#[cfg(unix)]
impl Drop for ResourceLock {
    fn drop(&mut self) {
        // A concurrent fork can temporarily inherit this open file
        // description before CLOEXEC takes effect. Unlock explicitly when the
        // operation ends instead of waiting for that child to exec or exit.
        if let Err(error) = self._file.unlock() {
            crate::cli_error(&format!("cannot release lock {}: {error}", self.lock_path.display()));
        }
    }
}

#[cfg(target_os = "wasi")]
impl Drop for ResourceLock {
    fn drop(&mut self) {
        // WASIp1 has no native process lock. This guard created the path
        // exclusively; a crash can leave it for controlled maintenance.
        let _ = std::fs::remove_file(&self.legacy_path);
    }
}

/// A stable inode arbitrates one named resource. Existing legacy create-new
/// locks are never interpreted as abandoned, even when their PID is absent.
#[cfg(any(unix, windows))]
pub(crate) fn acquire_lock(
    path: &std::path::Path,
    staging: &std::path::Path,
) -> Result<ResourceLock, String> {
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let error = |action: &str, cause: std::io::Error| {
        format!("cannot {action} {}: {cause}", path.display())
    };
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let staged = staging.join(format!(
                "native-lock-{}-{time}-{}",
                process_id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            crate::output::write_new_file(&staged, NATIVE_LOCK_FORMAT)?;
            // Atomic create-if-absent protects an old writer which won the
            // race and ensures no initialized native inode is replaced.
            let published = std::fs::hard_link(&staged, path);
            std::fs::remove_file(&staged).map_err(|cause| {
                format!(
                    "cannot remove lock staging file {}: {cause}",
                    staged.display()
                )
            })?;
            match published {
                Ok(()) => crate::output::sync_directory(
                    path.parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or(std::path::Path::new(".")),
                )?,
                Err(cause) if cause.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(cause) => return Err(error("publish lock", cause)),
            }
        }
        Err(cause) => return Err(error("inspect lock", cause)),
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|cause| error("inspect lock", cause))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!("invalid lock: {}", path.display()));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    protect_lock_open(&mut options);
    let file = options
        .open(path)
        .map_err(|cause| error("open lock", cause))?;
    try_service_lock(&file)
        .map_err(|cause| format!("resource is locked: {}: {cause}", path.display()))?;
    let mut guard = ResourceLock {
        _file: file,
        #[cfg(unix)]
        lock_path: path.to_owned(),
    };
    if !lock_file_matches_path(&guard._file, path).map_err(|cause| error("verify lock", cause))? {
        return Err(format!("invalid or replaced lock: {}", path.display()));
    }
    let mut marker = Vec::new();
    (&mut guard._file)
        .take((NATIVE_LOCK_FORMAT.len() + 1) as u64)
        .read_to_end(&mut marker)
        .map_err(|cause| error("read lock", cause))?;
    if marker != NATIVE_LOCK_FORMAT {
        return Err(format!("resource is locked by an incompatible or legacy lock: {}; stop every process using this resource, verify no operation remains active, and remove only the legacy lock under controlled maintenance before retrying", path.display()));
    }
    Ok(guard)
}

/// WASIp1 has no native file locking. Preserve exclusive-create semantics,
/// but never open or remove an existing native v2 inode. Both formats compete
/// for this same pathname, so different backends cannot silently coexist.
#[cfg(target_os = "wasi")]
pub(crate) fn acquire_lock(
    path: &std::path::Path,
    _staging: &std::path::Path,
) -> Result<ResourceLock, String> {
    use std::io::Write;
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                format!("resource is locked or uses the native v2 lock format: {}; WASIp1 cannot acquire an existing lock and must not remove a native lock", path.display())
            } else {
                format!("cannot create lock {}: {error}", path.display())
            }
        })?;
    let mut guard = ResourceLock {
        _file: file,
        legacy_path: path.to_owned(),
    };
    guard
        ._file
        .write_all(&[b"WGTLCK\0\x01".as_slice(), &0u32.to_be_bytes()].concat())
        .map_err(|error| format!("cannot write lock {}: {error}", path.display()))?;
    guard
        ._file
        .sync_all()
        .map_err(|error| format!("cannot sync lock {}: {error}", path.display()))?;
    Ok(guard)
}

#[cfg(not(any(unix, windows, target_os = "wasi")))]
pub(crate) fn acquire_lock(
    _path: &std::path::Path,
    _staging: &std::path::Path,
) -> Result<ResourceLock, String> {
    Err("repository locking is unavailable on this target".into())
}

/// The lock's directory must be protected against out-of-band replacement.
/// Keep Windows' normal sharing so a held worktree lock can move with its tree.
pub(crate) fn protect_lock_open(options: &mut std::fs::OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Refuse a final-component symlink and avoid waiting on a substituted
        // FIFO between inspection and open.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    let _ = options;
}

#[cfg(unix)]
pub(crate) fn lock_file_matches_path(
    file: &std::fs::File,
    path: &std::path::Path,
) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let path_metadata = std::fs::symlink_metadata(path)?;
    let file_metadata = file.metadata()?;
    Ok(path_metadata.is_file()
        && !path_metadata.file_type().is_symlink()
        && file_metadata.is_file()
        && path_metadata.dev() == file_metadata.dev()
        && path_metadata.ino() == file_metadata.ino())
}

#[cfg(windows)]
pub(crate) fn lock_file_matches_path(
    file: &std::fs::File,
    path: &std::path::Path,
) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    fn identity(file: &std::fs::File) -> std::io::Result<(u32, u32, u32)> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the live file handle and writable information struct are valid.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || !file.metadata()?.is_file() {
        return Ok(false);
    }
    let path_file = std::fs::File::open(path)?;
    Ok(identity(file)? == identity(&path_file)?)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn lock_file_matches_path(
    _file: &std::fs::File,
    _path: &std::path::Path,
) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "native repository writer locks are unavailable",
    ))
}

/// Serialize native guards without preventing ordinary reads of their files.
/// Windows byte-range locks are mandatory even for another handle in this
/// process, so reserve one byte beyond EOF instead of locking its contents.
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

#[cfg(all(test, target_os = "linux"))]
mod lock_tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".tmp");
            fs::create_dir_all(&root).unwrap();
            let path = root.join(format!("native-lock-fork-test-{}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    struct ChildGuard { pid: libc::pid_t, reaped: bool }
    impl ChildGuard {
        fn wait(&mut self) -> std::io::Result<i32> {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut status = 0;
            loop {
                // SAFETY: pid is the child returned by this test's fork.
                let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
                if result == self.pid { self.reaped = true; return Ok(status); }
                if result < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ECHILD) { self.reaped = true; }
                    if error.raw_os_error() != Some(libc::EINTR) { return Err(error); }
                }
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "child did not exit"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !self.reaped {
                // SAFETY: this is our unreaped child, so its PID cannot be reused.
                unsafe { libc::kill(self.pid, libc::SIGKILL); }
                if let Err(error) = self.wait() { eprintln!("could not reap test child: {error}"); }
            }
        }
    }

    fn pipe() -> (File, File) {
        let mut fds = [-1; 2];
        // SAFETY: fds points to two writable integers; ownership is transferred
        // to exactly one File per descriptor after successful pipe2.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    #[test]
    fn normal_drop_unlocks_even_while_forked_child_waits_before_exec() {
        let temp = Temp::new();
        let path = temp.0.join("write.lock");
        let guard = acquire_lock(&path, &temp.0).unwrap();
        let (drop_tx, drop_rx) = mpsc::channel();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            let _ = drop_rx.recv();
            drop(guard);
            let _ = dropped_tx.send(());
        });
        let (mut notify_read, notify_write) = pipe();
        let (release_read, mut release_write) = pipe();
        let notify_read_fd = notify_read.as_raw_fd();
        let notify_write_fd = notify_write.as_raw_fd();
        let release_read_fd = release_read.as_raw_fd();
        let release_write_fd = release_write.as_raw_fd();
        // SAFETY: another thread owns the guard. After fork the child calls
        // only async-signal-safe libc functions before exec or _exit.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                libc::close(notify_read_fd);
                libc::close(release_write_fd);
                let ready = [1u8];
                if libc::write(notify_write_fd, ready.as_ptr().cast(), 1) != 1 { libc::_exit(120); }
                let mut resume = [0u8];
                if libc::read(release_read_fd, resume.as_mut_ptr().cast(), 1) != 1 { libc::_exit(121); }
                let argv = [c"true".as_ptr(), std::ptr::null()];
                let env = [std::ptr::null()];
                libc::execve(c"/bin/true".as_ptr(), argv.as_ptr(), env.as_ptr());
                libc::_exit(122);
            }
        }
        assert!(pid > 0, "fork failed");
        let mut child = ChildGuard { pid, reaped: false };
        drop(notify_write);
        drop(release_read);
        let (ready_tx, ready_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            let _ = ready_tx.send(notify_read.read_exact(&mut byte));
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        assert!(acquire_lock(&path, &temp.0).is_err());
        drop_tx.send(()).unwrap();
        dropped_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        owner.join().unwrap();
        // The child is definitely alive and has not yet executed CLOEXEC.
        let reacquired = acquire_lock(&path, &temp.0).unwrap();
        drop(reacquired);
        release_write.write_all(&[1]).unwrap();
        drop(release_write);
        let status = child.wait().unwrap();
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        reader.join().unwrap();
    }
}
