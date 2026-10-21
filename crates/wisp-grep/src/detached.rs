//! Start a background daemon without leaking the caller's handles.
//!
//! A long-lived child that inherits a caller's stdout pipe keeps that pipe
//! open after the launcher exits. Piped callers (CI capture, agent harnesses,
//! `wg up | tee`) then wait for EOF forever. On Windows, `CreateProcessW`
//! with `bInheritHandles = TRUE` copies *every* inheritable handle, not only
//! the standard handles. This module passes an explicit handle list, so the
//! child inherits exactly its stdin (NUL) and its log file. On Unix, a
//! `pre_exec` hook closes every descriptor >= 3, so the child also starts with
//! only NUL (stdin) and the log (stdout and stderr).

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// A spawned daemon. Dropping it never stops the daemon.
pub struct DetachedChild {
    pid: u32,
    #[cfg(windows)]
    handle: isize,
    #[cfg(unix)]
    child: std::process::Child,
}

impl DetachedChild {
    /// Process id of the daemon.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Exit code when the daemon has exited, otherwise `None`.
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        #[cfg(windows)]
        {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn WaitForSingleObject(handle: isize, millis: u32) -> u32;
                fn GetExitCodeProcess(handle: isize, code: *mut u32) -> i32;
            }
            const WAIT_TIMEOUT: u32 = 258;
            // SAFETY: the handle is owned by this value and stays valid until
            // `Drop` closes it.
            let waited = unsafe { WaitForSingleObject(self.handle, 0) };
            if waited == WAIT_TIMEOUT {
                return Ok(None);
            }
            let mut code = 0u32;
            // SAFETY: same handle; `code` is a valid out pointer.
            if unsafe { GetExitCodeProcess(self.handle, &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Some(code as i32))
        }
        #[cfg(unix)]
        {
            Ok(self
                .child
                .try_wait()?
                .map(|status| status.code().unwrap_or(-1)))
        }
    }
}

#[cfg(windows)]
impl Drop for DetachedChild {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        // SAFETY: this value owns the process handle.
        unsafe {
            CloseHandle(self.handle as *mut std::ffi::c_void);
        }
    }
}

/// Spawn `exe args...` detached. Its stdin is NUL; stdout and stderr append to
/// `log`. The child inherits no other handle from the caller.
pub fn spawn(exe: &Path, args: &[OsString], log: &Path) -> io::Result<DetachedChild> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(windows)]
    {
        windows::spawn(exe, args, log)
    }
    #[cfg(unix)]
    {
        unix::spawn(exe, args, log)
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsString;
    use std::io;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::DetachedChild;

    pub(super) fn spawn(exe: &Path, args: &[OsString], log: &Path) -> io::Result<DetachedChild> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let mut command = Command::new(exe);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file);
        // Rust sets close-on-exec on the descriptors it opens, and the three
        // standard descriptors now point at NUL or the log. A descriptor >= 3
        // that the caller made inheritable would still reach the daemon, so
        // the child marks all of them close-on-exec before `exec`. Marking
        // instead of closing keeps the standard library's own close-on-exec
        // error pipe working, so a failed `exec` is still reported by
        // `spawn`. A new process group lets the daemon outlive a terminal
        // session.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: the hook only calls syscalls; it does not allocate or
            // take a lock, so it is safe between `fork` and `exec`.
            unsafe {
                command.pre_exec(cloexec_extra_fds);
            }
            command.process_group(0);
        }
        let child = command.spawn()?;
        let pid = child.id();
        Ok(DetachedChild { pid, child })
    }

    /// Mark every descriptor >= 3 close-on-exec in the forked child. Must be
    /// async-signal-safe: syscalls only, no allocation and no locks.
    #[cfg(target_os = "linux")]
    fn cloexec_extra_fds() -> io::Result<()> {
        const CLOSE_RANGE_CLOEXEC: libc::c_int = 1 << 2;
        // SAFETY: `close_range` takes three scalar arguments and touches no
        // process memory. Kernels before 5.11 return ENOSYS or EINVAL; a libc
        // without the wrapper is why this uses `syscall` directly.
        let marked = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            )
        };
        if marked == 0 {
            return Ok(());
        }
        cloexec_by_limit()
    }

    /// Fallback for Linux kernels without `CLOSE_RANGE_CLOEXEC` and for other
    /// Unix.
    #[cfg(not(target_os = "linux"))]
    fn cloexec_extra_fds() -> io::Result<()> {
        cloexec_by_limit()
    }

    fn cloexec_by_limit() -> io::Result<()> {
        // SAFETY: `getrlimit` writes one `rlimit` struct owned by this frame.
        let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let max = if limit.rlim_cur == libc::RLIM_INFINITY {
            // No finite bound. Real Unix systems use a finite limit; this cap
            // is larger than any practical table.
            1_048_576
        } else if limit.rlim_cur > libc::c_int::MAX as libc::rlim_t {
            libc::c_int::MAX
        } else {
            limit.rlim_cur as libc::c_int
        };
        let mut fd: libc::c_int = 3;
        while fd < max {
            // SAFETY: `fcntl` on an unopened descriptor only returns EBADF.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
            fd += 1;
        }
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::{OsString, c_void};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::ptr;

    use super::DetachedChild;

    const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
    const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_chars: u32,
        y_chars: u32,
        fill: u32,
        flags: u32,
        show: u16,
        reserved2: u16,
        reserved_ptr: *mut u8,
        std_input: isize,
        std_output: isize,
        std_error: isize,
    }

    #[repr(C)]
    struct StartupInfoExW {
        info: StartupInfoW,
        attributes: *mut c_void,
    }

    #[repr(C)]
    struct ProcessInformation {
        process: isize,
        thread: isize,
        pid: u32,
        tid: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateProcessW(
            application: *const u16,
            command_line: *mut u16,
            process_attributes: *mut c_void,
            thread_attributes: *mut c_void,
            inherit_handles: i32,
            flags: u32,
            environment: *mut c_void,
            cwd: *const u16,
            startup: *const c_void,
            process: *mut ProcessInformation,
        ) -> i32;
        fn InitializeProcThreadAttributeList(
            list: *mut c_void,
            count: u32,
            flags: u32,
            size: *mut usize,
        ) -> i32;
        fn UpdateProcThreadAttribute(
            list: *mut c_void,
            flags: u32,
            attribute: usize,
            value: *const c_void,
            size: usize,
            previous: *mut c_void,
            returned: *mut usize,
        ) -> i32;
        fn DeleteProcThreadAttributeList(list: *mut c_void);
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
    }

    fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Quote one argument with the same rules the C runtime uses.
    fn append_arg(command_line: &mut Vec<u16>, arg: &std::ffi::OsStr) {
        let text: Vec<u16> = arg.encode_wide().collect();
        let quote = text.is_empty()
            || text
                .iter()
                .any(|c| matches!(*c, 0x20 | 0x09 | 0x0a | 0x0b | 0x22));
        if !quote {
            command_line.extend_from_slice(&text);
            return;
        }
        command_line.push(b'"' as u16);
        let mut backslashes = 0usize;
        for c in text {
            if c == b'\\' as u16 {
                backslashes += 1;
            } else if c == b'"' as u16 {
                command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
                command_line.push(c);
                backslashes = 0;
            } else {
                command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
                command_line.push(c);
                backslashes = 0;
            }
        }
        command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
        command_line.push(b'"' as u16);
    }

    fn inheritable(file: &std::fs::File) -> io::Result<isize> {
        let handle = file.as_raw_handle() as isize;
        // SAFETY: the handle belongs to `file` and outlives this call.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(handle)
    }

    pub(super) fn spawn(exe: &Path, args: &[OsString], log: &Path) -> io::Result<DetachedChild> {
        let nul = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("NUL")?;
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let handles = [inheritable(&nul)?, inheritable(&log_file)?];

        let mut command_line = Vec::new();
        append_arg(&mut command_line, exe.as_os_str());
        for arg in args {
            command_line.push(b' ' as u16);
            append_arg(&mut command_line, arg);
        }
        command_line.push(0);
        let application = wide(exe.as_os_str());

        // The attribute list is the only way to stop CreateProcessW from
        // copying every inheritable handle the parent holds.
        let mut size = 0usize;
        // SAFETY: the first call with a null list only reports the size.
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
        }
        let words = size.div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0usize; words];
        let list = buffer.as_mut_ptr() as *mut c_void;
        // SAFETY: `buffer` is at least `size` bytes and suitably aligned.
        if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `handles` outlives the Update and CreateProcess calls.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                handles.as_ptr() as *const c_void,
                std::mem::size_of_val(&handles),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if updated == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: `list` was initialized above.
            unsafe { DeleteProcThreadAttributeList(list) };
            return Err(error);
        }

        let mut info = StartupInfoExW {
            info: StartupInfoW {
                cb: std::mem::size_of::<StartupInfoExW>() as u32,
                reserved: ptr::null_mut(),
                desktop: ptr::null_mut(),
                title: ptr::null_mut(),
                x: 0,
                y: 0,
                x_size: 0,
                y_size: 0,
                x_chars: 0,
                y_chars: 0,
                fill: 0,
                flags: STARTF_USESTDHANDLES,
                show: 0,
                reserved2: 0,
                reserved_ptr: ptr::null_mut(),
                std_input: handles[0],
                std_output: handles[1],
                std_error: handles[1],
            },
            attributes: list,
        };
        let mut process = ProcessInformation {
            process: 0,
            thread: 0,
            pid: 0,
            tid: 0,
        };
        let flags = EXTENDED_STARTUPINFO_PRESENT
            | CREATE_NO_WINDOW
            | CREATE_NEW_PROCESS_GROUP
            | DETACHED_PROCESS;
        // SAFETY: every pointer refers to a live value for the duration of the
        // call; bInheritHandles is TRUE because the whitelisted handles must be
        // inheritable, but the handle list restricts inheritance to them.
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                1,
                flags,
                ptr::null_mut(),
                ptr::null(),
                &mut info as *mut StartupInfoExW as *const c_void,
                &mut process,
            )
        };
        let error = io::Error::last_os_error();
        // SAFETY: `list` was initialized above.
        unsafe { DeleteProcThreadAttributeList(list) };
        drop(nul);
        drop(log_file);
        if created == 0 {
            return Err(error);
        }
        // SAFETY: CreateProcessW returned a valid thread handle.
        unsafe { CloseHandle(process.thread as *mut c_void) };
        Ok(DetachedChild {
            pid: process.pid,
            handle: process.process,
        })
    }
}
