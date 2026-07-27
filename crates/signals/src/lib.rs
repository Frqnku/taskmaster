use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Install handlers for shutdown signals and, on Unix, SIGHUP reloads.
pub fn install() -> io::Result<()> {
    platform::install()
}

/// Return and clear the pending shutdown request.
pub fn take_shutdown() -> bool {
    SHUTDOWN_REQUESTED.swap(false, Ordering::SeqCst)
}

/// Return and clear the pending configuration reload request.
pub fn take_reload() -> bool {
    RELOAD_REQUESTED.swap(false, Ordering::SeqCst)
}

#[cfg(unix)]
mod platform {
    use super::{RELOAD_REQUESTED, SHUTDOWN_REQUESTED};
    use std::io;
    use std::sync::atomic::Ordering;

    const SIGHUP: i32 = 1;
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    unsafe extern "C" {
        fn signal(signal: i32, handler: *const ()) -> *const ();
    }

    extern "C" fn handle_signal(signal: i32) {
        if signal == SIGHUP {
            RELOAD_REQUESTED.store(true, Ordering::SeqCst);
        } else if signal == SIGINT || signal == SIGTERM {
            SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
        }
    }

    pub(super) fn install() -> io::Result<()> {
        for signal_number in [SIGHUP, SIGINT, SIGTERM] {
            // `signal` is part of the C runtime exposed by every supported Unix target.
            let previous = unsafe { signal(signal_number, handle_signal as *const ()) };
            if previous as isize == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::SHUTDOWN_REQUESTED;
    use std::io;
    use std::sync::atomic::Ordering;

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }

    extern "system" fn handle_console_event(event: u32) -> i32 {
        match event {
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT => {
                SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
                1
            }
            _ => 0,
        }
    }

    pub(super) fn install() -> io::Result<()> {
        let installed = unsafe { SetConsoleCtrlHandler(Some(handle_console_event), 1) };
        if installed == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::io;

    pub(super) fn install() -> io::Result<()> {
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    unsafe extern "C" {
        fn raise(signal: i32) -> i32;
    }

    #[test]
    fn sighup_requests_reload() {
        install().expect("install signal handlers");
        assert_eq!(unsafe { raise(1) }, 0);
        assert!(take_reload());
        assert!(!take_reload());
    }
}
