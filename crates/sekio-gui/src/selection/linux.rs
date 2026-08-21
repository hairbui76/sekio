//! Linux: there is no way to ask a file manager what is selected.
//!
//! This is not an oversight in sekio — the API genuinely does not exist. What
//! was checked, and what each check turned up:
//!
//! - `org.freedesktop.FileManager1` (implemented by Nautilus, Dolphin, Nemo,
//!   Caja, Thunar) is a *show* interface: `ShowFolders`, `ShowItems`,
//!   `ShowItemProperties`. It can point a file manager at a file; it cannot
//!   ask one anything. Its two properties, `OpenLocations` and
//!   `OpenWindowsWithLocations`, report which *folders* are open — never a
//!   selection. That is the only thing this module uses it for, and only as
//!   context for a bare filename (see `resolve_bare_name`).
//! - Dolphin's own `org.kde.dolphin.MainWindow` exports `openDirectories`,
//!   `openFiles`, `activateWindow`, `isActiveWindow`, `isUrlOpen`,
//!   `isItemVisibleInAnyView`, `pasteIntoFolder` and `changeUrl`. Selection
//!   exists internally as a `selectionChanged` signal with no getter, so
//!   there is nothing to call. `isItemVisibleInAnyView` answers "is this in a
//!   view", which is the wrong direction — it needs the path we are looking
//!   for as input.
//! - Nautilus exports `org.gtk.Actions`, `org.gtk.Application`,
//!   `org.gnome.Nautilus.FileOperations{,2}` and a Shell search provider.
//!   All of them act; none of them report state. Thunar's
//!   `org.xfce.FileManager` is the same shape.
//!
//! So the strategies here are, in order: the X11/Wayland PRIMARY selection
//! (which several managers fill with the selected file's name or URI), then
//! the CLIPBOARD (which holds `file://` URIs after a copy in most of them),
//! then — only for a bare name with no directory — the folders the file
//! manager admits to having open. Every one of them is a guess about text
//! someone else put in a shared buffer, so every candidate is filtered
//! through [`super::usable`] before it is returned.
//!
//! Origins are reported strictly by where the *path* came from. A path read
//! out of PRIMARY or CLIPBOARD is [`Origin::Clipboard`], however
//! file-manager-shaped it looks — there is no way to tell a manager's URI
//! from one a browser or a terminal put there. Only `resolve_bare_name` says
//! [`Origin::FileManager`], because the directory half of that answer came
//! from the file manager's own report of what it has open, and the answer
//! cannot exist without it. Nothing here reads a real selection, because
//! nothing on Linux exposes one.
//!
//! The hard rule, borrowed from `sekio-core`'s video renderer: this runs on a
//! hotkey press, so no child process may ever stall it. `xclip -o` blocks
//! until the selection *owner* answers, and a wedged owner would otherwise
//! hang the daemon forever. Every child is spawned with a deadline, polled
//! with `try_wait`, and killed *and reaped* the moment it passes. Nothing
//! calls `Command::output`, which waits unboundedly.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{usable, Origin, Selection, Source};

/// Everything this module is allowed to spend, across every strategy. A
/// hotkey press that takes longer than this feels broken.
const TOTAL_BUDGET: Duration = Duration::from_millis(200);
/// Leash on a single child. Clipboard tools answer in single-digit
/// milliseconds; anything slower is a wedged selection owner.
const CHILD_TIMEOUT: Duration = Duration::from_millis(100);
/// Poll interval while waiting. Short, because the whole budget is short.
const POLL_INTERVAL: Duration = Duration::from_millis(2);
/// Cap on what we read back from a child. A clipboard can hold a whole
/// document; a path is never more than a few hundred bytes.
const MAX_READ: u64 = 64 * 1024;
/// How many lines of a multi-line clipboard to inspect before giving up.
const MAX_LINES: usize = 32;
/// Longest bare filename we will try to resolve — `NAME_MAX` on Linux.
const MAX_NAME_LEN: usize = 255;
/// Most open folders to try a bare name against.
const MAX_LOCATIONS: usize = 16;

const FILE_MANAGER1_NAME: &str = "org.freedesktop.FileManager1";
const FILE_MANAGER1_PATH: &str = "/org/freedesktop/FileManager1";

// ----------------------------------------------------------------- Desktop

/// The Linux selection source. Both halves are probed once at construction:
/// a hotkey press should not be re-scanning PATH.
pub struct Desktop {
    clipboard: Option<Clipboard>,
    bus: Option<Bus>,
}

impl Desktop {
    pub fn new() -> Self {
        Self {
            clipboard: Clipboard::detect(),
            bus: Bus::detect(),
        }
    }

    /// Last resort: PRIMARY held a bare name (`report.pdf`) rather than a
    /// path — what an inline rename or a name-cell selection leaves behind.
    /// A name alone is meaningless, so it is joined against the folders the
    /// file manager reports having open and kept only if that names a real
    /// file. Deliberately last: a single word is weak evidence, and this
    /// costs two D-Bus round trips.
    ///
    /// This is the one path that reports [`Origin::FileManager`]: the folder
    /// came from the file manager itself, so the result is not "a path found
    /// in the clipboard" — there was no path in the clipboard. It is still a
    /// join of two weak facts, which is why it runs only after every direct
    /// reading has failed.
    fn resolve_bare_name(&self, text: &str, deadline: Instant) -> Option<Selection> {
        let bus = self.bus.as_ref()?;
        let name = bare_name(text)?;
        // Never let a hotkey *launch* a file manager: `FileManager1` is a
        // D-Bus-activatable name, so calling it cold would start Nautilus.
        // Ask the bus daemon (which is never activatable) first.
        if !bus.name_has_owner(FILE_MANAGER1_NAME, deadline) {
            return None;
        }
        for dir in bus.open_locations(deadline) {
            let candidate = dir.join(&name);
            if usable(&candidate) {
                return Some(Selection::new(candidate, Origin::FileManager));
            }
        }
        None
    }
}

impl Default for Desktop {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for Desktop {
    fn current(&self) -> Option<Selection> {
        let deadline = Instant::now() + TOTAL_BUDGET;
        // No display server, or no tool to talk to it: nothing to try. This
        // is the headless path, and it does no work at all.
        let clipboard = self.clipboard.as_ref()?;

        let mut primary_text: Option<String> = None;

        for selection in [Buffer::Primary, Buffer::Clipboard] {
            for &target in clipboard.targets() {
                if Instant::now() >= deadline {
                    return None;
                }
                let Some(text) = clipboard.read(selection, target, deadline) else {
                    // The tool failed: no display, no owner, or no such
                    // target. Asking the same buffer again in a different
                    // format will not help.
                    break;
                };
                if let Some(path) = first_usable(&text) {
                    return Some(Selection::new(path, Origin::Clipboard));
                }
                if selection == Buffer::Primary && target == Target::Text {
                    primary_text = Some(text);
                }
            }
        }

        self.resolve_bare_name(primary_text.as_deref()?, deadline)
    }

    fn describe(&self) -> &'static str {
        match (self.clipboard.as_ref().map(|c| c.tool), self.bus.is_some()) {
            (Some(Tool::WlPaste), true) => {
                "Linux desktop (PRIMARY selection via wl-paste, open folders via D-Bus)"
            }
            (Some(Tool::WlPaste), false) => "Linux desktop (PRIMARY selection via wl-paste)",
            (Some(Tool::Xclip), true) => {
                "Linux desktop (PRIMARY selection via xclip, open folders via D-Bus)"
            }
            (Some(Tool::Xclip), false) => "Linux desktop (PRIMARY selection via xclip)",
            (Some(Tool::Xsel), true) => {
                "Linux desktop (PRIMARY selection via xsel, open folders via D-Bus)"
            }
            (Some(Tool::Xsel), false) => "Linux desktop (PRIMARY selection via xsel)",
            (None, _) => {
                "Linux desktop (unavailable: no display server, or no wl-paste/xclip/xsel on PATH)"
            }
        }
    }
}

// --------------------------------------------------------------- clipboard

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Buffer {
    /// The current selection — what middle-click pastes. Several file
    /// managers put the highlighted file's name or URI here.
    Primary,
    /// The explicit Ctrl-C buffer, which holds `file://` URIs after a copy.
    Clipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// Whatever text the owner offers.
    Text,
    /// `text/uri-list`, what a file manager offers after copying files.
    UriList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    WlPaste,
    Xclip,
    Xsel,
}

struct Clipboard {
    tool: Tool,
    bin: PathBuf,
}

impl Clipboard {
    /// Pick a tool from the session's own environment: Wayland first when
    /// `WAYLAND_DISPLAY` is set, then the X11 tools, which also work under
    /// XWayland (where `DISPLAY` is set too). No display variable at all
    /// means a headless or tty session and there is nothing to read.
    fn detect() -> Option<Self> {
        if env_set("WAYLAND_DISPLAY") {
            if let Some(bin) = find_on_path("wl-paste") {
                return Some(Self {
                    tool: Tool::WlPaste,
                    bin,
                });
            }
        }
        if env_set("DISPLAY") {
            if let Some(bin) = find_on_path("xclip") {
                return Some(Self {
                    tool: Tool::Xclip,
                    bin,
                });
            }
            if let Some(bin) = find_on_path("xsel") {
                return Some(Self {
                    tool: Tool::Xsel,
                    bin,
                });
            }
        }
        None
    }

    /// Targets worth asking for, in order. xsel cannot request a target at
    /// all, so it gets one attempt.
    fn targets(&self) -> &'static [Target] {
        match self.tool {
            Tool::Xsel => &[Target::Text],
            _ => &[Target::Text, Target::UriList],
        }
    }

    /// `None` means the tool failed or was killed: no display, no selection
    /// owner, or the owner does not offer that target. All normal outcomes.
    fn read(&self, buffer: Buffer, target: Target, deadline: Instant) -> Option<String> {
        let out = TempFile::reserve("clip");
        let sink = std::fs::File::create(out.path()).ok()?;

        let mut cmd = Command::new(&self.bin);
        // Collect stdout in a file rather than a pipe: a pipe nobody drains
        // while we poll `try_wait` can fill and deadlock the child against
        // our own timeout loop.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(sink))
            .stderr(Stdio::null());

        match self.tool {
            Tool::WlPaste => {
                cmd.arg("--no-newline");
                if buffer == Buffer::Primary {
                    cmd.arg("--primary");
                }
                if target == Target::UriList {
                    cmd.arg("--type").arg("text/uri-list");
                }
            }
            Tool::Xclip => {
                cmd.arg("-o").arg("-selection");
                cmd.arg(match buffer {
                    Buffer::Primary => "primary",
                    Buffer::Clipboard => "clipboard",
                });
                // Ask for UTF8_STRING rather than xclip's XA_STRING default,
                // which is Latin-1 and mangles non-ASCII filenames.
                cmd.arg("-t").arg(match target {
                    Target::Text => "UTF8_STRING",
                    Target::UriList => "text/uri-list",
                });
            }
            Tool::Xsel => {
                cmd.arg("--output");
                cmd.arg(match buffer {
                    Buffer::Primary => "--primary",
                    Buffer::Clipboard => "--clipboard",
                });
            }
        }

        if !run_bounded(cmd, deadline) {
            return None;
        }
        read_capped(out.path())
    }
}

// ------------------------------------------------------------------- D-Bus

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusTool {
    /// glib's, ships with anything GTK-adjacent.
    Gdbus,
    /// dbus's own, ships with the bus daemon itself.
    DbusSend,
}

struct Bus {
    tool: BusTool,
    bin: PathBuf,
}

impl Bus {
    fn detect() -> Option<Self> {
        // Without a session bus there is nothing to talk to. `gdbus` would
        // also find one via `$XDG_RUNTIME_DIR/bus`, so accept either.
        let has_bus = env_set("DBUS_SESSION_BUS_ADDRESS")
            || std::env::var_os("XDG_RUNTIME_DIR")
                .map(|dir| Path::new(&dir).join("bus").exists())
                .unwrap_or(false);
        if !has_bus {
            return None;
        }
        if let Some(bin) = find_on_path("gdbus") {
            return Some(Self {
                tool: BusTool::Gdbus,
                bin,
            });
        }
        find_on_path("dbus-send").map(|bin| Self {
            tool: BusTool::DbusSend,
            bin,
        })
    }

    /// Is anyone actually holding this name? Asked of the bus daemon, which
    /// answers without starting anything — the point being to never activate
    /// a file manager that was not already running.
    fn name_has_owner(&self, name: &str, deadline: Instant) -> bool {
        let mut cmd = match self.tool {
            BusTool::Gdbus => {
                let mut cmd = self.base();
                cmd.arg("call")
                    .arg("--session")
                    .arg("--dest")
                    .arg("org.freedesktop.DBus")
                    .arg("--object-path")
                    .arg("/org/freedesktop/DBus")
                    .arg("--method")
                    .arg("org.freedesktop.DBus.NameHasOwner")
                    .arg(name);
                cmd
            }
            BusTool::DbusSend => {
                let mut cmd = self.base();
                cmd.arg("--session")
                    .arg("--print-reply")
                    .arg("--dest=org.freedesktop.DBus")
                    .arg("/org/freedesktop/DBus")
                    .arg("org.freedesktop.DBus.NameHasOwner")
                    .arg(format!("string:{name}"));
                cmd
            }
        };
        let out = TempFile::reserve("dbus");
        let Ok(sink) = std::fs::File::create(out.path()) else {
            return false;
        };
        cmd.stdout(Stdio::from(sink));
        if !run_bounded(cmd, deadline) {
            return false;
        }
        // `(true,)` from gdbus, `boolean true` from dbus-send.
        read_capped(out.path()).is_some_and(|text| text.contains("true"))
    }

    /// The folders the file manager currently has open, from
    /// `FileManager1.OpenLocations`. Not a selection and never used as one —
    /// only as the directory a bare filename might belong to. Managers that
    /// implement the interface without the property simply return nothing.
    fn open_locations(&self, deadline: Instant) -> Vec<PathBuf> {
        let mut cmd = match self.tool {
            BusTool::Gdbus => {
                let mut cmd = self.base();
                cmd.arg("call")
                    .arg("--session")
                    .arg("--dest")
                    .arg(FILE_MANAGER1_NAME)
                    .arg("--object-path")
                    .arg(FILE_MANAGER1_PATH)
                    .arg("--method")
                    .arg("org.freedesktop.DBus.Properties.Get")
                    .arg(FILE_MANAGER1_NAME)
                    .arg("OpenLocations");
                cmd
            }
            BusTool::DbusSend => {
                let mut cmd = self.base();
                cmd.arg("--session")
                    .arg("--print-reply")
                    .arg(format!("--dest={FILE_MANAGER1_NAME}"))
                    .arg(FILE_MANAGER1_PATH)
                    .arg("org.freedesktop.DBus.Properties.Get")
                    .arg(format!("string:{FILE_MANAGER1_NAME}"))
                    .arg("string:OpenLocations");
                cmd
            }
        };
        let out = TempFile::reserve("dbus");
        let Ok(sink) = std::fs::File::create(out.path()) else {
            return Vec::new();
        };
        cmd.stdout(Stdio::from(sink));
        if !run_bounded(cmd, deadline) {
            return Vec::new();
        }
        let Some(text) = read_capped(out.path()) else {
            return Vec::new();
        };
        quoted_strings(&text)
            .into_iter()
            .filter_map(|s| path_from_uri(&s))
            .filter(|p| p.is_dir())
            .take(MAX_LOCATIONS)
            .collect()
    }

    fn base(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }
}

/// Pull the quoted strings out of a D-Bus tool's reply. gdbus prints GVariant
/// (`(<['file:///a', "it's"]>,)` — single quotes, switching to double when the
/// value contains an apostrophe, which `g_file_get_uri` does not escape) and
/// dbus-send prints `array [ string "file:///a" ]`. Tracking the opening
/// quote handles both without a format-specific parser.
fn quoted_strings(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\'' && c != '"' {
            continue;
        }
        let quote = c;
        let mut s = String::new();
        loop {
            match chars.next() {
                // Unterminated: whatever follows is not trustworthy.
                None => return found,
                Some('\\') => match chars.next() {
                    Some(escaped) => s.push(escaped),
                    None => return found,
                },
                Some(c) if c == quote => break,
                Some(c) => s.push(c),
            }
        }
        found.push(s);
    }
    found
}

// ------------------------------------------------------------------ parsing

/// The first line of `text` that names a file that actually exists.
///
/// Multi-line is the normal case, not the exception: several selected files
/// arrive as one URI per line, and Nautilus's `x-special/gnome-copied-files`
/// leads with a bare `copy` verb. Taking the first *usable* line handles both
/// — there is no way to know which of several the user meant, so the first
/// one wins.
fn first_usable(text: &str) -> Option<PathBuf> {
    text.lines()
        .take(MAX_LINES)
        .filter_map(candidate)
        .find(|path| usable(path))
}

/// One line of clipboard text as a path, or `None` if it cannot be one. Pure:
/// it never touches the filesystem, so `usable` still has to run afterwards.
fn candidate(line: &str) -> Option<PathBuf> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match scheme_of(line) {
        // `file:` is the only scheme naming something we can open. `https:`,
        // `smb:`, `sftp:` and friends are all rejected: they may be perfectly
        // valid, they are just not local files.
        Some(scheme) if scheme.eq_ignore_ascii_case("file") => path_from_uri(line),
        Some(_) => None,
        // No scheme: a plain path. Anything relative is rejected here rather
        // than guessed at, since we have no directory to resolve it against.
        None if line.starts_with('/') => Some(PathBuf::from(line)),
        None => None,
    }
}

/// The URI scheme of `s`, if it has one. A scheme is letters, digits, `+`,
/// `-` and `.` after a leading letter, terminated by `:` — and it must come
/// before any `/`, so `/home/a:b/c` is a path, not a `home`-scheme URI.
fn scheme_of(s: &str) -> Option<&str> {
    let end = s.find(':')?;
    if s[..end].contains('/') {
        return None;
    }
    let scheme = &s[..end];
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some(scheme)
}

/// `file:///home/u/My%20Report.pdf` → `/home/u/My Report.pdf`.
///
/// Decoding is done on bytes, not chars, because a Linux filename is a byte
/// string: `%C3%A9` has to come back out as the two bytes of `é`, and a
/// filename in some legacy encoding has to survive unchanged.
fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let rest = strip_prefix_ignore_ascii_case(uri, "file:")?;
    let path = match strip_prefix_ignore_ascii_case(rest, "//") {
        Some(after_slashes) => {
            // `file://host/path` — the authority runs to the next `/`.
            let end = after_slashes.find('/')?;
            let (host, path) = after_slashes.split_at(end);
            // A named host means the file lives on another machine, which is
            // not something we can open. Empty and `localhost` are us.
            if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
                return None;
            }
            path
        }
        // `file:/path` — legal per RFC 8089, just rare.
        None => rest,
    };

    let bytes = percent_decode(path);
    // Must be absolute, and a NUL can never appear in a path — a URI
    // containing `%00` is malformed or hostile, not a filename.
    if !bytes.starts_with(b"/") || bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// `%XX` → byte. Anything that is not a well-formed escape is passed through
/// literally: a stray `%` in a filename is far more likely than a truncated
/// escape, and dropping bytes would corrupt the name either way.
fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `text` read as a bare filename — one line, no directory part, nothing
/// exotic. Everything else is rejected, because this name is about to be
/// joined onto a directory we did not get it from.
fn bare_name(text: &str) -> Option<OsString> {
    let line = text.lines().next()?.trim();
    if line.is_empty() || line.len() > MAX_NAME_LEN {
        return None;
    }
    // A separator would let a stray selection walk out of the folder, and
    // `.`/`..` name the folder itself rather than anything in it.
    if line.contains('/') || line == "." || line == ".." {
        return None;
    }
    if line.chars().any(|c| c.is_control()) {
        return None;
    }
    // A URI is not a bare name; if it were usable, an earlier pass took it.
    if scheme_of(line).is_some() {
        return None;
    }
    Some(OsString::from(line))
}

// -------------------------------------------------- child process control

/// Spawn, wait under a deadline, and report whether the child exited cleanly
/// in time. A timeout is a `false`, never an error: every caller has a
/// fallback, and "the clipboard tool did not answer" is a normal Tuesday.
fn run_bounded(mut cmd: Command, deadline: Instant) -> bool {
    let now = Instant::now();
    if now >= deadline {
        return false;
    }
    // Whichever expires first: this child's own leash, or what is left of
    // the whole hotkey budget.
    let child_deadline = (now + CHILD_TIMEOUT).min(deadline);
    let Ok(mut child) = cmd.spawn() else {
        // On PATH a moment ago, not runnable now. Same as "no tool".
        return false;
    };
    wait_bounded(&mut child, child_deadline)
}

/// Poll `try_wait` until the child exits or the deadline passes, killing and
/// reaping it in the latter case. This is the guarantee that a wedged `xclip`
/// cannot wedge the daemon.
fn wait_bounded(child: &mut Child, deadline: Instant) -> bool {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => {
                kill_and_reap(child);
                return false;
            }
        }
        if Instant::now() >= deadline {
            kill_and_reap(child);
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Always both: `kill` only sends the signal, `wait` is what keeps a resident
/// daemon from collecting zombies for the rest of its life.
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Read a child's output back, capped. Lossy on purpose: clipboard contents
/// are arbitrary bytes, and a path we cannot decode as UTF-8 was never going
/// to survive being compared against anything anyway.
fn read_capped(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_READ).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn env_set(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|value| !value.is_empty())
}

/// Resolve `name` against PATH by hand — a `which` dependency would buy
/// nothing over `env::split_paths`. Mirrors `sekio-core`'s video renderer,
/// minus the Windows half this module cannot be compiled for.
fn find_on_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        // An empty PATH entry means "the current directory" to some shells.
        // Running whatever happens to sit in the cwd is a well-known
        // foot-gun, so skip it.
        if dir.as_os_str().is_empty() {
            continue;
        }
        let full = dir.join(name);
        if std::fs::metadata(&full)
            .map(|meta| is_executable(&meta))
            .unwrap_or(false)
        {
            return Some(full);
        }
    }
    None
}

fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.is_file() && meta.permissions().mode() & 0o111 != 0
}

// ------------------------------------------------------------- temp files

/// A path under the OS temp dir, deleted when the guard drops — on every
/// path out, including the timeout one.
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    /// The pid separates concurrent sekio processes and the counter
    /// separates concurrent lookups inside one, so no randomness is needed.
    fn reserve(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("sekio-{tag}-{}-{n}.txt", std::process::id());
        Self {
            path: std::env::temp_dir().join(name),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // Best effort: a temp file we cannot remove must not panic a hotkey.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that cleans itself up, so the filesystem-touching
    /// tests can exercise the real `usable()` guard instead of mocking it.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("sekio-seltest-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Self { dir }
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.dir.join(name);
            std::fs::write(&path, b"x").expect("write scratch file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn uri_for(path: &Path) -> String {
        // Only the characters our fixtures actually use need escaping.
        let text = path.to_string_lossy().replace(' ', "%20");
        format!("file://{text}")
    }

    // ------------------------------------------------------------- parsing

    #[test]
    fn parses_a_plain_file_uri() {
        assert_eq!(
            path_from_uri("file:///home/u/report.pdf"),
            Some(PathBuf::from("/home/u/report.pdf"))
        );
        // No authority at all is legal too.
        assert_eq!(
            path_from_uri("file:/etc/hosts"),
            Some(PathBuf::from("/etc/hosts"))
        );
        // localhost is this machine.
        assert_eq!(
            path_from_uri("file://localhost/etc/hosts"),
            Some(PathBuf::from("/etc/hosts"))
        );
        // Schemes are case-insensitive.
        assert_eq!(
            path_from_uri("FILE:///etc/hosts"),
            Some(PathBuf::from("/etc/hosts"))
        );
    }

    #[test]
    fn percent_decodes_spaces_and_punctuation() {
        assert_eq!(
            path_from_uri("file:///home/u/My%20Report%20(final).pdf"),
            Some(PathBuf::from("/home/u/My Report (final).pdf"))
        );
        assert_eq!(
            path_from_uri("file:///tmp/a%23b%25c.txt"),
            Some(PathBuf::from("/tmp/a#b%c.txt"))
        );
    }

    #[test]
    fn percent_decodes_non_ascii_as_bytes() {
        // "é" and a CJK name: multi-byte UTF-8 arrives as one escape per byte.
        assert_eq!(
            path_from_uri("file:///home/u/caf%C3%A9.txt"),
            Some(PathBuf::from("/home/u/café.txt"))
        );
        assert_eq!(
            path_from_uri("file:///home/u/%E6%97%A5%E6%9C%AC%E8%AA%9E.md"),
            Some(PathBuf::from("/home/u/日本語.md"))
        );
        // Bytes that are not valid UTF-8 still make a path — Linux filenames
        // are byte strings, and this one must not be lost or mangled.
        let latin1 = path_from_uri("file:///tmp/caf%E9.txt").expect("byte path");
        assert_eq!(latin1.as_os_str().as_encoded_bytes(), b"/tmp/caf\xE9.txt");
    }

    #[test]
    fn malformed_escapes_are_left_alone() {
        assert_eq!(percent_decode("100%"), b"100%");
        assert_eq!(percent_decode("a%2"), b"a%2");
        assert_eq!(percent_decode("a%ZZb"), b"a%ZZb");
        assert_eq!(percent_decode("%41%42"), b"AB");
    }

    #[test]
    fn rejects_uris_that_are_not_local_files() {
        for uri in [
            "https://example.com/report.pdf",
            "http://example.com/",
            "smb://server/share/file.txt",
            "sftp://host/home/u/x",
            "trash:///report.pdf",
            "recent:///",
            "data:text/plain,hello",
        ] {
            assert_eq!(candidate(uri), None, "{uri} must not be treated as a path");
        }
        // A file URI on another machine is a valid URI and still not ours.
        assert_eq!(path_from_uri("file://fileserver/share/x.txt"), None);
        // Relative and NUL-bearing URIs are malformed.
        assert_eq!(path_from_uri("file://relative/../x"), None);
        assert_eq!(path_from_uri("file:///tmp/a%00b"), None);
        assert_eq!(path_from_uri("notfile:///tmp/x"), None);
    }

    #[test]
    fn rejects_text_that_is_not_a_path() {
        for text in [
            "",
            "   ",
            "just some copied text",
            "relative/path.txt",
            "./also/relative",
            "Lorem ipsum dolor sit amet",
            "12345",
        ] {
            assert_eq!(candidate(text), None, "{text:?} must not be a candidate");
        }
        // A colon in a directory name is not a URI scheme.
        assert_eq!(
            candidate("/home/u/notes:2024/x.md"),
            Some(PathBuf::from("/home/u/notes:2024/x.md"))
        );
    }

    #[test]
    fn takes_the_first_usable_line_of_many() {
        let scratch = Scratch::new("multi");
        let first = scratch.file("first file.txt");
        let second = scratch.file("second.txt");

        // Several selected files arrive as one URI per line.
        let text = format!("{}\n{}\n", uri_for(&first), uri_for(&second));
        assert_eq!(first_usable(&text), Some(first.clone()));

        // Nautilus's x-special/gnome-copied-files leads with a verb, and a
        // missing file must be skipped rather than ending the search.
        let text = format!(
            "copy\nfile:///definitely/not/here/at/all\n{}\n{}",
            uri_for(&second),
            uri_for(&first)
        );
        assert_eq!(first_usable(&text), Some(second));

        // Plain paths, CRLF line endings, and blank lines all work.
        let text = format!("\r\n\r\n{}\r\n", first.display());
        assert_eq!(first_usable(&text), Some(first));
    }

    #[test]
    fn nothing_usable_yields_nothing() {
        assert_eq!(first_usable(""), None);
        assert_eq!(first_usable("some text\nmore text\n"), None);
        assert_eq!(first_usable("file:///definitely/not/here/at/all"), None);
        // A directory that exists is usable — previewing one is meaningful.
        let tmp = std::env::temp_dir();
        assert_eq!(first_usable(&uri_for(&tmp)), Some(tmp));
    }

    #[test]
    fn a_huge_clipboard_is_bounded() {
        // A copied document must not be walked line by line forever.
        let mut text = "not a path\n".repeat(10_000);
        text.push_str("/etc\n");
        assert_eq!(first_usable(&text), None);
    }

    // -------------------------------------------------------- bare names

    #[test]
    fn bare_names_accept_only_a_plain_filename() {
        assert_eq!(bare_name("report.pdf"), Some(OsString::from("report.pdf")));
        assert_eq!(
            bare_name("  My Photo.png  \nsecond line"),
            Some(OsString::from("My Photo.png"))
        );
        for text in [
            "",
            "   ",
            "sub/dir.txt",
            "/absolute/path",
            ".",
            "..",
            "file:///tmp/x",
            "https://example.com",
        ] {
            assert_eq!(bare_name(text), None, "{text:?} must not be a bare name");
        }
        // NAME_MAX is the ceiling; nothing longer can exist on disk.
        assert!(bare_name(&"a".repeat(MAX_NAME_LEN)).is_some());
        assert_eq!(bare_name(&"a".repeat(MAX_NAME_LEN + 1)), None);
        // Control characters would mean this is not really a filename.
        assert_eq!(bare_name("bell\u{7}.txt"), None);
    }

    // ------------------------------------------------------- D-Bus replies

    #[test]
    fn extracts_strings_from_both_dbus_reply_formats() {
        // gdbus prints GVariant.
        let gdbus = "(<['file:///home/u/Documents', 'file:///home/u/Pictures']>,)\n";
        assert_eq!(
            quoted_strings(gdbus),
            vec![
                "file:///home/u/Documents".to_string(),
                "file:///home/u/Pictures".to_string()
            ]
        );
        // ...switching to double quotes when the value holds an apostrophe,
        // which g_file_get_uri leaves unescaped.
        let apostrophe = r#"(<["file:///home/u/Bob's Files"]>,)"#;
        assert_eq!(
            quoted_strings(apostrophe),
            vec!["file:///home/u/Bob's Files".to_string()]
        );
        // dbus-send prints its own thing.
        let dbus_send = "array [\n   string \"file:///home/u/Downloads\"\n]\n";
        assert_eq!(
            quoted_strings(dbus_send),
            vec!["file:///home/u/Downloads".to_string()]
        );
        // An unterminated quote must not hang or panic.
        assert_eq!(quoted_strings("'unfinished"), Vec::<String>::new());
        assert_eq!(quoted_strings(""), Vec::<String>::new());
        assert_eq!(quoted_strings(r"'a\'b'"), vec!["a'b".to_string()]);
    }

    /// Exercises the real `gdbus`/`dbus-send` plumbing against the one name
    /// that is always present when a session bus is: the bus daemon itself.
    /// Deliberately does *not* touch `FileManager1`, which is activatable —
    /// asking about it the wrong way would launch a file manager.
    #[test]
    fn name_has_owner_talks_to_a_real_bus() {
        let Some(bus) = Bus::detect() else {
            eprintln!("skipping: no session bus or D-Bus tool");
            return;
        };
        let deadline = Instant::now() + TOTAL_BUDGET;
        assert!(
            bus.name_has_owner("org.freedesktop.DBus", deadline),
            "the bus daemon always owns its own name"
        );
        assert!(!bus.name_has_owner("org.sekio.NoSuchServiceXyzzy", deadline));
    }

    // --------------------------------------------------------- path lookup

    #[test]
    fn finds_a_binary_that_exists_on_path() {
        match find_on_path("sh") {
            Some(found) => assert!(found.exists(), "{} should exist", found.display()),
            None => eprintln!("skipping: no sh on PATH"),
        }
    }

    #[test]
    fn returns_none_for_a_nonsense_binary() {
        assert!(find_on_path("sekio-definitely-not-a-real-binary-xyzzy").is_none());
        // An empty name must never resolve to a directory.
        assert!(find_on_path("").is_none());
    }

    // ------------------------------------------------------ child bounding

    /// The promise the whole module rests on: a child that never exits is
    /// killed at the deadline rather than hanging the hotkey.
    #[test]
    fn a_hung_child_is_killed_at_the_deadline() {
        let Some(sleeper) = find_on_path("sleep") else {
            eprintln!("skipping: no sleep binary");
            return;
        };
        let mut cmd = Command::new(sleeper);
        cmd.arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let started = Instant::now();
        let ok = run_bounded(cmd, Instant::now() + TOTAL_BUDGET);
        let elapsed = started.elapsed();

        assert!(!ok, "a killed child never counts as success");
        assert!(
            elapsed < Duration::from_secs(2),
            "waited {elapsed:?} on a {CHILD_TIMEOUT:?} leash — the child was not killed"
        );
    }

    #[test]
    fn an_already_expired_deadline_spawns_nothing() {
        let Some(sleeper) = find_on_path("sleep") else {
            eprintln!("skipping: no sleep binary");
            return;
        };
        let mut cmd = Command::new(sleeper);
        cmd.arg("30").stdin(Stdio::null());
        let started = Instant::now();
        assert!(!run_bounded(cmd, Instant::now() - Duration::from_secs(1)));
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn temp_guard_removes_its_file_on_drop() {
        let recorded = {
            let tmp = TempFile::reserve("guardtest");
            std::fs::write(tmp.path(), b"scratch").expect("write temp file");
            tmp.path().to_path_buf()
        };
        assert!(!recorded.exists(), "TempFile::drop must delete its file");
    }

    #[test]
    fn read_capped_stops_at_the_limit() {
        let tmp = TempFile::reserve("capped");
        let big = "x".repeat(MAX_READ as usize * 2);
        std::fs::write(tmp.path(), &big).expect("write temp file");
        let text = read_capped(tmp.path()).expect("read back");
        assert_eq!(text.len(), MAX_READ as usize);
    }

    // ------------------------------------------------------ the whole thing

    #[test]
    fn describes_itself_without_lying() {
        let desktop = Desktop::new();
        let described = desktop.describe();
        assert!(described.starts_with("Linux desktop"), "{described}");
        // Whatever it claims must match what was actually found.
        match desktop.clipboard.as_ref().map(|c| c.tool) {
            Some(Tool::WlPaste) => assert!(described.contains("wl-paste")),
            Some(Tool::Xclip) => assert!(described.contains("xclip")),
            Some(Tool::Xsel) => assert!(described.contains("xsel")),
            None => assert!(described.contains("unavailable")),
        }
    }

    /// A stand-in for `xclip` that prints `output` and exits with `code`,
    /// so the whole spawn → temp file → parse → `usable` chain can be
    /// exercised on a machine with no display server.
    fn fake_tool(scratch: &Scratch, output: &str, code: i32) -> Option<PathBuf> {
        find_on_path("sh")?;
        let script = scratch.dir.join("fake-xclip");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s' \"{output}\"\nexit {code}\n"),
        )
        .ok()?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).ok()?;
        Some(script)
    }

    fn with_tool(bin: PathBuf) -> Desktop {
        Desktop {
            clipboard: Some(Clipboard {
                tool: Tool::Xclip,
                bin,
            }),
            // No bus: this is testing the clipboard chain, and nothing here
            // may talk to a real session bus.
            bus: None,
        }
    }

    #[test]
    fn reads_a_uri_all_the_way_through_a_child_process() {
        let scratch = Scratch::new("e2e");
        let wanted = scratch.file("selected file.txt");
        let Some(bin) = fake_tool(&scratch, &uri_for(&wanted), 0) else {
            eprintln!("skipping: no sh to build a fake tool");
            return;
        };

        let found = with_tool(bin).current().expect("should resolve the URI");
        assert_eq!(found.path, wanted);
        // A path lifted out of a shared buffer is a clipboard find, whatever
        // shape it has — only the D-Bus-assisted path claims otherwise.
        assert_eq!(found.origin, Origin::Clipboard);
    }

    #[test]
    fn a_failing_tool_and_junk_text_both_yield_nothing() {
        let scratch = Scratch::new("e2e-none");
        // Exit 1 is what "Can't open display" and "no selection owner" look
        // like from out here.
        if let Some(bin) = fake_tool(&scratch, "", 1) {
            assert_eq!(with_tool(bin).current(), None);
        }
        let scratch = Scratch::new("e2e-junk");
        if let Some(bin) = fake_tool(&scratch, "just some copied text", 0) {
            assert_eq!(with_tool(bin).current(), None);
        }
    }

    /// The D-Bus half, end to end, against a stand-in for `gdbus`: PRIMARY
    /// holds a bare filename, the "file manager" reports an open folder, and
    /// the two together name a real file. Uses a fake rather than the real
    /// bus because the real call would *activate* a file manager.
    #[test]
    fn a_bare_name_resolves_against_an_open_folder() {
        let scratch = Scratch::new("bare");
        let wanted = scratch.file("photo.png");
        let Some(clip) = fake_tool(&scratch, "photo.png", 0) else {
            eprintln!("skipping: no sh to build a fake tool");
            return;
        };

        let gdbus = scratch.dir.join("fake-gdbus");
        let script = format!(
            "#!/bin/sh\ncase \"$*\" in\n  *OpenLocations*) printf \"(<['file://{}']>,)\\n\" ;;\n  *) printf '(true,)\\n' ;;\nesac\n",
            scratch.dir.display()
        );
        std::fs::write(&gdbus, script).expect("write fake gdbus");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gdbus, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake gdbus");

        let desktop = Desktop {
            clipboard: Some(Clipboard {
                tool: Tool::Xclip,
                bin: clip,
            }),
            bus: Some(Bus {
                tool: BusTool::Gdbus,
                bin: gdbus,
            }),
        };

        let found = desktop.current().expect("bare name should resolve");
        assert_eq!(found.path, wanted);
        // The folder came from the file manager, so this one is not a
        // clipboard find.
        assert_eq!(found.origin, Origin::FileManager);
    }

    /// A tool that never returns must not take the hotkey with it.
    #[test]
    fn a_wedged_tool_cannot_outlast_the_budget() {
        let scratch = Scratch::new("e2e-hang");
        let Some(sleeper) = find_on_path("sleep") else {
            eprintln!("skipping: no sleep binary");
            return;
        };
        let script = scratch.dir.join("fake-xclip");
        if std::fs::write(
            &script,
            format!("#!/bin/sh\nexec {} 30\n", sleeper.display()),
        )
        .is_err()
        {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        if std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).is_err() {
            return;
        }

        let started = Instant::now();
        let found = with_tool(script).current();
        let elapsed = started.elapsed();

        assert_eq!(found, None);
        assert!(
            elapsed < TOTAL_BUDGET * 3,
            "a wedged clipboard tool held the hotkey for {elapsed:?}"
        );
    }

    /// Headless degradation, which is the only path this machine can prove:
    /// with no display server there is nothing to read, and `current()` must
    /// say so immediately instead of hanging, erroring, or guessing.
    #[test]
    fn current_degrades_instead_of_hanging() {
        let desktop = Desktop::new();
        let started = Instant::now();
        let found = desktop.current();
        let elapsed = started.elapsed();

        assert!(
            elapsed < TOTAL_BUDGET * 3,
            "current() took {elapsed:?}, well past its {TOTAL_BUDGET:?} budget"
        );
        if env_set("DISPLAY") || env_set("WAYLAND_DISPLAY") {
            // On a real desktop the answer depends on what is selected right
            // now; all we can require is that it is a file if it is anything.
            if let Some(selection) = found {
                assert!(usable(&selection.path));
            }
        } else {
            assert_eq!(found, None, "no display server means no selection");
        }
    }

    #[test]
    fn repeated_lookups_are_stable_and_cheap() {
        let desktop = Desktop::new();
        let started = Instant::now();
        for _ in 0..5 {
            let _ = desktop.current();
        }
        assert!(
            started.elapsed() < TOTAL_BUDGET * 6,
            "five lookups should not add up on a headless machine"
        );
    }
}
