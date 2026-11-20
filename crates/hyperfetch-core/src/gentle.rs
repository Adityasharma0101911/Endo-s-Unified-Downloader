//! "Go easy while I use this PC": downloads step aside for what is done on the computer. On
//! Windows the `.part` file's reads and writes carry a low I/O priority hint, and the threads that
//! write and hash it give what they touch a low memory priority, so a large download neither
//! pushes the user's own files out of the cache nor holds up a game's reads; [`eco`] puts the whole
//! process in efficiency mode (EcoQoS). Elsewhere it does nothing.
//!
//! The network is not part of it. Low-priority sockets (`SIO_PRIORITY_HINT`) must be set before a
//! socket connects, and the HTTP client connects its own sockets, with no way in before that.

use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};

static ON: AtomicBool = AtomicBool::new(false);

/// Turns the mode on or off for what is written from then on.
pub fn set(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// While the mode is on, the current thread's memory priority is low until this is dropped.
pub struct Background(bool);

impl Background {
    pub fn here() -> Self {
        Self(on() && imp::memory_low(true))
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        if self.0 {
            imp::memory_low(false);
        }
    }
}

/// Gives `file` the low I/O priority hint while the mode is on, the normal one once it is off;
/// `hinted` is what it has now, so the system is asked only when that changes.
pub(crate) fn hint(file: &File, hinted: &AtomicBool) {
    let low = on();
    if hinted.swap(low, Ordering::Relaxed) != low {
        imp::io_low(file, low);
    }
}

/// Efficiency mode (EcoQoS) for this process when `on`, else the system's own choice again. False
/// when the system has none or refused.
pub fn eco(on: bool) -> bool {
    imp::eco(on)
}

#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{FileIoPriorityHintInfo, IoPriorityHintLow, IoPriorityHintNormal, SetFileInformationByHandle, FILE_IO_PRIORITY_HINT_INFO};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, ProcessPowerThrottling, SetProcessInformation, SetThreadInformation, ThreadMemoryPriority, MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_LOW,
        MEMORY_PRIORITY_NORMAL, PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    };

    pub fn io_low(file: &File, low: bool) -> bool {
        let info = FILE_IO_PRIORITY_HINT_INFO { PriorityHint: if low { IoPriorityHintLow } else { IoPriorityHintNormal } };
        // SAFETY: the handle is open while `file` lives, and the buffer is the struct the class names.
        unsafe { SetFileInformationByHandle(file.as_raw_handle(), FileIoPriorityHintInfo, (&raw const info).cast(), size_of::<FILE_IO_PRIORITY_HINT_INFO>() as u32) != 0 }
    }

    pub fn memory_low(low: bool) -> bool {
        let info = MEMORY_PRIORITY_INFORMATION { MemoryPriority: if low { MEMORY_PRIORITY_LOW } else { MEMORY_PRIORITY_NORMAL } };
        // SAFETY: the pseudo handle of this thread, and the struct the class names.
        unsafe { SetThreadInformation(GetCurrentThread(), ThreadMemoryPriority, (&raw const info).cast(), size_of::<MEMORY_PRIORITY_INFORMATION>() as u32) != 0 }
    }

    pub fn eco(on: bool) -> bool {
        let speed = if on { PROCESS_POWER_THROTTLING_EXECUTION_SPEED } else { 0 };
        let state = PROCESS_POWER_THROTTLING_STATE { Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION, ControlMask: speed, StateMask: speed };
        // SAFETY: the pseudo handle of this process, and the struct the class names.
        unsafe { SetProcessInformation(GetCurrentProcess(), ProcessPowerThrottling, (&raw const state).cast(), size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32) != 0 }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn io_low(_: &std::fs::File, _: bool) -> bool {
        false
    }

    pub fn memory_low(_: bool) -> bool {
        false
    }

    pub fn eco(_: bool) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn memory_priority() -> u32 {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadInformation, ThreadMemoryPriority, MEMORY_PRIORITY_INFORMATION};
        let mut info = MEMORY_PRIORITY_INFORMATION { MemoryPriority: 0 };
        // SAFETY: as in `imp::memory_low`, read instead of written.
        let ok = unsafe { GetThreadInformation(GetCurrentThread(), ThreadMemoryPriority, (&raw mut info).cast(), size_of::<MEMORY_PRIORITY_INFORMATION>() as u32) };
        assert_ne!(ok, 0);
        info.MemoryPriority
    }

    #[test]
    fn the_mode_steps_aside_only_while_it_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let file = File::create(dir.path().join("a.part")).unwrap();
        let hinted = AtomicBool::new(false);

        hint(&file, &hinted);
        assert!(!hinted.load(Ordering::Relaxed) && !Background::here().0, "off: nothing changes");

        set(true);
        hint(&file, &hinted);
        let easy = Background::here();
        set(false);
        assert!(hinted.load(Ordering::Relaxed));
        assert_eq!(easy.0, cfg!(windows));
        #[cfg(windows)]
        assert_eq!(memory_priority(), 2);
        drop(easy);
        #[cfg(windows)]
        assert_eq!(memory_priority(), 5, "the thread is given back");
        hint(&file, &hinted);
        assert!(!hinted.load(Ordering::Relaxed), "the file is given back too");

        assert_eq!(eco(true), cfg!(windows));
        assert_eq!(eco(false), cfg!(windows));
    }
}
