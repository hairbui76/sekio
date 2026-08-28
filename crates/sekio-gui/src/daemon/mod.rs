//! Single-instance daemon (ROADMAP Phase 3 "single-instance daemon so popups
//! reuse a warm `Previewer`").
//!
//! Cold start is already cheap (~4 ms to the first preview), but a spacebar
//! popup should not pay for a process spawn, a cold page cache and a fresh
//! syntax set every time. `sekio-gui --daemon` keeps one process resident with
//! its window hidden — and so does a plain `sekio-gui`, with its window on
//! screen. Every later launch first tries to hand its work to that process and
//! exits the moment it is acknowledged: a path to preview, or `Request::Show`
//! from a launcher entry that only wants the existing window brought forward.
//!
//! Two transports, one contract. Linux uses a Unix domain socket under
//! `$XDG_RUNTIME_DIR`; Windows uses a named pipe under `\\.\pipe\`. Everything
//! above the transport — the wire format, the accept loop, the "become a
//! client instead of a second daemon" decision — is this file, shared, and
//! [`Listener`] is deliberately a platform-neutral name so no caller needs a
//! `#[cfg]` to spell the type it is handed.
//!
//! **The daemon is an optimisation, never a requirement.** Every failure mode
//! here — no socket, a stale socket from a crashed daemon, a wedged peer, a
//! path that cannot be expressed in the wire format — resolves to
//! [`Handoff::Unavailable`], and the caller opens its own window exactly as it
//! did before this module existed. Nothing in here may panic: a daemon that
//! dies on a malformed byte is worse than no daemon at all.
//!
//! Protocol: one line per connection, UTF-8, at most [`MAX_MESSAGE`] bytes
//! including the newline; the daemon answers `ok\n` or `err\n`. The line is
//! either an absolute path to preview or the literal word `show`, which asks
//! the daemon to put its window on screen — what a launcher entry sends when a
//! resident sekio already owns the session. The two can never be confused: a
//! path on this wire is always absolute, and `show` is not.
//!
//! Everything arriving on the transport is untrusted: the length bound is
//! enforced while reading (never by allocating first and truncating after),
//! the path must be absolute (the daemon's cwd is not the client's), and a
//! malformed message is logged and dropped — it must never take the daemon
//! down.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{
    accept_one, bind, install_signal_cleanup, is_running, socket_name, socket_path,
    socket_path_for, try_handoff_at, Listener, SocketGuard,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    accept_one, bind, install_signal_cleanup, is_running, pipe_name, socket_path, try_handoff_at,
    Listener, SocketGuard,
};

/// Hard bound on one request, newline included. `PATH_MAX` is 4096 on Linux
/// and a Windows path tops out well below that, so this accepts every path
/// either kernel can hand us and nothing longer: a confused (or hostile)
/// client cannot make the daemon allocate without limit.
pub const MAX_MESSAGE: usize = 4096;

/// Bound on the acknowledgement the client reads back. It is `ok\n`/`err\n`;
/// this only exists so a rogue listener cannot stream forever into a client.
const MAX_ACK: u64 = 16;

/// Reads and writes are local and answered before any preview work starts, so
/// this only fires when the peer is wedged. Both sides then give up and fall
/// back rather than hanging.
const IO_TIMEOUT: Duration = Duration::from_secs(1);

/// Consecutive accept failures tolerated before the serve loop gives up.
/// Without this an unrecoverable listener error would spin a core forever.
const MAX_ACCEPT_ERRORS: usize = 16;

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

/// Why a message could not be turned into a path to preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// Nothing but whitespace/newline arrived.
    Empty,
    /// The line hit [`MAX_MESSAGE`] without terminating.
    TooLong,
    /// Not valid UTF-8. Unix paths are bytes and Windows paths are UTF-16, but
    /// a line protocol needs one encoding; such a path is simply not handed
    /// off (the client opens its own window instead).
    NotUtf8,
    /// Relative paths are meaningless across processes: the daemon's cwd is
    /// not the client's. Clients canonicalize before sending.
    NotAbsolute,
    /// Contains a byte the line protocol cannot carry (NUL or a newline).
    Unrepresentable,
    /// The transport itself failed.
    Io(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("empty request"),
            Self::TooLong => write!(f, "request longer than {MAX_MESSAGE} bytes"),
            Self::NotUtf8 => f.write_str("request is not valid UTF-8"),
            Self::NotAbsolute => f.write_str("path is not absolute"),
            Self::Unrepresentable => f.write_str("path contains a NUL or newline"),
            Self::Io(err) => write!(f, "socket error: {err}"),
        }
    }
}

/// The word that means "put your window on screen".
///
/// Safe to distinguish from a path by comparison alone: every path on this
/// wire is absolute, and `show` is not — [`parse_request`] rejects it as a
/// path either way, so the two spellings cannot collide however the client
/// spells its own paths.
const SHOW: &str = "show";

/// What a client is asking the resident sekio to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Preview this path. The original message, and still the common one.
    Preview(PathBuf),
    /// Put the window on screen, whatever it is showing.
    ///
    /// What a launcher entry sends: clicking the Start Menu or dock icon while
    /// a resident sekio already owns this session must raise *that* window,
    /// not open a second sekio with a second tray icon beside the first.
    Show,
}

/// Turn a request into the bytes to put on the wire.
///
/// Fails for anything the format cannot carry; the caller treats that as "no
/// daemon" and opens a window itself, so an odd filename never becomes an
/// error the user sees.
pub fn encode_request(request: &Request) -> Result<Vec<u8>, ProtocolError> {
    let path = match request {
        Request::Show => return Ok(format!("{SHOW}\n").into_bytes()),
        Request::Preview(path) => path,
    };
    let text = path.to_str().ok_or(ProtocolError::NotUtf8)?;
    if text.is_empty() {
        return Err(ProtocolError::Empty);
    }
    if !path.is_absolute() {
        return Err(ProtocolError::NotAbsolute);
    }
    if text.contains('\n') || text.contains('\r') || text.contains('\0') {
        return Err(ProtocolError::Unrepresentable);
    }
    let mut out = Vec::with_capacity(text.len() + 1);
    out.extend_from_slice(text.as_bytes());
    out.push(b'\n');
    if out.len() > MAX_MESSAGE {
        return Err(ProtocolError::TooLong);
    }
    Ok(out)
}

/// Validate one received line. Pure, so every rejection path is unit-tested.
///
/// `is_absolute` answers for the OS this is compiled for, which is the right
/// question here and only here: both ends of this protocol are processes on
/// the *same* machine, so "absolute" means what the running kernel says it
/// means. Nothing in this module takes a platform as a parameter, which is the
/// case CLAUDE.md warns about; the tests build their fixtures for the host for
/// the same reason.
pub fn parse_request(bytes: &[u8]) -> Result<Request, ProtocolError> {
    if bytes.len() > MAX_MESSAGE {
        return Err(ProtocolError::TooLong);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ProtocolError::NotUtf8)?;
    let text = text.trim_end_matches(['\n', '\r']);
    if text.is_empty() {
        return Err(ProtocolError::Empty);
    }
    if text == SHOW {
        return Ok(Request::Show);
    }
    if text.contains('\0') {
        return Err(ProtocolError::Unrepresentable);
    }
    let path = PathBuf::from(text);
    if !path.is_absolute() {
        return Err(ProtocolError::NotAbsolute);
    }
    Ok(Request::Preview(path))
}

/// Read one request off a stream with the length bound applied *while*
/// reading: `take` caps what can ever reach the buffer, so an endless client
/// costs us [`MAX_MESSAGE`] bytes, not memory.
pub fn read_request<R: Read>(reader: R) -> Result<Request, ProtocolError> {
    let mut reader = BufReader::new(reader.take(MAX_MESSAGE as u64));
    let mut buf = Vec::new();
    reader
        .read_until(b'\n', &mut buf)
        .map_err(|err| ProtocolError::Io(err.to_string()))?;
    // Hit the cap without ever seeing a newline: the line is longer than we
    // are willing to read, and what we have is a prefix, not a path.
    if buf.len() >= MAX_MESSAGE && buf.last() != Some(&b'\n') {
        return Err(ProtocolError::TooLong);
    }
    parse_request(&buf)
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

/// Filename-safe, bounded prefix of a session name (`:0` -> `-0`, kept short).
///
/// Shared by both backends: a Unix socket name has to fit `sun_path`, and a
/// pipe name may not contain a `\` beyond the `\\.\pipe\` prefix. Reducing
/// everything to `[a-z0-9-]` satisfies both without either having to think.
fn sanitize(value: &str) -> String {
    let mut out: String = value
        .chars()
        .take(16)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push('x');
    }
    out
}

/// FNV-1a: a few lines of std, no dependency, and collision resistance we do
/// not need — this only has to separate one user's concurrent sessions.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// A readable-but-unique tag for one arbitrary string.
///
/// [`sanitize`] alone is not enough — it truncates, so two long values that
/// share a prefix would collide — and a bare hash is not enough either,
/// because a name nobody can read is a name nobody can debug. Both, joined.
fn tag(value: &str) -> String {
    format!("{}-{:08x}", sanitize(value), fnv1a(value.as_bytes()) as u32)
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Outcome of trying to become *the* daemon.
pub enum Bind {
    /// We own the name. The guard releases it on drop.
    Bound(Listener, SocketGuard),
    /// Another daemon owns it and is answering — this process must become a
    /// client instead of a second daemon.
    AlreadyRunning,
}

/// What one accepted connection turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Accepted {
    /// A well-formed request: a path to preview, or "show your window".
    Request(Request),
    /// A connection that closed without sending anything. On Unix [`bind`]
    /// makes exactly this connection to tell a live daemon from a stale
    /// socket, so it is routine, not an error.
    Probe,
    /// Malformed: answered and logged, and then forgotten. A bad message must
    /// never take the daemon down.
    Rejected,
}

/// Read one request off an accepted connection and answer it.
///
/// Shared by both backends: the transport differs, the conversation does not.
/// Nothing here can fail outward — a connection that says something useless is
/// still a connection the daemon survived.
fn answer<S: Read + Write>(stream: &mut S) -> Accepted {
    match read_request(&mut *stream) {
        Ok(request) => {
            // Acknowledge before any preview work happens, so the client's
            // exit is not gated on rendering.
            let _ = stream.write_all(b"ok\n");
            let _ = stream.flush();
            Accepted::Request(request)
        }
        Err(ProtocolError::Empty) => Accepted::Probe,
        Err(err) => {
            // The reply says nothing about the request: never echo untrusted
            // bytes back out. Details go to our own log.
            let _ = stream.write_all(b"err\n");
            let _ = stream.flush();
            eprintln!("sekio-gui: ignoring malformed request ({err})");
            Accepted::Rejected
        }
    }
}

/// Accept forever, handing each valid path to the UI thread.
///
/// Runs on its own thread: the UI thread must never block on the transport.
/// `wake` is `Context::request_repaint`, which is what makes a hidden window
/// run its logic again and notice the new path.
pub fn serve<F: Fn()>(listener: Listener, tx: Sender<Request>, wake: F) {
    let mut errors = 0usize;
    loop {
        match accept_one(&listener) {
            Ok(Accepted::Request(request)) => {
                errors = 0;
                if tx.send(request).is_err() {
                    break; // UI is gone; so is the reason to listen.
                }
                wake();
            }
            Ok(Accepted::Probe | Accepted::Rejected) => errors = 0,
            Err(err) => {
                eprintln!("sekio-gui: accept failed: {err}");
                errors += 1;
                if errors >= MAX_ACCEPT_ERRORS {
                    eprintln!("sekio-gui: giving up on the daemon socket");
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Result of offering a path to a running daemon.
pub enum Handoff {
    /// The daemon acknowledged it: this process is done, and can exit.
    Delivered,
    /// No usable daemon. The reason is for `--timing` only — the caller's
    /// response is always the same: open a window here.
    Unavailable(String),
}

/// Try to hand `request` to the daemon for this session. Any path inside it
/// must already be canonicalized: the daemon's cwd is not ours.
pub fn try_handoff(request: &Request) -> Handoff {
    try_handoff_at(&socket_path(), request)
}

/// Send the request and wait for the acknowledgement.
///
/// Shared by both backends. The ack is what distinguishes "the daemon took it"
/// from "something else is listening on that name"; without it a rejected path
/// would silently show nothing at all.
fn deliver<S: Read + Write>(stream: &mut S, message: &[u8]) -> Handoff {
    if let Err(err) = stream.write_all(message).and_then(|()| stream.flush()) {
        return Handoff::Unavailable(err.to_string());
    }
    let mut ack = Vec::new();
    if let Err(err) = BufReader::new((&mut *stream).take(MAX_ACK)).read_until(b'\n', &mut ack) {
        return Handoff::Unavailable(err.to_string());
    }
    if ack.starts_with(b"ok") {
        Handoff::Delivered
    } else {
        Handoff::Unavailable(format!("daemon refused the request ({} bytes)", ack.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// An absolute path *for the host these tests run on*.
    ///
    /// The wire format carries real paths between two processes on the same
    /// machine, so `parse_request` asks the running kernel what "absolute"
    /// means. A `/tmp/...` literal is not absolute on Windows — it has no
    /// drive prefix — so hard-coding one would make these tests pass on Linux
    /// for the wrong reason and fail on the Windows runner, which is exactly
    /// the trap CLAUDE.md describes.
    fn absolute(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\tmp\{name}"))
        } else {
            PathBuf::from(format!("/tmp/{name}"))
        }
    }

    fn preview(path: &Path) -> Request {
        Request::Preview(path.to_path_buf())
    }

    #[test]
    fn valid_path_round_trips() {
        let path = absolute("a file.txt");
        let encoded = encode_request(&preview(&path)).expect("encode");
        assert_eq!(encoded.last(), Some(&b'\n'), "one line per connection");
        assert_eq!(
            read_request(&encoded[..]).expect("decode"),
            Request::Preview(path)
        );
    }

    /// The launcher's message. It has to round trip like any other, and — the
    /// point of choosing this word — it can never be mistaken for a path,
    /// because a path on this wire is always absolute and `show` is not.
    #[test]
    fn show_round_trips_and_cannot_collide_with_a_path() {
        let encoded = encode_request(&Request::Show).expect("encode");
        assert_eq!(encoded, b"show\n");
        assert_eq!(read_request(&encoded[..]).expect("decode"), Request::Show);
        assert_eq!(parse_request(b"show\r\n"), Ok(Request::Show));

        // A file actually named `show` is still a path, because the client
        // canonicalizes before sending and an absolute path never spells the
        // bare word.
        let named = absolute("show");
        let encoded = encode_request(&preview(&named)).expect("encode");
        assert_eq!(
            read_request(&encoded[..]).expect("decode"),
            Request::Preview(named)
        );
        // And a relative `./show` is rejected as a path rather than silently
        // becoming the command.
        assert_eq!(parse_request(b"./show\n"), Err(ProtocolError::NotAbsolute));
    }

    #[test]
    fn relative_paths_are_rejected_on_both_sides() {
        // Relative on every platform, so no host-shaped fixture is needed.
        assert_eq!(
            encode_request(&preview(Path::new("relative/file.txt"))),
            Err(ProtocolError::NotAbsolute)
        );
        assert_eq!(
            parse_request(b"relative/file.txt\n"),
            Err(ProtocolError::NotAbsolute)
        );
        assert_eq!(parse_request(b"./x\n"), Err(ProtocolError::NotAbsolute));
    }

    #[test]
    fn oversized_input_is_rejected_without_unbounded_reads() {
        let mut flood = vec![b'a'; MAX_MESSAGE * 4];
        flood.push(b'\n');
        assert_eq!(read_request(&flood[..]), Err(ProtocolError::TooLong));
        // Same verdict for a client that never sends a newline at all.
        let endless = vec![b'/'; MAX_MESSAGE * 4];
        assert_eq!(read_request(&endless[..]), Err(ProtocolError::TooLong));
        let oversized = vec![b'a'; MAX_MESSAGE + 1];
        assert_eq!(parse_request(&oversized), Err(ProtocolError::TooLong));
    }

    #[test]
    fn non_utf8_and_empty_and_nul_are_rejected() {
        assert_eq!(
            read_request(&b"/tmp/\xff\xfe\n"[..]),
            Err(ProtocolError::NotUtf8)
        );
        assert_eq!(read_request(&b"\n"[..]), Err(ProtocolError::Empty));
        assert_eq!(read_request(&b""[..]), Err(ProtocolError::Empty));
        // The NUL and newline checks come before the absoluteness one, so
        // these verdicts do not depend on the host's idea of a path root.
        assert_eq!(
            parse_request(b"/tmp/a\0b\n"),
            Err(ProtocolError::Unrepresentable)
        );
        assert_eq!(
            encode_request(&preview(&absolute("two\nlines"))),
            Err(ProtocolError::Unrepresentable)
        );
    }

    /// `main.rs` moves the listener onto the socket thread, so both backends
    /// have to keep it `Send`. On Windows that rests on an `unsafe impl` over
    /// a raw `HANDLE`; a refactor that drops it should fail here rather than
    /// in whichever binary happens to spawn the thread.
    #[test]
    fn the_listener_can_move_to_the_socket_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Listener>();
        assert_send::<SocketGuard>();
    }

    #[test]
    fn tags_are_readable_stable_and_collision_free() {
        assert_eq!(tag("wayland-0"), tag("wayland-0"), "must be stable");
        assert_ne!(tag("wayland-0"), tag("wayland-1"));
        // Two values that sanitize to the same 16-char prefix still differ,
        // and neither can grow without bound.
        let a = tag("/run/user/1000/wayland-0");
        let b = tag("/run/user/1000/wayland-1");
        assert_ne!(a, b);
        assert!(tag(&"x".repeat(4000)).len() <= 25);
    }
}
