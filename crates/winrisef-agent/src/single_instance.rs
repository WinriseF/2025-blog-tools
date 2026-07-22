#[cfg(target_os = "windows")]
pub struct LaunchInstanceGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(target_os = "windows")]
pub fn acquire() -> anyhow::Result<LaunchInstanceGuard> {
    use std::ptr;

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError},
        System::Threading::CreateMutexW,
    };

    let name = "Local\\WinriseF-Toolbox-Agent-Launch"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe { CreateMutexW(ptr::null(), 1, name.as_ptr()) };
    anyhow::ensure!(!handle.is_null(), "failed to create the Agent launch mutex");
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_running {
        unsafe {
            CloseHandle(handle);
        }
        anyhow::bail!("another WinriseF Agent launch is already in progress");
    }
    tracing::debug!("acquired the per-user Agent launch mutex");
    Ok(LaunchInstanceGuard { handle })
}

#[cfg(target_os = "windows")]
impl Drop for LaunchInstanceGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::ReleaseMutex};

        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub struct LaunchInstanceGuard;

#[cfg(not(target_os = "windows"))]
pub fn acquire() -> anyhow::Result<LaunchInstanceGuard> {
    Ok(LaunchInstanceGuard)
}
