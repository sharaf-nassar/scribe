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
        let mut len: libc::size_t = 0;

        let size_result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                std::ptr::null_mut(),
                &mut len,
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
                mib.len() as libc::c_uint,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                &mut actual_len,
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
        if argc_size != 4 || buf.len() < argc_size {
            return None;
        }

        let argc = i32::from_ne_bytes(buf[..argc_size].try_into().ok()?);
        if argc < 0 {
            return None;
        }

        let mut data = &buf[argc_size..];
        let exe_end = data.iter().position(|byte| *byte == 0)?;
        data = &data[exe_end + 1..];
        while data.first() == Some(&0) {
            data = &data[1..];
        }

        let mut args = Vec::new();
        for _ in 0..argc {
            if data.is_empty() {
                break;
            }
            let arg_end = data.iter().position(|byte| *byte == 0).unwrap_or(data.len());
            let arg = &data[..arg_end];
            if !arg.is_empty() {
                args.push(arg.to_vec());
            }
            data = if arg_end < data.len() { &data[arg_end + 1..] } else { &[] };
            while data.first() == Some(&0) {
                data = &data[1..];
            }
        }

        Some(args)
    }
}

#[cfg(target_os = "macos")]
pub use imp::{macos_proc_args, macos_proc_cwd, macos_proc_exe_path};
