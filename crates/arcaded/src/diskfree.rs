//! Free bytes on the volume that holds a path. Callers treat 0 as "don't know" and
//! stop a copy rather than fill the disk.

pub fn free_bytes(p: &std::path::Path) -> u64 {
    #[cfg(unix)]
    {
        let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) else { return 0 };
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return 0;
        }
        return s.f_bavail as u64 * s.f_frsize as u64;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetDiskFreeSpaceExW(
                dir: *const u16,
                free_to_caller: *mut u64,
                total: *mut u64,
                total_free: *mut u64,
            ) -> i32;
        }
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut free = 0u64;
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        if ok == 0 { 0 } else { free }
    }
}

/// Total and free bytes on the volume that holds a path (None when they can't be read).
pub fn space(p: &std::path::Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        let total = s.f_blocks as u64 * s.f_frsize as u64;
        return (total > 0).then(|| (total, s.f_bavail as u64 * s.f_frsize as u64));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetDiskFreeSpaceExW(
                dir: *const u16,
                free_to_caller: *mut u64,
                total: *mut u64,
                total_free: *mut u64,
            ) -> i32;
        }
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let (mut free, mut total) = (0u64, 0u64);
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
        if ok == 0 || total == 0 { None } else { Some((total, free)) }
    }
}
