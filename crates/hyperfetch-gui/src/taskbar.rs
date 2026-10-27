//! The app's button on the Windows taskbar: taken off while the window is in the tray, and
//! showing the overall progress of the running downloads (`ITaskbarList3`, called over COM by
//! hand). Other systems have none.

/// What the taskbar button shows: nothing, or the share done (in thousandths) of the running
/// downloads, as is, paused (yellow) or with an error (red).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    None,
    Normal(u16),
    Paused(u16),
    Error(u16),
}

#[cfg(windows)]
pub use imp::Taskbar;

/// No taskbar button to show anything on.
#[cfg(not(windows))]
pub struct Taskbar;

#[cfg(not(windows))]
impl Taskbar {
    pub fn new(_title: &str) -> Option<Self> {
        None
    }

    pub fn show_button(&self, _on: bool) {}

    pub fn set_progress(&self, _progress: Progress) -> bool {
        true
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumThreadWindows, GetWindowTextW};

    use super::Progress;

    const TASKBAR_LIST: GUID = GUID::from_u128(0x56fdf344_fd6d_11d0_958a_006097c9a090);
    const ITASKBAR_LIST3: GUID = GUID::from_u128(0xea1afb91_9e28_4b86_90e9_9e9f8a5eee6f);

    /// `ITaskbarList3`'s methods up to the ones used here, in its vtable's order.
    #[repr(C)]
    struct Methods {
        _query_interface: usize,
        _add_ref: usize,
        release: unsafe extern "system" fn(*mut List) -> u32,
        hr_init: unsafe extern "system" fn(*mut List) -> i32,
        add_tab: unsafe extern "system" fn(*mut List, HWND) -> i32,
        delete_tab: unsafe extern "system" fn(*mut List, HWND) -> i32,
        _activate_tab: usize,
        _set_active_alt: usize,
        _mark_fullscreen_window: usize,
        set_progress_value: unsafe extern "system" fn(*mut List, HWND, u64, u64) -> i32,
        set_progress_state: unsafe extern "system" fn(*mut List, HWND, i32) -> i32,
    }

    #[repr(C)]
    struct List {
        methods: *const Methods,
    }

    /// The taskbar button of the app's window.
    pub struct Taskbar {
        list: *mut List,
        window: HWND,
    }

    impl Taskbar {
        /// The taskbar of this thread's window titled `title` (the app's, on the UI thread).
        pub fn new(title: &str) -> Option<Self> {
            let window = thread_window(title)?;
            let mut list: *mut c_void = null_mut();
            // SAFETY: COM is set up for this thread (a second time is harmless), and
            // CoCreateInstance writes an ITaskbarList3 to `list` when it succeeds.
            let created = unsafe {
                CoInitializeEx(null(), COINIT_APARTMENTTHREADED as _);
                CoCreateInstance(&TASKBAR_LIST, null_mut(), CLSCTX_INPROC_SERVER, &ITASKBAR_LIST3, &mut list)
            };
            if created < 0 || list.is_null() {
                return None;
            }
            let taskbar = Self { list: list.cast(), window };
            // SAFETY: a live ITaskbarList3, which is released when `taskbar` drops.
            (unsafe { (taskbar.methods().hr_init)(taskbar.list) } >= 0).then_some(taskbar)
        }

        fn methods(&self) -> &Methods {
            // SAFETY: `list` is a live ITaskbarList3 for as long as `self` is.
            unsafe { &*(*self.list).methods }
        }

        /// Puts the window's button on the taskbar, or takes it off.
        pub fn show_button(&self, on: bool) {
            let methods = self.methods();
            let call = if on { methods.add_tab } else { methods.delete_tab };
            // SAFETY: a live ITaskbarList3 and the app's window.
            unsafe { call(self.list, self.window) };
        }

        /// Shows `progress` on the button; whether it took (it cannot before the button is up).
        pub fn set_progress(&self, progress: Progress) -> bool {
            // TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_ERROR, TBPF_PAUSED.
            let (state, done) = match progress {
                Progress::None => (0, 0),
                Progress::Normal(done) => (2, done),
                Progress::Error(done) => (4, done),
                Progress::Paused(done) => (8, done),
            };
            let methods = self.methods();
            // SAFETY: a live ITaskbarList3 and the app's window.
            unsafe {
                (methods.set_progress_state)(self.list, self.window, state) >= 0
                    && (state == 0 || (methods.set_progress_value)(self.list, self.window, done.into(), 1000) >= 0)
            }
        }
    }

    impl Drop for Taskbar {
        fn drop(&mut self) {
            // SAFETY: the one reference this holds.
            unsafe { (self.methods().release)(self.list) };
        }
    }

    /// This thread's top-level window titled `title`.
    fn thread_window(title: &str) -> Option<HWND> {
        /// Stops at the window titled as `found` says, keeping it there.
        unsafe extern "system" fn titled(window: HWND, found: LPARAM) -> BOOL {
            // SAFETY: `found` is the pair `thread_window` passes, alive through the enumeration.
            let found = unsafe { &mut *(found as *mut (Vec<u16>, HWND)) };
            let mut text = [0u16; 128];
            // SAFETY: writes at most the buffer's length.
            let len = unsafe { GetWindowTextW(window, text.as_mut_ptr(), text.len() as i32) };
            if text[..len.max(0) as usize] == found.0[..] {
                found.1 = window;
                return 0;
            }
            1
        }
        let mut found: (Vec<u16>, HWND) = (title.encode_utf16().collect(), null_mut());
        // SAFETY: `titled` gets a pointer to `found`, which outlives the call.
        unsafe { EnumThreadWindows(GetCurrentThreadId(), Some(titled), &mut found as *mut _ as LPARAM) };
        (!found.1.is_null()).then_some(found.1)
    }
}
