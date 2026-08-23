//! The Unix half of the daemon: a `SOCK_STREAM` Unix domain socket under
//! `$XDG_RUNTIME_DIR`, and the stale-socket dance that a filesystem-backed
//! rendezvous point forces on anyone who uses one.
//!
//! The Windows backend has no equivalent of most of this file, and that is the
//! point of the split: a socket is a *file*, so it outlives the process that
//! made it, and telling a live daemon from the corpse of a crashed one is work
//! that only exists here.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use super::{encode_request, tag, Accepted, Bind, Handoff, IO_TIMEOUT};

/// What [`bind`] hands back and [`accept_one`] accepts on.
///
/// A plain alias here and a real type on Windows: callers only ever move it to
/// the socket thread and pass it back, so they need a name, not an interface.
pub type Listener = UnixListener;

// ---------------------------------------------------------------------------
// Socket location
// ---------------------------------------------------------------------------

/// Where this user's daemon for this session listens.
pub fn socket_path() -> PathBuf {
    socket_path_for(
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        std::env::temp_dir(),
        current_uid(),
        session_id().as_deref(),
    )
}

/// Pure form of [`socket_path`], so the naming rules can be tested without
/// touching the environment (which is process-global and racy under `cargo
/// test`'s thread pool).
///
/// `$XDG_RUNTIME_DIR` is preferred because it is per-user, mode 0700 and wiped
/// at logout; `temp_dir()` is the fallback for sessions that do not set it.
pub fn socket_path_for(
    runtime_dir: Option<&OsStr>,
    temp_dir: PathBuf,
    uid: u32,
    session: Option<&str>,
) -> PathBuf {
    let dir = runtime_dir
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or(temp_dir);
    dir.join(socket_name(uid, session))
}

/// `sekio-gui-<uid>-<session>.sock`.
///
/// The uid keeps two users apart in the shared `temp_dir()` fallback, and the
/// session tag keeps one user's two logins (a Wayland seat and an X11 one, or
/// two nested compositors) from stealing each other's popups. The tag is a
/// readable prefix plus a hash of the full value, so it stays recognisable
/// while an arbitrarily long `$WAYLAND_DISPLAY` still cannot overflow
/// `sun_path` (108 bytes).
pub fn socket_name(uid: u32, session: Option<&str>) -> String {
    let session = match session.filter(|s| !s.is_empty()) {
        Some(session) => tag(session),
        None => "nosession".to_string(),
    };
    format!("sekio-gui-{uid}-{session}.sock")
}

/// Which display server session we belong to. Wayland first: under XWayland
/// both variables are set, and the Wayland one is the finer-grained answer.
fn session_id() -> Option<String> {
    for key in ["WAYLAND_DISPLAY", "DISPLAY"] {
        if let Some(value) = std::env::var_os(key) {
            let value = value.to_string_lossy().into_owned();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// The real uid. `std` exposes no `getuid`, and this file is Unix-only
/// anyway, so it is read off `/proc/self`'s owner, with `$HOME`'s owner as the
/// fallback for the odd setup without `/proc` and 0 if even that fails. A
/// wrong answer here costs a shared socket *name*, not correctness: the
/// per-user runtime dir still separates users, and the `temp_dir()` fallback
/// separates them by the socket's own ownership.
fn current_uid() -> u32 {
    if let Ok(meta) = fs::metadata("/proc/self") {
        return meta.uid();
    }
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(meta) = fs::metadata(home) {
            return meta.uid();
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Removes the socket file when the daemon goes away.
///
/// `std` does not unlink a `UnixListener`'s path on drop, so without this every
/// exit would leave a stale socket behind. The identity check matters: by the
/// time we exit, another daemon may already have replaced the file, and
/// deleting *its* socket would break a live process.
#[derive(Debug)]
pub struct SocketGuard {
    path: PathBuf,
    ident: Option<SocketIdent>,
}

/// Enough of a socket's inode to recognise it later.
///
/// Device and inode alone are not enough: deleting a socket frees its inode
/// number, and the very next `bind` on the same directory routinely gets it
/// back — a successor daemon would then look exactly like us. The creation
/// timestamp is what actually distinguishes the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdent {
    dev: u64,
    ino: u64,
    ctime: (i64, i64),
}

impl SocketGuard {
    fn new(path: PathBuf) -> Self {
        let ident = ident_of(&path);
        Self { path, ident }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Unlink the socket if it is still the one we bound.
    pub fn release(&self) {
        remove_socket(&self.path, self.ident);
    }

    fn ident(&self) -> Option<SocketIdent> {
        self.ident
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        self.release();
    }
}

fn ident_of(path: &Path) -> Option<SocketIdent> {
    fs::metadata(path).ok().map(|m| SocketIdent {
        dev: m.dev(),
        ino: m.ino(),
        ctime: (m.ctime(), m.ctime_nsec()),
    })
}

/// Remove `path` only if it is still the socket identified by `ident`.
fn remove_socket(path: &Path, ident: Option<SocketIdent>) {
    if ident.is_some() && ident_of(path) == ident {
        let _ = fs::remove_file(path);
    }
}

/// Bind the listening socket, resolving both races that matter.
///
/// `EADDRINUSE` means the *file* exists, which says nothing about whether a
/// process is behind it. The only way to tell a live daemon from the corpse of
/// a crashed one is to try to connect:
///
/// * connect succeeds -> a daemon is live, we lost the race, become a client;
/// * connect is refused -> the file is stale, unlink it and bind once more. If
///   that second bind also collides, the winner is by definition live (it bound
///   after our unlink), so we again become a client. Two daemons starting
///   together therefore always end with exactly one listener.
pub fn bind(path: &Path) -> io::Result<Bind> {
    match UnixListener::bind(path) {
        Ok(listener) => Ok(Bind::Bound(listener, SocketGuard::new(path.to_path_buf()))),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                return Ok(Bind::AlreadyRunning);
            }
            if !is_socket(path) {
                // Something that is not a socket squats the name. Refuse to
                // delete it; the caller reports the error.
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "path exists and is not a socket",
                ));
            }
            let _ = fs::remove_file(path);
            match UnixListener::bind(path) {
                Ok(listener) => Ok(Bind::Bound(listener, SocketGuard::new(path.to_path_buf()))),
                Err(err) if err.kind() == io::ErrorKind::AddrInUse => Ok(Bind::AlreadyRunning),
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    }
}

fn is_socket(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

/// Accept exactly one connection and read what it carries. Only a failure of
/// the listener itself is an `Err`.
pub fn accept_one(listener: &Listener) -> io::Result<Accepted> {
    let (mut stream, _addr) = listener.accept()?;
    // A client that connects and then says nothing must not stall the next
    // popup, so neither direction may block indefinitely.
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    Ok(super::answer(&mut stream))
}

/// Unlink the socket on SIGINT/SIGTERM/SIGHUP.
///
/// The `Drop` guard covers every ordinary exit, but a `kill` (or a Ctrl-C in
/// the terminal that started the daemon) unwinds nothing. This is the one
/// place a dependency buys something `std` cannot express at all: `std` has no
/// way to install a signal handler, so `signal_hook` — pure Rust over `libc`,
/// no C toolchain, already in this workspace's lockfile via crossterm — runs
/// the cleanup on a dedicated thread.
pub fn install_signal_cleanup(guard: &SocketGuard) -> io::Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let path = guard.path().to_path_buf();
    let ident = guard.ident();
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
    std::thread::Builder::new()
        .name("sekio-signals".to_owned())
        .spawn(move || {
            if signals.forever().next().is_some() {
                remove_socket(&path, ident);
                std::process::exit(0);
            }
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Is a daemon answering on `socket` right now?
///
/// Connect and close, sending nothing: the daemon reads an empty request and
/// treats it as [`Accepted::Probe`], exactly like the liveness check [`bind`]
/// makes when it loses the startup race. Nothing is previewed and no window
/// moves, which is what makes this safe for `--doctor` to call.
pub fn is_running(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

/// [`super::try_handoff`] against an explicit socket, so tests need no
/// environment.
pub fn try_handoff_at(socket: &Path, path: &Path) -> Handoff {
    let message = match encode_request(path) {
        Ok(message) => message,
        Err(err) => return Handoff::Unavailable(format!("cannot hand off this path: {err}")),
    };

    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(err) => {
            // ECONNREFUSED means the socket file outlived the process that
            // created it (crash, SIGKILL, OOM). Unlink it here, in the client:
            // a crashed daemon must never permanently break `sekio-gui`, and
            // the next `--daemon` needs a clean name to bind.
            if err.kind() == io::ErrorKind::ConnectionRefused && is_socket(socket) {
                let _ = fs::remove_file(socket);
                return Handoff::Unavailable("removed a stale socket".to_owned());
            }
            return Handoff::Unavailable(err.to_string());
        }
    };
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    super::deliver(&mut stream, &message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::mpsc;

    /// A private directory for one test's sockets, short enough to stay well
    /// inside `sun_path`'s 108 bytes.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sekio-ipc-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn socket_path_prefers_xdg_runtime_dir() {
        let path = socket_path_for(
            Some(OsStr::new("/run/user/1000")),
            PathBuf::from("/tmp"),
            1000,
            Some("wayland-0"),
        );
        assert_eq!(path.parent(), Some(Path::new("/run/user/1000")));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("sekio-gui-1000-"), "uid must be in {name}");
        assert!(name.contains("wayland-0"), "session must be in {name}");
        assert!(name.ends_with(".sock"));
    }

    #[test]
    fn socket_path_falls_back_to_temp_dir() {
        let unset = socket_path_for(None, PathBuf::from("/tmp"), 7, Some(":0"));
        assert_eq!(unset.parent(), Some(Path::new("/tmp")));
        // A relative XDG_RUNTIME_DIR is not usable either.
        let relative = socket_path_for(
            Some(OsStr::new("run/user/7")),
            PathBuf::from("/tmp"),
            7,
            Some(":0"),
        );
        assert_eq!(relative.parent(), Some(Path::new("/tmp")));
    }

    #[test]
    fn socket_name_separates_users_and_sessions() {
        let a = socket_name(1000, Some("wayland-0"));
        let b = socket_name(1001, Some("wayland-0"));
        let c = socket_name(1000, Some("wayland-1"));
        let d = socket_name(1000, None);
        assert_ne!(a, b, "two users must not share a socket");
        assert_ne!(a, c, "two sessions must not share a socket");
        assert_ne!(a, d);
        assert_eq!(a, socket_name(1000, Some("wayland-0")), "must be stable");
        // Two sessions that sanitize to the same prefix still differ.
        assert_ne!(
            socket_name(1000, Some("/run/user/1000/wayland-0")),
            socket_name(1000, Some("/run/user/1000/wayland-1")),
        );
    }

    #[test]
    fn socket_name_stays_inside_sun_path() {
        let hostile = "x".repeat(4000);
        let name = socket_name(u32::MAX, Some(&hostile));
        assert!(name.len() < 64, "sun_path is 108 bytes, got {}", name.len());
    }

    #[test]
    fn missing_socket_is_reported_as_unavailable() {
        let dir = scratch("absent");
        let socket = dir.join("nothing.sock");
        assert!(matches!(
            try_handoff_at(&socket, Path::new("/etc/hostname")),
            Handoff::Unavailable(_)
        ));
        assert!(!socket.exists(), "must not create anything");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_socket_is_detected_and_removed_by_the_client() {
        let dir = scratch("stale");
        let socket = dir.join("d.sock");
        // A crashed daemon leaves the file behind: `std` never unlinks it.
        let listener = UnixListener::bind(&socket).expect("bind");
        drop(listener);
        assert!(socket.exists(), "the corpse of a crashed daemon");

        match try_handoff_at(&socket, Path::new("/etc/hostname")) {
            Handoff::Unavailable(_) => {}
            Handoff::Delivered => panic!("nothing is listening, nothing can be delivered"),
        }
        assert!(!socket.exists(), "the stale socket must be cleaned up");

        // And the name is free again, so a fresh daemon can start.
        match bind(&socket).expect("rebind") {
            Bind::Bound(_, guard) => guard.release(),
            Bind::AlreadyRunning => panic!("nothing is running"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bind_reclaims_a_stale_socket_and_yields_to_a_live_one() {
        let dir = scratch("race");
        let socket = dir.join("d.sock");

        let dead = UnixListener::bind(&socket).expect("bind");
        drop(dead);
        let (listener, guard) = match bind(&socket).expect("bind over the stale file") {
            Bind::Bound(listener, guard) => (listener, guard),
            Bind::AlreadyRunning => panic!("the previous daemon is dead"),
        };

        // Second daemon, same socket: it must stand down, not fight.
        assert!(
            matches!(bind(&socket).expect("second bind"), Bind::AlreadyRunning),
            "the loser of the race becomes a client"
        );

        drop(listener);
        drop(guard);
        assert!(!socket.exists(), "the guard unlinks on the way out");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn guard_leaves_a_replacement_socket_alone() {
        let dir = scratch("ident");
        let socket = dir.join("d.sock");
        let (listener, guard) = match bind(&socket).expect("bind") {
            Bind::Bound(listener, guard) => (listener, guard),
            Bind::AlreadyRunning => panic!("nothing is running"),
        };
        drop(listener);
        // A successor rebinds the same name before we finish exiting.
        fs::remove_file(&socket).expect("unlink");
        let successor = UnixListener::bind(&socket).expect("successor binds");
        drop(guard);
        assert!(socket.exists(), "must not delete another daemon's socket");
        drop(successor);
        let _ = fs::remove_dir_all(&dir);
    }

    /// End to end over a real socket: a daemon-side listener on its own
    /// thread, a client handoff, and the path arriving on the UI channel.
    #[test]
    fn handoff_delivers_the_path_to_the_ui_channel() {
        let dir = scratch("e2e");
        let socket = dir.join("d.sock");
        let (listener, guard) = match bind(&socket).expect("bind") {
            Bind::Bound(listener, guard) => (listener, guard),
            Bind::AlreadyRunning => panic!("nothing is running"),
        };

        let (tx, rx) = mpsc::channel();
        let (woke_tx, woke_rx) = mpsc::channel();
        // One connection only, then the thread ends — no listener is left
        // parked in `accept` when the test process exits.
        let server = std::thread::spawn(move || {
            if let Accepted::Path(path) = accept_one(&listener).expect("accept") {
                tx.send(path).expect("send to ui");
                woke_tx.send(()).expect("wake ui");
            }
        });

        let target = dir.join("target.txt");
        fs::write(&target, b"hello").expect("fixture");
        assert!(
            matches!(try_handoff_at(&socket, &target), Handoff::Delivered),
            "a live daemon must acknowledge the handoff"
        );
        server.join().expect("server thread");

        assert_eq!(rx.recv_timeout(IO_TIMEOUT).expect("path"), target);
        woke_rx.recv_timeout(IO_TIMEOUT).expect("ui was woken");

        drop(guard);
        assert!(!socket.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// `bind` connects to an existing socket purely to see whether anyone is
    /// home. That must not look like a bad request to the daemon it probes.
    #[test]
    fn liveness_probe_is_not_treated_as_a_bad_request() {
        let dir = scratch("probe");
        let socket = dir.join("d.sock");
        let (listener, guard) = match bind(&socket).expect("bind") {
            Bind::Bound(listener, guard) => (listener, guard),
            Bind::AlreadyRunning => panic!("nothing is running"),
        };

        let server = std::thread::spawn(move || accept_one(&listener).expect("accept"));
        // Exactly what a second daemon losing the startup race does.
        assert!(matches!(
            bind(&socket).expect("second bind"),
            Bind::AlreadyRunning
        ));
        assert_eq!(server.join().expect("server thread"), Accepted::Probe);

        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A malformed message is answered, logged and forgotten — the daemon
    /// stays up and serves the next client.
    #[test]
    fn malformed_request_does_not_stop_the_daemon() {
        let dir = scratch("garbage");
        let socket = dir.join("d.sock");
        let (listener, guard) = match bind(&socket).expect("bind") {
            Bind::Bound(listener, guard) => (listener, guard),
            Bind::AlreadyRunning => panic!("nothing is running"),
        };

        let server = std::thread::spawn(move || {
            let first = accept_one(&listener).expect("accept garbage");
            let second = accept_one(&listener).expect("accept the good one");
            (first, second)
        });

        let mut bad = UnixStream::connect(&socket).expect("connect");
        bad.write_all(b"../etc/passwd\n").expect("write");
        bad.flush().expect("flush");
        let mut ack = String::new();
        bad.read_to_string(&mut ack).expect("read ack");
        assert_eq!(ack, "err\n", "a rejected path is refused, not obeyed");
        drop(bad);

        assert!(matches!(
            try_handoff_at(&socket, Path::new("/etc/hostname")),
            Handoff::Delivered
        ));
        let (first, second) = server.join().expect("server thread");
        assert_eq!(first, Accepted::Rejected, "the bad request is dropped");
        assert_eq!(second, Accepted::Path(PathBuf::from("/etc/hostname")));

        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }
}
