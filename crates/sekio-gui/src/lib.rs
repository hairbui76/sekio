//! The sekio GUI as a library.
//!
//! `src/main.rs` is a thin `fn main` over this: argument parsing, the daemon
//! and the window come from here. The split exists so the UI is *reachable*
//! from an integration test — a binary-only crate has nothing for `tests/` to
//! link against, which is why not one frame of this app had ever been rendered
//! before `tests/render.rs`. Nothing else about the layout changed: every
//! module is the same file it was, and `crate::…` paths inside them still
//! resolve the same way.

pub mod app;
pub mod browser;
pub mod console;
#[cfg(unix)]
pub mod daemon;
pub mod dialog;
pub mod fonts;
pub mod hotkey;
pub mod paths;
pub mod recent;
pub mod selection;
pub mod state;
pub mod style;
pub mod timing;
pub mod worker;
