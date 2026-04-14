#[cfg(target_os = "macos")]
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

    #[allow(unsafe_code, reason = "proc_pidinfo FFI is required for macOS CWD detection")]
    {
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
}
