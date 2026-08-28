//! The Windows half of the daemon: a named pipe under `\\.\pipe\`.
//!
//! A named pipe is a kernel object, not a file, and almost every difference
//! from the Unix backend follows from that one fact:
//!
//! * There is nothing to unlink. The pipe exists exactly as long as a handle to
//!   it does, so a daemon that is `TerminateProcess`-ed leaves no corpse behind
//!   and [`SocketGuard`] has nothing to clean up. The whole stale-socket dance
//!   in `unix.rs` — the liveness probe, the identity check, the double bind —
//!   has no counterpart here.
//! * There is no `SIGTERM`, so [`install_signal_cleanup`] is a no-op that says
//!   so rather than a gap somebody has to rediscover.
//! * "The name is taken" arrives as `ERROR_ACCESS_DENIED`, which is the single
//!   most surprising thing in this file. See [`bind`].
//!
//! Access control is explicit: the pipe is created with a DACL naming the
//! current user's SID and nobody else, because a `NULL` descriptor would give
//! the object the token's *default* DACL — which is not the same promise, and
//! on a machine with several interactive users is not a promise at all.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ::windows::core::{Error as WinError, PCWSTR, PWSTR};
use ::windows::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE,
    ERROR_FILE_NOT_FOUND, ERROR_NO_DATA, ERROR_PATH_NOT_FOUND, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, ERROR_SEM_TIMEOUT, HANDLE, HLOCAL,
};
use ::windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use ::windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use ::windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use ::windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, SetNamedPipeHandleState,
    WaitNamedPipeW, NAMED_PIPE_MODE, NMPWAIT_NOWAIT, PIPE_NOWAIT, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use ::windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::{encode_request, tag, Accepted, Bind, Handoff, Request, IO_TIMEOUT, MAX_MESSAGE};

/// Exactly one instance ever exists. That is the single-instance guarantee
/// stated in the kernel rather than in a comment: a second daemon cannot
/// create one, and a second *client* waits its turn (see [`try_handoff_at`])
/// instead of being served by a process that is not the one holding the window.
const MAX_INSTANCES: u32 = 1;

/// `nDefaultTimeOut`, used by a `WaitNamedPipeW` that passes
/// `NMPWAIT_USE_DEFAULT_WAIT`. Both of ours pass an explicit timeout, so this
/// only bounds a third party that stumbles onto the name.
const DEFAULT_TIMEOUT_MS: u32 = 1_000;

/// How long a client waits for the one instance to come free before deciding
/// there is no usable daemon. Short on purpose: a popup that stalls is worse
/// than a popup that opens its own window.
const CONNECT_WAIT_MS: u32 = 250;

/// The poll interval the non-blocking pipe forces on us, and its ceiling. It
/// starts small so the common case (the peer has already written) costs
/// nothing measurable, and backs off so a wedged peer is not a spin loop.
const FIRST_NAP: Duration = Duration::from_micros(250);
const MAX_NAP: Duration = Duration::from_millis(4);

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

/// Where this user's daemon for this session listens. There is no file: this
/// is the pipe's name, carried in a `PathBuf` only so the signature matches
/// the Unix one and `main.rs` needs no `#[cfg]` to print it.
pub fn socket_path() -> PathBuf {
    PathBuf::from(pipe_name(&current_user(), session_id().as_deref()))
}

/// `\\.\pipe\sekio-gui-<user>-<session>`.
///
/// `user` is the current user's SID, which is what actually separates two
/// accounts: pipe names live in one machine-wide namespace, so unlike a socket
/// under `$XDG_RUNTIME_DIR` there is no directory doing that job for us. The
/// SID is used rather than `%USERNAME%` because a name is reused across domains
/// and can be renamed, and because we need the SID anyway to build the security
/// descriptor — asking for it twice would be the odd choice.
///
/// `session` is `%SESSIONNAME%` (`Console`, `RDP-Tcp#3`, …), the Windows answer
/// to `$WAYLAND_DISPLAY`: one user logged in twice must not have one login's
/// popups appear on the other's screen. It is read from the environment rather
/// than from `ProcessIdToSessionId` because that call lives behind a `windows`
/// crate feature this crate does not otherwise need, and a wrong answer here
/// costs a shared *name*, never correctness — the SID in the DACL is what keeps
/// other users out.
///
/// Both halves go through the shared `tag`, so the name stays readable and
/// bounded. A pipe name may not contain a `\` beyond the prefix and may not
/// exceed 256 characters; `tag` emits only `[a-z0-9-]` and at most 25
/// characters, which keeps both rules true by construction.
pub fn pipe_name(user: &str, session: Option<&str>) -> String {
    let user = tag(user);
    let session = match session.filter(|s| !s.is_empty()) {
        Some(session) => tag(session),
        None => "nosession".to_string(),
    };
    format!(r"\\.\pipe\sekio-gui-{user}-{session}")
}

/// Which login session we belong to.
fn session_id() -> Option<String> {
    std::env::var_os("SESSIONNAME").map(|v| v.to_string_lossy().into_owned())
}

/// The current user's SID, or something stable to fall back on.
///
/// Every failure here is survivable: a wrong identity string means this
/// daemon and its clients agree on a name that is merely less specific than it
/// should be. It must never be an error the user sees, so there is no `?` and
/// no `unwrap` on this path.
fn current_user() -> String {
    match current_user_sid() {
        Ok(sid) => sid,
        Err(_) => std::env::var_os("USERNAME")
            .map(|v| v.to_string_lossy().into_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "unknown".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

/// A `HANDLE` that closes itself.
///
/// Used for both the pipe instances and the process token: leaking either from
/// a resident process is a slow, invisible failure, which is precisely the kind
/// this module is not allowed to have.
#[derive(Debug)]
struct Handle(HANDLE);

impl Handle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a Win32 call that returned a valid handle
        // and is closed exactly once, here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// SAFETY: a `HANDLE` is a process-wide index into the kernel's object table,
// not a pointer into any thread's state. Moving one between threads is what
// every Win32 program does; the raw pointer inside the newtype is what stops
// the compiler from working that out on its own. `Send` and not `Sync`: the
// listener hands the handle to one thread at a time behind a `Mutex`.
unsafe impl Send for Handle {}

/// A security descriptor built from SDDL, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    /// `D:(A;;GA;;;<sid>)` — a DACL with exactly one ACE, granting
    /// `GENERIC_ALL` to the SID and, because a DACL that lists somebody grants
    /// nobody else anything, denying every other account. Notably that includes
    /// `SYSTEM` and the Administrators group: an admin can still take ownership
    /// of the object, but no service and no second interactive user can open
    /// this pipe and feed paths to somebody else's previewer.
    ///
    /// SDDL rather than an `ACL` assembled by hand because the hand-assembled
    /// version is thirty lines of `InitializeAcl`/`AddAccessAllowedAce` with a
    /// manual size calculation, and a mistake in it fails open.
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let text = wide(sddl);
        let mut raw = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `text` is NUL-terminated and outlives the call; `raw` is a
        // valid out-pointer. On success the descriptor is `LocalAlloc`-ed and
        // owned by us, which is what `Drop` below frees.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR::from_raw(text.as_ptr()),
                SDDL_REVISION_1,
                &mut raw,
                None,
            )
            .map_err(to_io)?;
        }
        Ok(Self(raw))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the `LocalAlloc`-ed block handed back by
        // `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// The current user's SID in string form (`S-1-5-21-…-1001`).
fn current_user_sid() -> io::Result<String> {
    // SAFETY: every pointer below is either a local out-parameter or memory
    // this function owns; each call's result is checked before the next uses
    // it, and the token handle is closed by `Handle`'s `Drop`.
    unsafe {
        let mut raw = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw).map_err(to_io)?;
        let token = Handle(raw);

        // The documented two-call shape: the first asks how big the answer is
        // and is *expected* to fail, the second reads it.
        let mut len = 0u32;
        let _ = GetTokenInformation(token.raw(), TokenUser, None, 0, &mut len);
        if len == 0 {
            return Err(io::Error::other("the process token reports no user"));
        }
        // A `Vec<u64>`, not a `Vec<u8>`: `TOKEN_USER` holds a pointer, so
        // reading it through a byte buffer's one-byte alignment is undefined
        // behaviour on paper and a fault in practice on some targets.
        let mut buf = vec![0u64; len.div_ceil(8) as usize];
        GetTokenInformation(
            token.raw(),
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
        .map_err(to_io)?;

        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).map_err(to_io)?;
        let sid = text.to_string().map_err(io::Error::other);
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        sid
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Nothing to release.
///
/// The type exists so `main.rs` can hold "the thing that owns the rendezvous
/// point" without a `#[cfg]`. On Unix that is a file the process must unlink;
/// here the pipe disappears with its last handle, so [`release`](Self::release)
/// deliberately does nothing and the name it remembers is only ever printed.
#[derive(Debug)]
pub struct SocketGuard {
    path: PathBuf,
}

impl SocketGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A named pipe has no filesystem entry to delete. Removing this would be
    /// indistinguishable from forgetting it, hence the explicit empty body.
    pub fn release(&self) {}
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// The listening end of the pipe.
///
/// A struct rather than the Unix backend's alias, because a named pipe server
/// is not a listener that spawns connections: it *is* the connection, and it
/// has to be reset between clients. It keeps the SDDL rather than a built
/// descriptor so nothing holding a raw pointer has to be made `Send`.
pub struct Listener {
    /// The pipe name, wide and NUL-terminated, ready to hand back to Win32.
    name: Vec<u16>,
    sddl: String,
    /// The instance waiting for its next client. `None` only after a failed
    /// accept closed it, in which case the next accept makes a fresh one.
    idle: Mutex<Option<Handle>>,
}

impl Listener {
    /// The instance to serve the next client on, creating one if the last
    /// accept had to throw its handle away.
    fn instance(&self) -> io::Result<Handle> {
        // A poisoned lock is a panic in another thread, not a reason to stop
        // serving: the handle behind it is still perfectly good.
        let mut idle = self.idle.lock().unwrap_or_else(|err| err.into_inner());
        match idle.take() {
            Some(pipe) => Ok(pipe),
            None => create_instance(&self.name, &self.sddl, false),
        }
    }

    fn recycle(&self, pipe: Handle) {
        let mut idle = self.idle.lock().unwrap_or_else(|err| err.into_inner());
        *idle = Some(pipe);
    }
}

/// Become *the* daemon, or discover that one already exists.
///
/// The surprise: `CreateNamedPipeW` with `FILE_FLAG_FIRST_PIPE_INSTANCE` fails
/// with **`ERROR_ACCESS_DENIED` (5)** when the name is already owned, not with
/// anything named "exists" or "in use". Read literally it looks like a
/// permissions bug, and treating it as one — reporting it to the user instead
/// of standing down — is how you get two daemons or none. That error *is* the
/// [`Bind::AlreadyRunning`] signal. (`ERROR_PIPE_BUSY` is accepted too: it is
/// what a name at its instance limit returns, which means the same thing.)
///
/// There is no stale-pipe case. A pipe is a kernel object whose lifetime ends
/// with its last handle, so a daemon that crashed took its name with it; the
/// probe-then-unlink-then-rebind sequence `unix.rs` needs has nothing to do
/// here, and its absence is not an oversight.
///
/// A SID we cannot read is a hard error rather than a fall back to a weaker
/// descriptor: without it there is no way to name the one account the pipe is
/// for, and a daemon that serves everybody is worse than no daemon at all —
/// which, since the name then never exists, is exactly what clients see.
pub fn bind(path: &Path) -> io::Result<Bind> {
    let name = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the pipe name is not UTF-8"))?;
    let sddl = format!("D:(A;;GA;;;{})", current_user_sid()?);
    let name = wide(name);
    match create_instance(&name, &sddl, true) {
        Ok(pipe) => {
            let listener = Listener {
                name,
                sddl,
                idle: Mutex::new(Some(pipe)),
            };
            let guard = SocketGuard {
                path: path.to_path_buf(),
            };
            Ok(Bind::Bound(listener, guard))
        }
        Err(err) if is_taken(&err) => Ok(Bind::AlreadyRunning),
        Err(err) => Err(err),
    }
}

fn is_taken(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(code)
            if code == ERROR_ACCESS_DENIED.0 as i32 || code == ERROR_PIPE_BUSY.0 as i32
    )
}

/// Create one instance of the pipe.
///
/// `first` sets `FILE_FLAG_FIRST_PIPE_INSTANCE`, which is what makes the race
/// between two starting daemons resolvable at all: without it the second
/// process would happily create a *second* instance of the same name and both
/// would believe they were the daemon, with clients dealt to whichever
/// answered first.
fn create_instance(name: &[u16], sddl: &str, first: bool) -> io::Result<Handle> {
    let descriptor = Descriptor::from_sddl(sddl)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0 .0,
        bInheritHandle: false.into(),
    };
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // `PIPE_WAIT` here and only here: an idle daemon must park in
    // `ConnectNamedPipe` at zero cost. The mode is flipped for the
    // conversation itself; `accept_one` explains why.
    let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;

    // SAFETY: `name` is NUL-terminated, `attributes` borrows a descriptor that
    // outlives the call, and the returned handle is checked before use.
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR::from_raw(name.as_ptr()),
            open_mode,
            mode,
            MAX_INSTANCES,
            MAX_MESSAGE as u32,
            MAX_MESSAGE as u32,
            DEFAULT_TIMEOUT_MS,
            Some(&attributes),
        )
    };
    if handle.is_invalid() {
        return Err(last_error());
    }
    Ok(Handle(handle))
}

/// Accept exactly one connection and read what it carries. Only a failure of
/// the pipe itself is an `Err`.
///
/// The instance is reset and reused rather than closed and recreated. That is
/// not just cheaper: closing the last handle destroys the *name*, and a client
/// that called `CreateFileW` during that window would be told the pipe does
/// not exist and would conclude there is no daemon at all. Reconnecting the
/// same instance leaves no such gap.
pub fn accept_one(listener: &Listener) -> io::Result<Accepted> {
    let pipe = listener.instance()?;
    set_mode(&pipe, PIPE_READMODE_BYTE | PIPE_WAIT)?;
    connect(&pipe)?;
    // Non-blocking from here on. Named pipes have no equivalent of a socket's
    // `SO_RCVTIMEO`, so this is what a deadline is actually made of: in
    // `PIPE_NOWAIT` mode a read with nothing buffered returns `ERROR_NO_DATA`
    // immediately, and `Conversation` turns that into a short sleep and a
    // retry until `IO_TIMEOUT` runs out. The honest cost is a poll loop
    // instead of a blocking wait for the duration of one request — microseconds
    // in the normal case, and a bounded hundred-odd wakeups against a peer that
    // connects and then says nothing. The alternative, staying in `PIPE_WAIT`,
    // is a daemon that any client can freeze by connecting and going quiet.
    let _ = set_mode(&pipe, PIPE_READMODE_BYTE | PIPE_NOWAIT);

    let mut stream = Conversation::new(&pipe);
    let accepted = super::answer(&mut stream);
    // `DisconnectNamedPipe` discards whatever the client has not read yet, so
    // hanging up first would throw away the `ok\n` we just wrote in exactly the
    // case that matters. Waiting for the client to close instead is bounded by
    // the same deadline; `FlushFileBuffers`, the obvious alternative, blocks
    // until the peer drains the buffer and has no timeout at all.
    stream.wait_for_hangup();

    // SAFETY: `pipe` is a connected server instance we own.
    let _ = unsafe { DisconnectNamedPipe(pipe.raw()) };
    listener.recycle(pipe);
    Ok(accepted)
}

/// Wait for a client.
///
/// `ERROR_PIPE_CONNECTED` is a success: it means the client got in between
/// `CreateNamedPipeW` and this call, so there is already a connection to serve.
/// Treating it as an error is the classic named-pipe bug — it turns the
/// fastest clients, the ones that were waiting on `WaitNamedPipeW` and
/// connected the instant the instance came free, into the ones that get
/// dropped.
fn connect(pipe: &Handle) -> io::Result<()> {
    // SAFETY: `pipe` is a server instance we own and no overlapped structure
    // is involved.
    match unsafe { ConnectNamedPipe(pipe.raw(), None) } {
        Ok(()) => Ok(()),
        Err(err) if code_of(&err) == ERROR_PIPE_CONNECTED.0 => Ok(()),
        Err(err) => Err(to_io(err)),
    }
}

fn set_mode(pipe: &Handle, mode: NAMED_PIPE_MODE) -> io::Result<()> {
    // SAFETY: `pipe` is a pipe handle we own and `mode` is a local.
    unsafe { SetNamedPipeHandleState(pipe.raw(), Some(&mode), None, None) }.map_err(to_io)
}

/// There is no `SIGTERM` on Windows and no filesystem entry to leave behind, so
/// there is nothing for a cleanup handler to clean up: the pipe dies with the
/// process that owns it whether that process exits, crashes or is killed from
/// Task Manager. Returning `Ok(())` is the whole correct implementation.
pub fn install_signal_cleanup(_guard: &SocketGuard) -> io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Is a daemon answering on `socket` right now?
///
/// `WaitNamedPipeW` rather than a connect-and-close: it answers the question
/// without occupying the one instance and without waking the daemon at all,
/// which is what makes it safe for `--doctor` to call. `ERROR_SEM_TIMEOUT` (and
/// `ERROR_PIPE_BUSY`) mean the name exists but every instance is serving
/// somebody — still a running daemon.
pub fn is_running(socket: &Path) -> bool {
    let Some(name) = socket.to_str() else {
        return false;
    };
    let name = wide(name);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    if unsafe { WaitNamedPipeW(PCWSTR::from_raw(name.as_ptr()), NMPWAIT_NOWAIT) }.as_bool() {
        return true;
    }
    // SAFETY: reading the calling thread's last-error value.
    let code = unsafe { GetLastError() }.0;
    code == ERROR_SEM_TIMEOUT.0 || code == ERROR_PIPE_BUSY.0
}

/// [`super::try_handoff`] against an explicit pipe name, so tests need no
/// environment.
pub fn try_handoff_at(socket: &Path, request: &Request) -> Handoff {
    let message = match encode_request(request) {
        Ok(message) => message,
        Err(err) => return Handoff::Unavailable(format!("cannot hand off this request: {err}")),
    };
    let Some(name) = socket.to_str() else {
        return Handoff::Unavailable("the pipe name is not UTF-8".to_owned());
    };
    let name = wide(name);
    let pipe = match open(&name) {
        Ok(pipe) => pipe,
        Err(reason) => return Handoff::Unavailable(reason),
    };
    // Same deadline as the server, for the same reason: a daemon wedged
    // mid-preview must cost this process a moment, not a popup. If the mode
    // switch fails the writes below still complete — the pipe is empty and our
    // message fits its buffer — so this is best-effort by design.
    let _ = set_mode(&pipe, PIPE_READMODE_BYTE | PIPE_NOWAIT);
    let mut stream = Conversation::new(&pipe);
    super::deliver(&mut stream, &message)
}

/// Open the client end.
///
/// `ERROR_FILE_NOT_FOUND` is the ordinary "no daemon" answer — the name only
/// exists while somebody is listening. `ERROR_PIPE_BUSY` is the interesting
/// one: a daemon *is* there but its single instance is mid-request, so wait
/// briefly for it to come free and try once more. Once, and briefly: falling
/// back to a local window costs a few milliseconds of cold start, while
/// blocking here costs the user a popup that never appears.
fn open(name: &[u16]) -> Result<Handle, String> {
    match create_file(name) {
        Ok(pipe) => Ok(pipe),
        Err(err) if is_absent(&err) => Err("no daemon is listening".to_owned()),
        Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
            // SAFETY: `name` is NUL-terminated and outlives the call.
            let free = unsafe { WaitNamedPipeW(PCWSTR::from_raw(name.as_ptr()), CONNECT_WAIT_MS) };
            if !free.as_bool() {
                return Err("the daemon is busy".to_owned());
            }
            create_file(name).map_err(|err| err.to_string())
        }
        Err(err) => Err(err.to_string()),
    }
}

fn is_absent(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(code)
            if code == ERROR_FILE_NOT_FOUND.0 as i32 || code == ERROR_PATH_NOT_FOUND.0 as i32
    )
}

fn create_file(name: &[u16]) -> io::Result<Handle> {
    // SAFETY: `name` is NUL-terminated and outlives the call; the handle is
    // wrapped for closing the moment it is known good.
    unsafe {
        CreateFileW(
            PCWSTR::from_raw(name.as_ptr()),
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map(Handle)
    .map_err(to_io)
}

// ---------------------------------------------------------------------------
// Reading and writing
// ---------------------------------------------------------------------------

/// One request/response exchange over a non-blocking pipe, with a deadline.
///
/// This is what lets the shared `answer`/`deliver`/`read_request` code work
/// unchanged on both platforms: everything above the transport talks to a
/// `Read + Write`, and the polling that a `PIPE_NOWAIT` handle demands is
/// hidden here rather than smeared through the protocol code.
struct Conversation<'a> {
    pipe: &'a Handle,
    deadline: Instant,
    backoff: Duration,
}

impl<'a> Conversation<'a> {
    fn new(pipe: &'a Handle) -> Self {
        Self {
            pipe,
            deadline: Instant::now() + IO_TIMEOUT,
            backoff: FIRST_NAP,
        }
    }

    /// Sleep a little, or give up because the deadline has passed. This is the
    /// only place a peer that goes silent can cost us time, and it is bounded.
    fn pause(&mut self) -> io::Result<()> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the peer stopped talking",
            ));
        }
        std::thread::sleep(self.backoff);
        self.backoff = (self.backoff * 2).min(MAX_NAP);
        Ok(())
    }

    /// Read until the client closes its end, so the acknowledgement we just
    /// wrote is safely out of the pipe before it is disconnected. Bounded by
    /// the same deadline, and a peer that will not hang up is simply dropped.
    fn wait_for_hangup(&mut self) {
        let mut scratch = [0u8; 32];
        while let Ok(1..) = self.read(&mut scratch) {}
    }
}

impl Read for Conversation<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let mut read = 0u32;
            // SAFETY: `buf` is a valid slice for the duration of the call and
            // `read` is a local out-parameter.
            let outcome =
                unsafe { ReadFile(self.pipe.raw(), Some(&mut *buf), Some(&mut read), None) };
            match outcome {
                // Nothing buffered yet: on a `PIPE_NOWAIT` handle that is what
                // "would block" looks like, reported either way round.
                Ok(()) if read == 0 => self.pause()?,
                Ok(()) => {
                    self.backoff = FIRST_NAP;
                    return Ok(read as usize);
                }
                Err(err) if code_of(&err) == ERROR_NO_DATA.0 => self.pause()?,
                // The peer hung up. That is end of file, not a failure — a
                // client that connects and closes is the liveness probe.
                Err(err)
                    if code_of(&err) == ERROR_BROKEN_PIPE.0
                        || code_of(&err) == ERROR_PIPE_NOT_CONNECTED.0 =>
                {
                    return Ok(0)
                }
                Err(err) => return Err(to_io(err)),
            }
        }
    }
}

impl Write for Conversation<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let mut written = 0u32;
            // SAFETY: `buf` is a valid slice for the duration of the call and
            // `written` is a local out-parameter.
            let outcome =
                unsafe { WriteFile(self.pipe.raw(), Some(buf), Some(&mut written), None) };
            match outcome {
                // The pipe's buffer is full and the peer has not drained it.
                Ok(()) if written == 0 => self.pause()?,
                Ok(()) => {
                    self.backoff = FIRST_NAP;
                    return Ok(written as usize);
                }
                Err(err) => return Err(to_io(err)),
            }
        }
    }

    /// Nothing to do, and deliberately not `FlushFileBuffers`: that waits for
    /// the peer to read everything, with no timeout, which is the one thing
    /// this module must never do. Bytes handed to `WriteFile` are already in
    /// the pipe.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Win32 odds and ends
// ---------------------------------------------------------------------------

/// UTF-16, NUL-terminated, for the `W` half of the API.
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The `GetLastError` value behind a `windows` crate error, which wraps it as
/// an HRESULT in the `FACILITY_WIN32` range (`0x8007_xxxx`).
fn code_of(err: &WinError) -> u32 {
    let hresult = err.code().0 as u32;
    if hresult & 0xffff_0000 == 0x8007_0000 {
        hresult & 0xffff
    } else {
        hresult
    }
}

fn to_io(err: WinError) -> io::Error {
    io::Error::from_raw_os_error(code_of(&err) as i32)
}

/// For the calls that report failure by returning an invalid handle rather
/// than a `Result`.
fn last_error() -> io::Error {
    // SAFETY: reading the calling thread's last-error value.
    io::Error::from_raw_os_error(unsafe { GetLastError() }.0 as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A SID-shaped string, so the tests read like the thing they describe.
    const ALICE: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    const BOB: &str = "S-1-5-21-1111111111-2222222222-3333333333-1002";

    #[test]
    fn pipe_name_separates_users_and_sessions() {
        let a = pipe_name(ALICE, Some("Console"));
        assert_eq!(a, pipe_name(ALICE, Some("Console")), "must be stable");
        assert_ne!(a, pipe_name(BOB, Some("Console")), "two users, two pipes");
        assert_ne!(a, pipe_name(ALICE, Some("RDP-Tcp#0")), "two logins too");
        assert_ne!(a, pipe_name(ALICE, None));
        // Two SIDs differing only past the 16th character — the truncation
        // `sanitize` applies — must still land on different pipes.
        assert_ne!(pipe_name(ALICE, None), pipe_name(BOB, None));
    }

    /// The name is a *name*, not a path: the only backslashes allowed are the
    /// ones in the `\\.\pipe\` prefix, and 256 characters is the ceiling.
    #[test]
    fn pipe_name_is_a_legal_pipe_name() {
        const PREFIX: &str = r"\\.\pipe\";
        for name in [
            pipe_name(ALICE, Some("Console")),
            pipe_name(&"x".repeat(4000), Some(&"y".repeat(4000))),
            pipe_name("", None),
        ] {
            let rest = name
                .strip_prefix(PREFIX)
                .unwrap_or_else(|| panic!("{name} must start with {PREFIX}"));
            assert!(!rest.contains('\\'), "a pipe name has one prefix: {name}");
            assert!(!rest.is_empty(), "{name} names nothing");
            assert!(name.len() <= 256, "{} characters is too long", name.len());
        }
    }
}
