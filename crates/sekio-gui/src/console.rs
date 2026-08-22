//! Where a windowed process's output goes on Windows.
//!
//! `sekio-gui` is linked with `#![windows_subsystem = "windows"]` (see the top
//! of `main.rs`). That is the only way to stop Windows opening a console
//! window next to the app every time it is launched from Explorer, the Start
//! Menu or a shortcut — a console-subsystem binary *always* gets one, and it
//! is what made the app look broken.
//!
//! The price is that a windows-subsystem process starts with **no standard
//! handles at all**: `GetStdHandle` returns NULL, so Rust's `println!` gets an
//! `Err` back and — because `print_to` panics on `Err` — takes the process
//! with it. `sekio-gui --doctor` would print nothing, and so would `--probe`,
//! `--timing`, `--help`, `--version` and every startup error. That is not a
//! trade worth making: `--doctor` is the documented answer to "the hotkey does
//! nothing".
//!
//! So the subsystem is declared *and* the output is put back:
//!
//! 1. `AttachConsole(ATTACH_PARENT_PROCESS)` — if the process was started from
//!    PowerShell, cmd or Windows Terminal, it joins that console. It creates
//!    nothing, which is the point: `AllocConsole` would hand back exactly the
//!    window this change exists to remove, so it is never called.
//! 2. The standard handles are then pointed at that console. Attaching alone
//!    is not enough to rely on: `AttachConsole` is documented to attach the
//!    process, not to fix up handles that were never initialised, so this
//!    opens `CONOUT$` — the console's active screen buffer, reachable by name
//!    like a file — and installs it with `SetStdHandle`. `println!` picks that
//!    up because std deliberately does not cache the value: "Don't cache
//!    handles but get them fresh for every read/write. This allows us to track
//!    changes to the value over time (such as if a process calls
//!    `SetStdHandle` while it's running)" — `std/src/sys/stdio/windows.rs`.
//!    This is the Rust-level equivalent of the C recipe
//!    `freopen("CONOUT$", "w", stdout)`.
//! 3. With no parent console the handles are pointed at `NUL` instead. There
//!    is nowhere for output to go — that is the normal GUI launch and must
//!    stay that way — but "nowhere" has to mean *discarded*, not *fails*,
//!    or the first `println!` anywhere in the program becomes a crash.
//!
//! A handle that is already set is never touched, and that is what keeps
//! `sekio-gui --doctor > report.txt` honest: a shell redirect hands the
//! process a real file handle before `main` runs, and clobbering it with
//! `CONOUT$` would move the report from the user's file onto their screen.
//! The same check makes the "we already had a console" case (a debugger, a
//! console-subsystem parent that passed its handles down) a no-op.
//!
//! Everything here is `#[cfg(windows)]`. On Linux `attach()` is a function
//! that returns `Attached::NotWindows` and does nothing at all: a process
//! started from a terminal inherits fds 0/1/2 and always has, and there is no
//! subsystem to declare.

/// What `attach()` found — a report, never an error. Every variant is a normal
/// way to start this program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attached {
    /// Not Windows. Nothing was done and nothing needed doing.
    NotWindows,
    /// A parent console was found: this was launched from a terminal, and
    /// `println!` reaches it.
    Parent,
    /// There is no console, because the app was launched from Explorer, the
    /// Start Menu, a shortcut or a shell hook — the ordinary GUI case, and the
    /// one the whole change is for. Output is discarded.
    NoConsole,
}

/// The facts one standard handle is judged on, separated from the rule so the
/// rule is testable without a Windows process to fake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StdHandle {
    /// `GetStdHandle` returned something other than NULL or
    /// `INVALID_HANDLE_VALUE` — the process was given this stream.
    pub set: bool,
    /// The kernel still recognises it. A value inherited from a parent whose
    /// handle was never duplicated into this process is `set` but not `live`,
    /// and writing to it would fail.
    pub live: bool,
}

/// Pure: should this standard handle be repointed at the device we opened?
///
/// Only when it would not work as it stands. A handle the shell set up — a
/// pipe, a `> file` redirect — is left exactly where it is.
pub fn needs_rebind(handle: StdHandle) -> bool {
    !(handle.set && handle.live)
}

/// One line for `--doctor`.
pub fn describe(attached: Attached) -> &'static str {
    match attached {
        Attached::NotWindows => "the terminal that started this process (nothing to attach)",
        Attached::Parent => "attached to the console of the terminal that started this process",
        Attached::NoConsole => {
            "no console — launched from Explorer, the Start Menu or a shortcut, \
             so output goes nowhere"
        }
    }
}

/// Give this process somewhere to print, without ever creating a window.
///
/// Call it as the first thing in `main`, before argument parsing: `--help`,
/// `--version` and clap's usage errors are all printed from inside `parse()`.
#[cfg(windows)]
pub fn attach() -> Attached {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};

    // SAFETY: `AttachConsole` takes a process id by value and writes through
    // no pointer of ours, so there is no memory to keep valid.
    // `ATTACH_PARENT_PROCESS` is the documented sentinel for "the console of
    // the process that launched this one". Calling it with no such console —
    // or when this process already has one — is defined behaviour: it returns
    // false and sets a last-error, which is why the result is inspected and
    // not unwrapped. A failure here is an ordinary outcome, not an error.
    let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok();

    // The same repair either way, on a different device: a real screen buffer
    // when there is a console, the bit bucket when there is not. What must not
    // happen is leaving a NULL standard handle behind, because `println!` on
    // one is an `Err`, and `println!` panics on `Err`.
    rebind(if attached { "CONOUT$" } else { "NUL" });

    if attached {
        Attached::Parent
    } else {
        Attached::NoConsole
    }
}

/// Nothing to do off Windows: a process started from a terminal inherits its
/// standard streams, and one started from a launcher inherits whatever the
/// launcher had. Neither case has ever needed help, and this must not change
/// either of them.
#[cfg(not(windows))]
pub fn attach() -> Attached {
    Attached::NotWindows
}

/// Point stdout and stderr at `device`, but only where they point nowhere.
///
/// stdin is deliberately left alone: nothing in this program reads it, and a
/// failed read is an `Err` a caller can see rather than a panic, so there is
/// nothing to repair.
#[cfg(windows)]
fn rebind(device: &str) {
    use windows::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

    let streams = [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    let wanted = streams.map(|stream| needs_rebind(inspect(stream)));
    if !wanted.iter().any(|needed| *needed) {
        // Both already go somewhere real — a redirect, a pipe, an inherited
        // console. Opening the device would be pointless and installing it
        // would be wrong.
        return;
    }
    let Some(handle) = open(device) else {
        // Nothing to install. Output stays as unreachable as it was, which is
        // the state this function found the process in; it is not a reason to
        // fail, and there is nowhere to report it to anyway.
        return;
    };
    for (stream, needed) in streams.into_iter().zip(wanted) {
        if needed {
            // SAFETY: `handle` is an open handle to `device`, deliberately
            // leaked by `open` so it outlives every write made through it —
            // the standard handles are read again on every `println!`, for the
            // rest of the process's life. `SetStdHandle` only stores the value
            // in this process's parameter block; it takes no ownership and
            // closes nothing, so installing the same handle for both streams
            // is fine. A failure is ignored for the same reason as above.
            let _ = unsafe { SetStdHandle(stream, handle) };
        }
    }
}

/// What this process currently holds for one standard stream.
#[cfg(windows)]
fn inspect(stream: windows::Win32::System::Console::STD_HANDLE) -> StdHandle {
    use windows::Win32::Foundation::GetHandleInformation;
    use windows::Win32::System::Console::GetStdHandle;

    // SAFETY: `GetStdHandle` reads one field out of this process's parameter
    // block and returns it by value — no pointers, no ownership, and nothing
    // to close afterwards. The `windows` wrapper already maps NULL and
    // `INVALID_HANDLE_VALUE` to `Err`, which is precisely "this stream was
    // never set up".
    let Ok(handle) = (unsafe { GetStdHandle(stream) }) else {
        return StdHandle::default();
    };
    let mut flags = 0u32;
    // SAFETY: `handle` is whatever the process parameters happen to hold, and
    // a stale value is exactly the case this call exists to detect — passing
    // one is defined: the kernel validates the handle and returns false for a
    // bad one rather than misbehaving. The out-parameter is a pointer to a
    // live local `u32`, which is the size and alignment the API writes.
    let live = unsafe { GetHandleInformation(handle, &mut flags) }.is_ok();
    StdHandle { set: true, live }
}

/// Open a console device by name, and leak the handle on purpose.
///
/// `CONOUT$` names the console's *active screen buffer* and is opened
/// read/write because that is what the CreateFile documentation requires of
/// it; `NUL` is the null device, which accepts and discards everything.
///
/// Both are reserved DOS device names that `CreateFile` resolves itself, and
/// nothing can shadow them — Windows will not let a file be called either. So
/// `std::fs` reaches them exactly as `CreateFileW` would, which is why this
/// needs no `Win32_Storage_FileSystem` feature and no third `unsafe` block.
///
/// The handle is leaked because it becomes this process's standard output for
/// the rest of its life: dropping the `File` would close it and leave the
/// standard handles pointing at a destroyed object. One handle, never closed,
/// is what `freopen("CONOUT$", "w", stdout)` amounts to in C too. `None` means
/// the device would not open, which changes nothing — the caller gives up.
#[cfg(windows)]
fn open(device: &str) -> Option<windows::Win32::Foundation::HANDLE> {
    use std::os::windows::io::IntoRawHandle as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .ok()?;
    Some(windows::Win32::Foundation::HANDLE(file.into_raw_handle()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_that_points_nowhere_is_rebound() {
        // The windows-subsystem launch this whole module is about: the process
        // was handed no standard streams at all.
        assert!(needs_rebind(StdHandle::default()));
    }

    #[test]
    fn a_redirected_handle_is_left_alone() {
        // `sekio-gui --doctor > report.txt`: the shell set this up before
        // `main` ran, and moving it to CONOUT$ would empty the user's file.
        assert!(!needs_rebind(StdHandle {
            set: true,
            live: true,
        }));
    }

    #[test]
    fn a_stale_inherited_handle_is_rebound() {
        // Set, but the kernel does not know it: writing to it would fail, so
        // it is no better than an unset one.
        assert!(needs_rebind(StdHandle {
            set: true,
            live: false,
        }));
    }

    /// Calling it on Windows is left out on purpose rather than forgotten: a
    /// test binary is a *console*-subsystem process that already owns a
    /// console, so `AttachConsole` there answers a different question than the
    /// one this code was written for, and the answer would prove nothing. The
    /// Windows behaviour cannot be faked from a test; the rule it turns on is
    /// the pure function tested above.
    #[test]
    #[cfg(not(windows))]
    fn attaching_off_windows_does_nothing_and_says_so() {
        assert_eq!(attach(), Attached::NotWindows);
        // Twice: nothing is initialised, so nothing can be initialised twice.
        assert_eq!(attach(), Attached::NotWindows);
    }

    #[test]
    fn every_outcome_describes_itself() {
        for attached in [Attached::NotWindows, Attached::Parent, Attached::NoConsole] {
            assert!(!describe(attached).is_empty());
        }
        // The one a Windows user is meant to act on has to say where output
        // went, not just that something failed.
        assert!(describe(Attached::NoConsole).contains("Explorer"));
    }
}
