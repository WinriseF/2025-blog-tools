#![deny(unsafe_op_in_unsafe_fn)]

use std::{io, path::Path};

#[cfg(windows)]
pub fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let mut source_wide = source.as_os_str().encode_wide().collect::<Vec<_>>();
    let mut destination_wide = destination.as_os_str().encode_wide().collect::<Vec<_>>();
    source_wide.push(0);
    destination_wide.push(0);

    // SAFETY: both pointers reference live, NUL-terminated UTF-16 buffers for the
    // duration of the call. Flags request same-volume replacement and durable metadata.
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}
