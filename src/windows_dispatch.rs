//! Deferred execution on the UI thread, between gpui message dispatches.
//!
//! Some Win32 entry points (ShellExecuteW, DoDragDrop, …) pump window
//! messages synchronously while they run. Called from inside a gpui
//! callback, the pumped messages reenter gpui's window procedure while
//! the `AppCell` is still mutably borrowed; gpui's reentrant dispatch
//! then panics with "RefCell already borrowed" and the unwind hits the
//! `extern "C"` window-procedure boundary ("panic in a function that
//! cannot unwind"), aborting the process. Observed in the wild as the
//! crash after "edit: staging …" once the staged download completed and
//! the file's default app was opened via ShellExecuteW.
//!
//! [`run_between_dispatches`] postpones such a call to a `SetTimer`
//! TIMERPROC, which Windows invokes while gpui's thread sits in
//! `GetMessage` — after the current dispatch (and its borrow) has
//! completed. The nested pump's messages each take short sequential
//! borrows, so nothing conflicts. Same pattern as `windows_drag`'s drag
//! start.

use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer, WM_TIMER};

/// One-shot timer id for deferred work (distinct from the drag-out timer).
const DEFER_TIMER_ID: usize = 0xA380;

/// Thunks waiting for the next between-dispatches window. A queue (not a
/// single slot) because editor opens can be requested back to back; the
/// timer proc drains it completely.
static PENDING: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>> = Mutex::new(Vec::new());

/// Run `thunk` on the UI thread once gpui's current dispatch completes.
/// `app_hwnd` is gpui's window handle (see `RootView::app_hwnd`).
pub fn run_between_dispatches(app_hwnd: isize, thunk: Arc<dyn Fn() + Send + Sync>) {
    if app_hwnd == 0 {
        // No window handle yet (shouldn't happen: the UI rendered before
        // any staged open was requested). A fresh thread still avoids the
        // reentrancy, at the price of ShellExecute's rare DDE-based
        // associations, which want the UI thread's message queue.
        std::thread::spawn(move || thunk());
        return;
    }
    PENDING.lock().unwrap().push(thunk);
    unsafe {
        // 1 ms; the proc kills the timer regardless (uElapse=0's meaning
        // is ambiguous on some Windows versions).
        SetTimer(
            Some(HWND(app_hwnd as *mut _)),
            DEFER_TIMER_ID,
            1,
            Some(defer_timer_proc),
        );
    }
}

unsafe extern "system" fn defer_timer_proc(hwnd: HWND, msg: u32, id: usize, _time: u32) {
    if msg != WM_TIMER || id != DEFER_TIMER_ID {
        return;
    }
    unsafe {
        let _ = KillTimer(Some(hwnd), DEFER_TIMER_ID);
    }
    for thunk in std::mem::take(&mut *PENDING.lock().unwrap()) {
        thunk();
    }
}
