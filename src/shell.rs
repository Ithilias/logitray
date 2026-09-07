//! Opens files and links with their associated application.

use std::ffi::OsStr;

#[cfg(target_os = "windows")]
pub fn open(target: impl AsRef<OsStr>) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_cmd: i32,
        ) -> *mut c_void;
    }
    const SW_SHOWNORMAL: i32 = 1;

    let target = target.as_ref();
    let wide: Vec<u16> = target.encode_wide().chain(std::iter::once(0)).collect();
    // The same association lookup explorer.exe performs, without spawning a
    // process whose name could be shadowed by a file next to our exe.
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            ptr::null(),
            wide.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Legacy return convention: values up to 32 are error codes, not handles.
    if result as usize <= 32 {
        tracing::warn!(
            "failed opening {}: ShellExecuteW error {}",
            target.to_string_lossy(),
            result as usize
        );
    }
}

#[cfg(not(target_os = "windows"))]
pub fn open(target: impl AsRef<OsStr>) {
    tracing::warn!(
        "cannot open {} on this platform",
        target.as_ref().to_string_lossy()
    );
}
