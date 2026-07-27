use std::path::PathBuf;

pub fn pick_files(dialog: rfd::FileDialog) -> Option<Vec<PathBuf>> {
    show(dialog, rfd::FileDialog::pick_files)
}

pub fn pick_folder(dialog: rfd::FileDialog) -> Option<PathBuf> {
    show(dialog, rfd::FileDialog::pick_folder)
}

pub fn save_file(dialog: rfd::FileDialog) -> Option<PathBuf> {
    show(dialog, rfd::FileDialog::save_file)
}

#[cfg(target_os = "windows")]
fn show<T>(dialog: rfd::FileDialog, open: impl FnOnce(rfd::FileDialog) -> T) -> T {
    let Some(owner) = windows::DialogOwner::new() else {
        tracing::warn!("failed to create the native dialog owner; using an unowned dialog");
        return open(dialog);
    };
    open(dialog.set_parent(&owner))
}

#[cfg(not(target_os = "windows"))]
fn show<T>(dialog: rfd::FileDialog, open: impl FnOnce(rfd::FileDialog) -> T) -> T {
    open(dialog)
}

#[cfg(target_os = "windows")]
mod windows {
    use std::num::NonZeroIsize;
    use std::ptr::null_mut;

    use raw_window_handle::{
        DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
        Win32WindowHandle, WindowHandle,
    };
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            BringWindowToTop, CreateWindowExW, DestroyWindow, SW_SHOW, SetForegroundWindow,
            ShowWindow, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        },
    };

    const STATIC_CLASS: &[u16] = &[
        b'S' as u16,
        b'T' as u16,
        b'A' as u16,
        b'T' as u16,
        b'I' as u16,
        b'C' as u16,
        0,
    ];
    const WINDOW_NAME: &[u16] = &[
        b'W' as u16,
        b'i' as u16,
        b'n' as u16,
        b'r' as u16,
        b'i' as u16,
        b's' as u16,
        b'e' as u16,
        b'F' as u16,
        0,
    ];

    pub(super) struct DialogOwner(HWND);

    impl DialogOwner {
        pub(super) fn new() -> Option<Self> {
            // SAFETY: STATIC is a built-in Win32 class. All pointers are valid for this call and
            // the returned HWND remains owned by this value until Drop destroys it on this thread.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                    STATIC_CLASS.as_ptr(),
                    WINDOW_NAME.as_ptr(),
                    WS_POPUP,
                    -32_000,
                    -32_000,
                    1,
                    1,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            };
            if hwnd.is_null() {
                return None;
            }
            // SAFETY: hwnd was created successfully above. The off-screen tool window never
            // appears in the taskbar; it exists only to give the modal dialog foreground ownership.
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
                BringWindowToTop(hwnd);
                SetForegroundWindow(hwnd);
            }
            Some(Self(hwnd))
        }
    }

    impl HasWindowHandle for DialogOwner {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let hwnd = NonZeroIsize::new(self.0 as isize).ok_or(HandleError::Unavailable)?;
            let raw = RawWindowHandle::Win32(Win32WindowHandle::new(hwnd));
            // SAFETY: the HWND belongs to this thread and remains valid for the returned handle's
            // lifetime because DialogOwner cannot be dropped while it is borrowed.
            Ok(unsafe { WindowHandle::borrow_raw(raw) })
        }
    }

    impl HasDisplayHandle for DialogOwner {
        fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
            Ok(DisplayHandle::windows())
        }
    }

    impl Drop for DialogOwner {
        fn drop(&mut self) {
            // SAFETY: this value exclusively owns the still-valid HWND created in new(). rfd has
            // already returned, so no dialog retains the parent handle when it is destroyed.
            unsafe {
                DestroyWindow(self.0);
            }
        }
    }
}
