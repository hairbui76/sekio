//! Turning a path into the one canonical spelling the rest of the app uses.
//!
//! There is exactly one place a Win32 *verbatim* (extended-length) path can
//! enter this program: [`Path::canonicalize`], which on Windows always answers
//! with the `\\?\` form — `\\?\C:\Users\Admin\Downloads`. That prefix is a
//! kernel-level "skip Win32 path parsing" marker. It is correct to use and
//! wrong to show: the home screen was printing it under every recent file, and
//! a user who copies it out hands `\\?\C:\…` to programs that choke on it.
//!
//! Two ways to fix that were on the table:
//!
//! 1. Strip it at *display* time. But there is no one display site — the
//!    recent list, the browser header, the window title, the `recent` state
//!    file, tooltips — so every future one has to remember, and the state file
//!    on disk keeps the ugly form regardless.
//! 2. Strip it at *canonicalize* time, so no verbatim path ever exists past
//!    the four calls that create one. One rule, enforced where the problem is
//!    introduced rather than at each of the places it leaks out.
//!
//! This module is (2): [`canonical`] replaces `Path::canonicalize` everywhere
//! in the crate, and nothing downstream can reintroduce the prefix.
//!
//! Path *equality* is what makes (2) safe here rather than merely tidier. The
//! recent list dedupes by `PathBuf` comparison, the sibling walk compares the
//! current path against directory entries, and the daemon protocol compares a
//! path that arrived over a socket against one resolved locally. Normalising
//! at the single point where verbatim paths are minted means both sides of
//! every one of those comparisons went through the same function, so they
//! still agree. (The daemon is `#[cfg(unix)]`, where verbatim paths do not
//! exist at all, so today it is unaffected either way; the property is what
//! keeps it true if it is ever ported.)
//!
//! **Everything here works on the string form**, and none of it consults
//! `Path::is_absolute`, `Path::components` or `std::path::Prefix`, all of which
//! answer for the *host* OS. That is what lets the Windows shapes be tested
//! from Linux — see CLAUDE.md, where this exact trap is on record for having
//! broken CI twice.

use std::borrow::Cow;
use std::io;
use std::path::{Path, PathBuf};

/// The Win32 extended-length prefix.
const VERBATIM: &str = r"\\?\";

/// What follows [`VERBATIM`] when the path is a UNC share.
const VERBATIM_UNC: &str = r"UNC\";

/// The classic Win32 path length limit, including the terminating NUL. A path
/// at or past it is exactly what the verbatim prefix exists to express, so
/// dropping the prefix there could turn an openable path into an unopenable
/// one. See [`survives_without_the_prefix`].
const MAX_PATH: usize = 260;

/// Canonicalise `path` and drop the verbatim prefix, so the result is both
/// absolute and presentable.
///
/// This is [`Path::canonicalize`] for the whole crate; call it instead.
pub fn canonical(path: &Path) -> io::Result<PathBuf> {
    path.canonicalize().map(|resolved| plain(&resolved))
}

/// The presentable spelling of an already-resolved path.
///
/// Split out from [`canonical`] for the `unwrap_or` case: a path that could not
/// be canonicalised is used as-is, and must not be quietly left in a different
/// form from every other path in the app.
pub fn plain(path: &Path) -> PathBuf {
    // A path that is not UTF-8 keeps its prefix. Only Windows produces one of
    // those (an unpaired surrogate in a file name), the case is vanishingly
    // rare, and a slightly ugly label beats reaching for `unsafe` to slice
    // `OsStr`'s encoded bytes.
    match path.to_str() {
        Some(text) => PathBuf::from(strip_verbatim(text).into_owned()),
        None => path.to_path_buf(),
    }
}

/// Rewrite the verbatim spelling of a Windows path as the one a user reads.
///
/// * `\\?\C:\x` becomes `C:\x`.
/// * `\\?\UNC\srv\share` becomes `\\srv\share` — the same share, spelled the
///   way it is typed.
/// * Everything else is returned untouched, including a plain `C:\x`, a Unix
///   `/home/x`, and a genuine UNC path `\\srv\share` that merely starts with
///   two backslashes.
///
/// Untouched also covers the two cases where the classic spelling would name
/// something *different*: a device path with no classic form at all
/// (`\\?\Volume{…}`, `\\?\PhysicalDrive0`), and a path the Win32 layer would
/// mangle (see [`survives_without_the_prefix`]). Correct-and-ugly beats
/// pretty-and-broken.
pub fn strip_verbatim(text: &str) -> Cow<'_, str> {
    let Some(rest) = text.strip_prefix(VERBATIM) else {
        return Cow::Borrowed(text);
    };
    let plain: Cow<'_, str> = if let Some(share) = rest.strip_prefix(VERBATIM_UNC) {
        // `\\?\UNC\srv\share` -> `\\srv\share`: the leading pair of
        // backslashes has to be written, not sliced, so this is the one case
        // that allocates.
        Cow::Owned(format!(r"\\{share}"))
    } else if starts_with_drive(rest) {
        Cow::Borrowed(rest)
    } else {
        return Cow::Borrowed(text);
    };
    if survives_without_the_prefix(&plain) {
        plain
    } else {
        Cow::Borrowed(text)
    }
}

/// Does `text` begin `X:\`, for some ASCII letter `X`?
///
/// Deliberately hand-rolled rather than `std::path::Prefix`: that parses by the
/// rules of the host OS, so on Linux it sees no drive letter at all and this
/// function would answer differently depending on where it was compiled.
fn starts_with_drive(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(drive), Some(':'), Some('\\')) if drive.is_ascii_alphabetic()
    )
}

/// Would the classic spelling still name the same file?
///
/// The verbatim prefix does not only lift the length limit, it also turns off
/// Win32 path normalisation. Two things therefore only work *with* it:
///
/// * a path at or past [`MAX_PATH`], which without the prefix is refused on
///   any machine that has not opted into long path support;
/// * a component with a trailing dot or space, which Win32 silently trims —
///   `\\?\C:\a\report.` opens `report.`, but `C:\a\report.` opens `report`.
///
/// Neither is common, and both are why `dunce::simplified` has the same guard.
fn survives_without_the_prefix(plain: &str) -> bool {
    plain.len() < MAX_PATH
        && !plain
            .split('\\')
            .any(|part| part.ends_with('.') || part.ends_with(' '))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug from the screenshot: the home screen showed
    /// `\\?\C:\Users\Admin\Downloads` as a recent file's parent directory.
    #[test]
    fn a_verbatim_drive_path_loses_its_prefix() {
        assert_eq!(strip_verbatim(r"\\?\C:\x"), r"C:\x");
        assert_eq!(
            strip_verbatim(r"\\?\C:\Users\Admin\Downloads"),
            r"C:\Users\Admin\Downloads"
        );
        // Lower case and the root of a drive.
        assert_eq!(strip_verbatim(r"\\?\d:\"), r"d:\");
    }

    #[test]
    fn a_verbatim_unc_path_becomes_the_share_as_typed() {
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\share"), r"\\srv\share");
        assert_eq!(
            strip_verbatim(r"\\?\UNC\srv\share\dir\file.txt"),
            r"\\srv\share\dir\file.txt"
        );
    }

    #[test]
    fn ordinary_paths_are_left_exactly_as_they_are() {
        for path in [
            r"C:\x",
            r"C:\Users\Admin\Downloads",
            "/home/x",
            "/home/x/Tải xuống",
            "relative/path",
            "",
            r"\",
            // A legitimate UNC path already in the form a user types. The
            // leading `\\` must not be mistaken for half a verbatim prefix.
            r"\\server\share",
            r"\\server\share\dir\file.txt",
            // Nor must a near-miss of the prefix itself.
            r"\\?",
            r"\\?x\C:\y",
            r"\?\C:\y",
        ] {
            assert_eq!(strip_verbatim(path), path, "{path:?} must not change");
        }
    }

    #[test]
    fn verbatim_paths_with_no_classic_spelling_keep_the_prefix() {
        // Device paths: `\\?\` here is not an extended-length marker for a
        // file at all, and there is nothing to strip it down to.
        for path in [
            r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\",
            r"\\?\PhysicalDrive0",
            r"\\?\pipe\sekio",
            // Not a drive letter, and not `UNC\`.
            r"\\?\1:\x",
            r"\\?\C:x",
            r"\\?\C:",
            r"\\?\",
        ] {
            assert_eq!(strip_verbatim(path), path, "{path:?} must not change");
        }
    }

    #[test]
    fn a_path_that_only_works_with_the_prefix_keeps_it() {
        let long = format!(r"\\?\C:\{}", "a".repeat(MAX_PATH));
        assert_eq!(
            strip_verbatim(&long),
            long,
            "past MAX_PATH the prefix is load-bearing, not decoration"
        );

        // Win32 trims trailing dots and spaces from components, so these name
        // a different file once the prefix is gone.
        assert_eq!(strip_verbatim(r"\\?\C:\a\report."), r"\\?\C:\a\report.");
        assert_eq!(strip_verbatim(r"\\?\C:\a \b"), r"\\?\C:\a \b");
        assert_eq!(
            strip_verbatim(r"\\?\UNC\srv\share\odd."),
            r"\\?\UNC\srv\share\odd."
        );

        // Just under the limit is still fine.
        let short = format!(r"\\?\C:\{}", "a".repeat(MAX_PATH - 8));
        assert_eq!(
            strip_verbatim(&short),
            short.strip_prefix(VERBATIM).unwrap()
        );
    }

    #[test]
    fn stripping_is_idempotent_and_borrows_when_nothing_changes() {
        let once = strip_verbatim(r"\\?\C:\x").into_owned();
        assert_eq!(strip_verbatim(&once), once);
        assert!(
            matches!(strip_verbatim("/home/x"), Cow::Borrowed(_)),
            "the common case must not allocate"
        );
    }

    /// `plain` is the `Path` face of the same rule, and must agree with it on
    /// both platforms' shapes regardless of the host.
    #[test]
    fn plain_applies_the_same_rule_to_a_path() {
        assert_eq!(plain(Path::new(r"\\?\C:\x")), PathBuf::from(r"C:\x"));
        assert_eq!(
            plain(Path::new(r"\\?\UNC\srv\share")),
            PathBuf::from(r"\\srv\share")
        );
        assert_eq!(plain(Path::new("/home/x")), PathBuf::from("/home/x"));
        assert_eq!(
            plain(Path::new(r"\\server\share")),
            PathBuf::from(r"\\server\share")
        );
    }

    /// Two paths that named the same file before must still name the same
    /// `PathBuf` after — this is what the recent list, the sibling walk and
    /// the daemon's path comparison all rely on.
    #[test]
    fn normalising_keeps_equal_paths_equal() {
        let over_the_socket = plain(Path::new(r"\\?\C:\Users\Admin\a.txt"));
        let resolved_locally = plain(Path::new(r"\\?\C:\Users\Admin\a.txt"));
        assert_eq!(over_the_socket, resolved_locally);
        assert_eq!(over_the_socket, PathBuf::from(r"C:\Users\Admin\a.txt"));
    }

    /// The real thing, on whatever host is running the tests: a path that
    /// exists resolves, and never comes back wearing a prefix.
    #[test]
    fn canonical_resolves_and_never_returns_a_verbatim_path() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("sekio-gui-canonical-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").expect("write fixture");

        let resolved = canonical(&file).expect("the file exists");
        assert!(!resolved.to_string_lossy().starts_with(VERBATIM));
        assert!(resolved.ends_with(file.file_name().expect("a file name")));
        // Same file reached two ways still compares equal.
        assert_eq!(
            resolved,
            canonical(&dir.join(".").join(file.file_name().expect("a file name")))
                .expect("the same file")
        );

        assert!(canonical(&dir.join("sekio-gui-does-not-exist")).is_err());
        let _ = std::fs::remove_file(&file);
    }
}
