//! One rule about child processes, shared by everything that starts one.
//!
//! sekio shells out in several places — ffmpeg for a video frame, LibreOffice
//! for a legacy document or a deck, curl for an update check. On Windows every
//! one of those flashes a console window on screen unless it is told not to:
//! the GUI is linked `windows_subsystem = "windows"` and has no console of its
//! own, so the child allocates one, paints it, and takes it away again. It
//! looks like a glitch, and on a slow spawn it looks like a glitch that lasts.
//!
//! `CREATE_NO_WINDOW` is the documented way to say "run this without a console
//! of your own", and it is what every helper sekio starts should be given.
//!
//! Two exceptions, both deliberate: a program whose *job* is to show a window
//! (`msiexec`, a desktop's file handler) must not be silenced, because the
//! window is the point.

use std::process::Command;

/// Start this child without giving it a console window.
///
/// A no-op everywhere but Windows, which is the only platform that would open
/// one.
#[cfg(windows)]
pub fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    /// `CREATE_NO_WINDOW`, from `processthreadsapi.h`. Spelled out rather than
    /// pulled from a binding crate: it is one documented constant, and the
    /// alternative is a dependency for a `u32`.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn hide_console(_command: &mut Command) {}
