//! Keeping the computer awake while there is work: a request that the system not go to sleep by
//! itself, never one to keep the screen on. Windows takes a power request (`powercfg /requests`
//! shows it with [`REASON`]), macOS runs its own `caffeinate`, tied to this process, and Linux
//! takes a logind sleep inhibitor through `systemd-inhibit` where there is one. Elsewhere, or when
//! the system refuses, nothing is held and downloads go on as before.

use std::io;

/// Why the computer is kept awake, as the system's lists of what keeps it awake show it.
pub const REASON: &str = "Endo's Unified Downloader is downloading files";

/// The system's promise not to sleep by itself, kept while this lives. The screen may still turn
/// off, and the user may still put the computer to sleep.
pub struct Awake(#[allow(dead_code)] imp::Hold);

/// Asks the system not to sleep by itself until the [`Awake`] returned is dropped.
pub fn stay_awake() -> io::Result<Awake> {
    imp::hold().map(Awake)
}

/// Takes a hold on the system (an [`Awake`] in the app, a stand-in in tests): released when dropped.
pub type Acquire = Box<dyn Fn() -> io::Result<Box<dyn Send>> + Send>;

/// Holds the computer awake while it is told there is work, and lets it go the moment there is
/// none. A hold the system refused is not asked for again until the work has stopped once, so a
/// computer without a way to keep it awake is not asked every frame.
pub struct KeepAwake {
    acquire: Acquire,
    held: Option<Box<dyn Send>>,
    refused: bool,
}

impl KeepAwake {
    pub fn new(acquire: Acquire) -> Self {
        Self { acquire, held: None, refused: false }
    }

    /// Through the system: see [`stay_awake`].
    pub fn system() -> Self {
        Self::new(Box::new(|| stay_awake().map(|awake| Box::new(awake) as Box<dyn Send>)))
    }

    /// Holds the computer awake while `work` is true, else lets it go.
    pub fn set(&mut self, work: bool) {
        if !work {
            self.held = None;
            self.refused = false;
        } else if self.held.is_none() && !self.refused {
            match (self.acquire)() {
                Ok(hold) => self.held = Some(hold),
                Err(e) => {
                    tracing::warn!("Could not keep the computer awake: {}", e);
                    self.refused = true;
                }
            }
        }
    }

    pub fn is_held(&self) -> bool {
        self.held.is_some()
    }
}

#[cfg(windows)]
mod imp {
    use std::io;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Power::{PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest};
    use windows_sys::Win32::System::Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0};

    /// A power request asking for the system, not the display.
    pub struct Hold(HANDLE);

    // SAFETY: a power request's handle may be used and closed from any thread.
    unsafe impl Send for Hold {}

    pub fn hold() -> io::Result<Hold> {
        let mut reason: Vec<u16> = super::REASON.encode_utf16().chain([0]).collect();
        // POWER_REQUEST_CONTEXT_VERSION is 0.
        let context = REASON_CONTEXT {
            Version: 0,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 { SimpleReasonString: reason.as_mut_ptr() },
        };
        // SAFETY: the context and its NUL-terminated string outlive the call, which copies them;
        // the handle is closed once, by the hold or here.
        unsafe {
            let request = PowerCreateRequest(&context);
            if request == INVALID_HANDLE_VALUE || request.is_null() {
                return Err(io::Error::last_os_error());
            }
            if PowerSetRequest(request, PowerRequestSystemRequired) == 0 {
                let e = io::Error::last_os_error();
                CloseHandle(request);
                return Err(e);
            }
            Ok(Hold(request))
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            // SAFETY: the request this hold set, and its handle, closed once.
            unsafe {
                PowerClearRequest(self.0, PowerRequestSystemRequired);
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod imp {
    use std::io;
    use std::process::{Child, ChildStdin, Command, Stdio};

    /// The program holding the computer awake for us, and its input: when this process ends,
    /// however it ends, `caffeinate` sees it gone and `cat` its input close, and the hold ends.
    pub struct Hold(Child, Option<ChildStdin>);

    pub fn hold() -> io::Result<Hold> {
        let mut command = if cfg!(target_os = "macos") {
            // -i: no idle sleep (the display may sleep); -w: until this process exits.
            let mut command = Command::new("/usr/bin/caffeinate");
            command.args(["-i", "-w", &std::process::id().to_string()]);
            command
        } else {
            let mut command = Command::new("systemd-inhibit");
            command.args(["--what=sleep", "--mode=block", "--who=Endo's Unified Downloader", &format!("--why={}", super::REASON), "cat"]);
            command
        };
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        let input = child.stdin.take();
        Ok(Hold(child, input))
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            drop(self.1.take());
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    pub struct Hold;

    pub fn hold() -> std::io::Result<Hold> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "this system has no way to be kept awake that the app knows"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    /// Counts its own release.
    struct Hold(Arc<AtomicUsize>);

    impl Drop for Hold {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A stand-in for the system: how many holds it gave and released, refusing while `refuse`.
    fn counted(refuse: Arc<AtomicUsize>) -> (KeepAwake, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (taken, released) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (t, r) = (Arc::clone(&taken), Arc::clone(&released));
        let keep = KeepAwake::new(Box::new(move || {
            t.fetch_add(1, Ordering::SeqCst);
            if refuse.load(Ordering::SeqCst) > 0 {
                return Err(io::Error::other("refused"));
            }
            Ok(Box::new(Hold(Arc::clone(&r))) as Box<dyn Send>)
        }));
        (keep, taken, released)
    }

    /// One hold for as long as there is work, however often that is said; released at once when
    /// there is none. A refusal is not asked again until the work stopped.
    #[test]
    fn the_computer_is_held_awake_only_while_there_is_work() {
        let refuse = Arc::new(AtomicUsize::new(0));
        let (mut keep, taken, released) = counted(Arc::clone(&refuse));
        let counts = || (taken.load(Ordering::SeqCst), released.load(Ordering::SeqCst));
        keep.set(false);
        assert_eq!(counts(), (0, 0));
        for _ in 0..3 {
            keep.set(true);
        }
        assert!(keep.is_held());
        assert_eq!(counts(), (1, 0));
        keep.set(false);
        assert!(!keep.is_held());
        assert_eq!(counts(), (1, 1));
        keep.set(true);
        drop(keep);
        assert_eq!(counts(), (2, 2), "dropped, it lets go");

        refuse.store(1, Ordering::SeqCst);
        let (mut keep, taken, _) = counted(Arc::clone(&refuse));
        keep.set(true);
        keep.set(true);
        assert_eq!(taken.load(Ordering::SeqCst), 1, "a refusal is not asked again");
        keep.set(false);
        keep.set(true);
        assert_eq!(taken.load(Ordering::SeqCst), 2, "asked again once the work stopped and came back");
    }
}
