#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "macOS process FFI is required for process metadata")]
mod imp {
    pub fn macos_proc_cwd(child_pid: u32) -> Option<std::path::PathBuf> {
        use std::ffi::CStr;
        use std::mem::MaybeUninit;
        use std::os::raw::c_void;

        const PROC_PIDVNODEPATHINFO: i32 = 9;

        // `proc_vnodepathinfo` is 2 * `vnode_info_path` (each 1152 bytes) = 2304 bytes.
        // `vnode_info_path` = `vnode_info` (128 bytes) + path `[c_char; 1024]`.
        // `pvi_cdir` is the first `vnode_info_path` member; its path starts at byte 128.
        const VIP_PATH_OFFSET: usize = 128;
        const VNODE_INFO_PATH_SIZE: usize = 1152;
        const PROC_VNODEPATHINFO_SIZE: usize = VNODE_INFO_PATH_SIZE * 2;

        unsafe extern "C" {
            fn proc_pidinfo(
                pid: i32,
                flavor: i32,
                arg: u64,
                buffer: *mut c_void,
                buffersize: i32,
            ) -> i32;
        }

        let mut buf = MaybeUninit::<[u8; PROC_VNODEPATHINFO_SIZE]>::uninit();

        let ret = unsafe {
            proc_pidinfo(
                i32::try_from(child_pid).ok()?,
                PROC_PIDVNODEPATHINFO,
                0,
                buf.as_mut_ptr().cast::<c_void>(),
                i32::try_from(PROC_VNODEPATHINFO_SIZE).ok()?,
            )
        };

        if ret <= 0 {
            return None;
        }

        let buf = unsafe { buf.assume_init() };
        let path_bytes = buf.get(VIP_PATH_OFFSET..VNODE_INFO_PATH_SIZE)?;
        let c_str = CStr::from_bytes_until_nul(path_bytes).ok()?;
        let path = std::path::PathBuf::from(c_str.to_str().ok()?);

        if path.as_os_str().is_empty() {
            return None;
        }

        Some(path)
    }

    /// Read `pid`'s process start time as `(seconds, microseconds)`.
    ///
    /// Backs the cross-platform child-identity token in
    /// [`crate::child_identity`]: the kernel never rewrites a live process's
    /// start time, so the pair (PID, start time) survives PID reuse.
    pub fn macos_proc_start_time(pid: i32) -> Option<(u64, u64)> {
        use std::mem::MaybeUninit;
        use std::os::raw::c_void;

        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();

        let ret = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast::<c_void>(),
                size,
            )
        };

        // `proc_pidinfo` returns the number of bytes written; a short write
        // means the struct was not fully populated.
        if ret != size {
            return None;
        }

        let info = unsafe { info.assume_init() };
        Some((info.pbi_start_tvsec, info.pbi_start_tvusec))
    }

    pub fn macos_proc_exe_path(pid: i32) -> Option<std::path::PathBuf> {
        use std::ffi::CStr;

        let mut buf = vec![0u8; usize::try_from(libc::PROC_PIDPATHINFO_MAXSIZE).ok()?];

        let ret = unsafe {
            libc::proc_pidpath(
                pid,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                u32::try_from(buf.len()).ok()?,
            )
        };
        if ret <= 0 {
            return None;
        }

        let path = CStr::from_bytes_until_nul(&buf).ok()?.to_str().ok()?;
        if path.is_empty() {
            return None;
        }
        Some(std::path::PathBuf::from(path))
    }

    pub fn macos_proc_args(pid: i32) -> Option<Vec<Vec<u8>>> {
        const MAX_PROCARGS2_BYTES: usize = 1024 * 1024;

        let mut mib: [libc::c_int; 3] = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mib_len = libc::c_uint::try_from(mib.len()).ok()?;
        let mut len: libc::size_t = 0;

        let size_result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib_len,
                std::ptr::null_mut(),
                std::ptr::from_mut(&mut len),
                std::ptr::null_mut(),
                0,
            )
        };
        if size_result == -1 || len == 0 || len > MAX_PROCARGS2_BYTES {
            return None;
        }

        let mut buf = vec![0u8; len];
        let mut actual_len = len;
        let args_result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib_len,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                std::ptr::from_mut(&mut actual_len),
                std::ptr::null_mut(),
                0,
            )
        };
        if args_result == -1 {
            return None;
        }

        let actual_len = actual_len.min(buf.len());
        buf.truncate(actual_len);
        parse_procargs2_argv(&buf)
    }

    fn parse_procargs2_argv(buf: &[u8]) -> Option<Vec<Vec<u8>>> {
        let argc_size = std::mem::size_of::<libc::c_int>();
        if argc_size != 4 {
            return None;
        }

        // `get` rather than an index: a short buffer is a "cannot parse"
        // answer, not a panic, and it subsumes the old length check.
        let argc = i32::from_ne_bytes(buf.get(..argc_size)?.try_into().ok()?);
        if argc < 0 {
            return None;
        }

        // Skip the executable path and the NUL padding that follows it.
        let mut data = buf.get(argc_size..)?;
        let exe_end = data.iter().position(|byte| *byte == 0)?;
        data = skip_leading_nuls(data.get(exe_end + 1..)?);

        let mut parsed = Vec::new();
        for _ in 0..argc {
            if data.is_empty() {
                break;
            }
            let arg_end = data.iter().position(|byte| *byte == 0).unwrap_or(data.len());
            let arg = data.get(..arg_end)?;
            if !arg.is_empty() {
                parsed.push(arg.to_vec());
            }
            // Past the final argument there is no separator to step over, so an
            // out-of-range tail is the empty slice rather than a failure.
            data = skip_leading_nuls(data.get(arg_end + 1..).unwrap_or(&[]));
        }

        Some(parsed)
    }

    /// Advances past the NUL padding `KERN_PROCARGS2` writes between entries.
    fn skip_leading_nuls(mut data: &[u8]) -> &[u8] {
        while data.first() == Some(&0) {
            data = data.get(1..).unwrap_or(&[]);
        }
        data
    }
}

#[cfg(target_os = "macos")]
pub use imp::{macos_proc_args, macos_proc_cwd, macos_proc_exe_path, macos_proc_start_time};
