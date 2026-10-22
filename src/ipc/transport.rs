//! Named pipe transport for IPC between `flow` CLI and `flowd` daemon.
//!
//! Uses the Windows named pipe `\\.\pipe\flow`. Both ends open it for
//! overlapped I/O so every read and write can carry a deadline: the client
//! bounds its round trip by `IPC_TIMEOUT`, the daemon bounds a connected
//! client's silence by [`SERVER_READ_TIMEOUT`] (a client that connects and
//! never writes used to wedge the daemon's main loop in `ReadFile`).
//! Messages are newline-delimited JSON (see [`super::message`]).

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::IO::{CancelIo, CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    WaitNamedPipeW,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcessToken, ResetEvent, SetEvent, WaitForSingleObject,
};
use windows::core::{HRESULT, PWSTR};

use super::message::{self, SocketMessage, SocketResponse};

/// Read buffer size for pipe I/O (8 KiB).
const BUF_SIZE: u32 = 8192;

/// Longest line either side accepts before giving up on the peer: a reply
/// is at most tens of KiB (`flow query all`), so more without a newline is
/// not the protocol, and the read fails with `InvalidData`.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Longest slice of a bad line the daemon puts in its log.
const LOG_LINE_PREVIEW: usize = 200;

/// Maximum wall-clock time a single client-side IPC round trip may take.
///
/// The CLI's transport is a request/response pair: write a message, then read
/// the daemon's reply. With a *synchronous* (blocking) pipe read there is no
/// escape hatch if the daemon accepts the connection but never replies — the
/// caller blocks forever. This is exactly what produced the integration-test
/// hang: a test panicking mid-cleanup destroys its windows, the daemon's main
/// thread stalls recomputing layout (including `SetWindowPos`, which is
/// unreliable on isolated test desktops), and so it never reads the `Stop`
/// command the dropping `DaemonGuard` sends.
///
/// The client pipe is therefore opened for **overlapped** I/O and every
/// read/write is bounded by this deadline (see [`read_line_overlapped`] and
/// [`write_all_overlapped`]). Thirty seconds is deliberately generous — a
/// healthy daemon answers in milliseconds, so this ceiling only ever trips when
/// the daemon is genuinely stuck, at which point failing fast beats hanging.
const IPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest the daemon waits for a connected client to send its command. A
/// `flow` invocation writes its line the instant it connects; a client that
/// is still silent after this is disconnected so the next one can be served
/// (without it the main loop sat in a `ReadFile` with no deadline).
pub const SERVER_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Longest the daemon waits for a client to drain a response.
const SERVER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the accept thread re-checks `stopping` while a connect is
/// pending (its cancel is also issued directly, this is the backstop).
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// Longest a client keeps retrying a connect that fails with
/// `ERROR_PIPE_BUSY`: the daemon serves one client at a time, so a second
/// `flow` right behind another lands between its disconnect and the next
/// accept (see `docs/src/dev-guide/ipc-and-watchdog.md`, *Overlapped I/O
/// and deadlines*).
const BUSY_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Win32 `ERROR_IO_PENDING` (997 / `0x3E5`) expressed as the `HRESULT`
/// windows-rs returns from an overlapped `ReadFile`/`WriteFile` that has not
/// yet completed. It is the normal "operation in flight" result, not an error.
const ERROR_IO_PENDING_HRESULT: HRESULT = HRESULT(0x8007_03E5_u32 as i32);

/// Wrapper around a Windows `HANDLE` that is closed on drop.
///
/// All pipe handles (both server and client) must be wrapped in this type
/// to prevent kernel handle leaks on early returns.
#[derive(Debug)]
struct PipeHandle(HANDLE);

impl PipeHandle {
    /// Returns the raw handle value.
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// Safety: HANDLE is a raw pointer-like value. We manage its lifetime via Drop.
unsafe impl Send for PipeHandle {}

/// RAII wrapper around a Win32 Event handle used for I/O signaling.
///
/// Created via [`CreateEventW`] (manual-reset mode, initially unset). The
/// handle is closed automatically when this wrapper is dropped, preventing
/// kernel handle leaks.
///
/// # Design
///
/// In the daemon's event-driven architecture, an [`EventHandle`] bridges
/// between a background thread (blocked in `ConnectNamedPipe`) and the main
/// thread (blocked in `WaitForMultipleObjects`). When the accept thread
/// detects a client connection it calls [`SetEvent`], waking the main thread
/// so it can proceed with IPC processing.
#[derive(Debug)]
struct EventHandle(HANDLE);

impl EventHandle {
    /// Creates a new manual-reset event that is initially unset.
    ///
    /// A manual-reset event stays signaled until explicitly reset, which is
    /// the correct semantic here: once a client connects, the event remains
    /// signaled until the main thread processes the connection and calls
    /// `start_accept` for the next one.
    ///
    /// # Errors
    ///
    /// Returns an error if [`CreateEventW`] fails or returns an invalid handle.
    fn new() -> io::Result<Self> {
        let handle = unsafe { CreateEventW(None, true, false, windows::core::PCWSTR::null()) }
            .map_err(|e| io::Error::other(format!("CreateEventW failed: {e}")))?;

        if handle.is_invalid() {
            return Err(io::Error::other("CreateEventW returned invalid handle"));
        }

        Ok(Self(handle))
    }

    /// Returns the raw [`HANDLE`] for use with `WaitForMultipleObjects`.
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// SAFETY: HANDLE is a process-wide kernel object identifier. Sending it
// across threads is safe — the kernel synchronizes access to the event object.
unsafe impl Send for EventHandle {}

/// Named pipe server for the `flowd` daemon.
///
/// Listens on `\\.\pipe\flow`, accepts one client at a time (sequential),
/// reads newline-delimited JSON messages, and writes responses.
///
/// # Event-Driven Architecture
///
/// Instead of blocking the main thread in `ConnectNamedPipe`, the server
/// uses a background accept thread. The flow is:
///
/// 1. Main thread calls [`start_accept()`] to spawn a one-shot thread that
///    blocks in `ConnectNamedPipe`.
/// 2. When a client connects, the accept thread calls `SetEvent` on the
///    `connected_event`, waking the main thread's `WaitForMultipleObjects`.
/// 3. The main thread processes IPC messages via [`read_message()`] /
///    [`write_response()`], then calls [`disconnect()`] and [`start_accept()`]
///    to accept the next client.
///
/// This decouples pipe-accept blocking from the main loop, allowing hook
/// events (window creation/removal) to be processed immediately even when no
/// CLI client is connected.
pub struct PipeServer {
    /// The server pipe instance. `None` only after [`shutdown`](Self::shutdown)
    /// had to give up on a wedged accept thread and leaked the handle on
    /// purpose (closing it would block — see `shutdown`).
    handle: Option<PipeHandle>,
    /// Manual-reset event signaled when a client connects to the pipe.
    /// The main thread waits on this via `WaitForMultipleObjects`.
    connected_event: EventHandle,
    /// Background thread blocked in `ConnectNamedPipe`.
    /// `None` when no accept is pending.
    accept_thread: Option<std::thread::JoinHandle<()>>,
    /// Set by [`shutdown`](Self::shutdown); the accept thread checks it after
    /// every `ConnectNamedPipe` return and exits instead of retrying.
    stopping: Arc<AtomicBool>,
    /// The pipe path this server listens on (for the throwaway self-connect
    /// in `shutdown`).
    pipe_name: String,
}

/// Longest [`PipeServer::shutdown`] waits for the accept thread to exit.
pub(crate) const ACCEPT_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(1_000);

impl PipeServer {
    /// Create a new named pipe server instance.
    ///
    /// This creates the pipe and the `connected_event` Win32 event object.
    /// The pipe is ready to accept connections — call [`Self::start_accept`]
    /// to begin listening on a background thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the pipe cannot be created (e.g., another daemon is
    /// already running) or if the event object cannot be created.
    pub fn create() -> io::Result<Self> {
        Self::create_named(&message::pipe_name())
    }

    /// Create a server on an explicit pipe path (see [`create`](Self::create)).
    /// Lets tests use an isolated name without touching `FLOW_PIPE_NAME`.
    ///
    /// # Errors
    ///
    /// As [`create`](Self::create).
    pub fn create_named(pipe_name: &str) -> io::Result<Self> {
        let name = wide(pipe_name);

        // DACL from `PipeSecurity` (daemon's user, SYSTEM, Administrators
        // only), remote clients rejected, overlapped so every server I/O
        // can be bounded (`read_message`) and cancelled (`shutdown`).
        let security = PipeSecurity::for_current_user()?;
        let attrs = security.attributes();
        // SAFETY: `name` is NUL-terminated; `attrs` points at a descriptor
        // that `security` keeps alive for the duration of the call.
        let handle = unsafe {
            CreateNamedPipeW(
                windows::core::PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUF_SIZE,
                BUF_SIZE,
                0,
                Some(&attrs),
            )
        };

        if handle.is_invalid() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "pipe already in use (is another daemon running?)",
            ));
        }

        let connected_event = EventHandle::new()?;

        Ok(Self {
            handle: Some(PipeHandle(handle)),
            connected_event,
            accept_thread: None,
            stopping: Arc::new(AtomicBool::new(false)),
            pipe_name: pipe_name.to_owned(),
        })
    }

    /// The raw server pipe handle (invalid after a leaked shutdown).
    fn pipe(&self) -> HANDLE {
        self.handle
            .as_ref()
            .map_or(HANDLE::default(), PipeHandle::raw)
    }

    /// `true` while an accept thread is pending.
    #[must_use]
    pub fn accept_pending(&self) -> bool {
        self.accept_thread
            .as_ref()
            .is_some_and(|t| !t.is_finished())
    }

    /// Stop accepting: cancel a pending `ConnectNamedPipe` and join the
    /// accept thread, bounded by `ACCEPT_SHUTDOWN_TIMEOUT`.
    ///
    /// `CloseHandle` on a pipe with a pending `ConnectNamedPipe` blocks until
    /// the connect completes, which used to hang `flowd` on every exit path
    /// where no client had just connected. Sets `stopping`, re-issues
    /// `CancelIoEx` until the thread leaves, then connects a throwaway client
    /// to its own pipe as the fallback. Returns `true` if the thread was
    /// joined in time; if not, the pipe handle is deliberately **leaked** so
    /// dropping the server can no longer block. See
    /// `docs/src/dev-guide/ipc-and-watchdog.md`, *Fail-back*.
    pub fn shutdown(&mut self) -> bool {
        self.stopping.store(true, Ordering::Release);
        let Some(thread) = self.accept_thread.take() else {
            return true;
        };
        if thread.is_finished() {
            let _ = thread.join();
            return true;
        }
        let pipe = self.pipe();
        let start = Instant::now();
        let deadline = start + ACCEPT_SHUTDOWN_TIMEOUT;
        let mut self_connected = false;
        while !thread.is_finished() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            // SAFETY: cancelling every pending I/O on our own pipe handle;
            // the accept thread reaps the cancelled connect with
            // GetOverlappedResult before touching its OVERLAPPED again.
            let _ = unsafe { CancelIoEx(pipe, None) };
            if !self_connected && now - start >= ACCEPT_SHUTDOWN_TIMEOUT / 3 {
                // Fallback: a real connection makes ConnectNamedPipe return.
                self_connected = true;
                let _ = connect_to_named_pipe(&self.pipe_name);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if thread.is_finished() {
            let _ = thread.join();
            true
        } else {
            log::error!(
                "PipeServer: accept thread did not exit within {:?}; leaking the pipe handle so shutdown cannot block",
                ACCEPT_SHUTDOWN_TIMEOUT
            );
            if let Some(h) = self.handle.take() {
                std::mem::forget(h);
            }
            // The JoinHandle is dropped here, detaching the thread.
            false
        }
    }

    /// Start accepting the next client connection on a background thread.
    ///
    /// Spawns a short-lived thread that issues an overlapped `ConnectNamedPipe`
    /// and waits for it (polling `stopping` so [`shutdown`](Self::shutdown)
    /// can always free it). When a client connects, the thread signals
    /// `connected_event` via
    /// `SetEvent` and exits. The main thread should use
    /// [`connected_event_handle`] to obtain the event handle for
    /// `WaitForMultipleObjects`.
    ///
    /// Call this method:
    /// - Once at startup (before entering the main loop)
    /// - After each client disconnects (to accept the next client)
    pub fn start_accept(&mut self) {
        // Join the previous accept thread if it exists.
        // The accept thread is one-shot: it exits after ConnectNamedPipe
        // returns and SetEvent is called. By the time we get here, the
        // thread has already finished, so join() returns immediately.
        if let Some(handle) = self.accept_thread.take()
            && let Err(e) = handle.join()
        {
            log::warn!("PipeServer: previous accept thread panicked: {e:?}");
        }

        // Reset the connected event before spawning a new accept thread.
        let _ = unsafe { ResetEvent(self.connected_event.raw()) };

        if self.stopping.load(Ordering::Acquire) {
            // Shutting down: never start a new accept.
            return;
        }

        let handle = self.pipe().0 as isize;
        let event = self.connected_event.raw().0 as isize;
        let stopping = Arc::clone(&self.stopping);

        self.accept_thread = Some(std::thread::spawn(move || {
            // Block until a client connects. This runs on a background thread
            // so the main thread is free to process hook events.
            //
            // SAFETY: HANDLE values are process-wide kernel object identifiers.
            // We pass them as isize to satisfy Send requirements (HANDLE itself
            // is !Send in windows-rs), then reconstruct inside the closure.
            let pipe = HANDLE(handle as *mut core::ffi::c_void);
            let event = HANDLE(event as *mut core::ffi::c_void);

            // ERROR_PIPE_CONNECTED (Win32 535 / 0x217): a client connected
            // before ConnectNamedPipe was called — a success case.
            const ERROR_PIPE_CONNECTED_HRESULT: windows::core::HRESULT =
                windows::core::HRESULT(0x8007_0217_u32 as i32);

            let io_event = match EventHandle::new() {
                Ok(e) => e,
                Err(e) => {
                    log::error!("PipeServer accept thread: cannot create its event: {e}");
                    return;
                }
            };

            // Retry loop. ConnectNamedPipe can fail transiently with
            // ERROR_NO_DATA ("The pipe is being closed") when the CLI's
            // `is_daemon_running` health-check connects + disconnects in the
            // window between pipe creation and this first ConnectNamedPipe,
            // leaving the instance in a "closing" state. The old code bailed
            // out here without signaling, so the connect event was never set
            // and IPC was permanently dead (debug.log showed exactly this).
            // Reset the instance with DisconnectNamedPipe and retry until a
            // real client connects. The loop also exits when the process
            // terminates (the handle close makes ConnectNamedPipe return an
            // error and the thread is killed on exit), so it cannot spin
            // forever in practice.
            loop {
                let _ = unsafe { ResetEvent(io_event.raw()) };
                let mut overlapped = OVERLAPPED {
                    hEvent: io_event.raw(),
                    ..Default::default()
                };
                // SAFETY: the pipe is an overlapped handle and `overlapped`
                // outlives the operation: every path below reaps a pending
                // connect with GetOverlappedResult(bWait = true) before the
                // struct goes out of scope, including after a cancel.
                let started = unsafe { ConnectNamedPipe(pipe, Some(&mut overlapped)) };
                let result = match started {
                    Ok(()) => Ok(()),
                    Err(e) if e.code() == ERROR_PIPE_CONNECTED_HRESULT => Ok(()),
                    Err(e) if e.code() == ERROR_IO_PENDING_HRESULT => {
                        // Wait for a client, re-checking `stopping` so a
                        // shutdown never depends on a cancel landing.
                        loop {
                            if stopping.load(Ordering::Acquire) {
                                // SAFETY: cancels this thread's pending
                                // connect on the pipe; reaped just below.
                                let _ = unsafe { CancelIoEx(pipe, Some(&overlapped)) };
                                break;
                            }
                            let ms = u32::try_from(ACCEPT_POLL.as_millis()).unwrap_or(50);
                            // SAFETY: waiting on our own event handle.
                            if unsafe { WaitForSingleObject(io_event.raw(), ms) } == WAIT_OBJECT_0 {
                                break;
                            }
                        }
                        let mut transferred = 0u32;
                        // SAFETY: `overlapped` started an I/O on `pipe`;
                        // bWait = true blocks until it has completed or its
                        // cancellation has landed, so the struct is no longer
                        // referenced by the kernel afterwards.
                        unsafe { GetOverlappedResult(pipe, &overlapped, &mut transferred, true) }
                    }
                    Err(e) => Err(e),
                };
                // Shutdown: whether the connect was cancelled, failed or
                // even succeeded (throwaway self-connect), leave without
                // signalling — the main loop is no longer serving clients.
                if stopping.load(Ordering::Acquire) {
                    log::debug!("PipeServer accept thread: shutdown requested, exiting");
                    return;
                }
                match result {
                    Ok(()) => break,
                    Err(e) if e.code() == ERROR_PIPE_CONNECTED_HRESULT => break,
                    Err(e) => {
                        log::warn!(
                            "PipeServer accept thread: ConnectNamedPipe failed ({e:#?}); \
                             resetting pipe instance and retrying"
                        );
                        // SAFETY: best-effort reset of the server-side instance
                        // so the next ConnectNamedPipe can wait for a fresh
                        // client. Errors (e.g. on a closing handle) are ignored.
                        let _ = unsafe { DisconnectNamedPipe(pipe) };
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            }

            // Signal the main thread that a client is connected.
            let _ = unsafe { SetEvent(event) };
        }));
    }

    /// Returns the event handle that is signaled when a client connects.
    ///
    /// Used by the main thread with `WaitForMultipleObjects` to wait for
    /// either hook events or pipe connections simultaneously.
    pub fn connected_event_handle(&self) -> HANDLE {
        self.connected_event.raw()
    }

    /// The pipe path this server listens on.
    #[must_use]
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    /// Read a single newline-terminated JSON message from the client.
    ///
    /// Blocks until a full line (ending in `\n`) is received, at most
    /// [`SERVER_READ_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// Returns an error on I/O failure or if the connection is closed
    /// without sending a message. A client that stays silent for
    /// [`SERVER_READ_TIMEOUT`] fails with [`io::ErrorKind::TimedOut`]; the
    /// daemon disconnects it. A line that arrives intact but does not
    /// decode as a [`SocketMessage`] fails with
    /// [`io::ErrorKind::InvalidData`]; the daemon answers that one with
    /// [`unrecognised_message_response`] before disconnecting, so a newer
    /// `flow` CLI talking to an older daemon gets a reply instead of a
    /// closed pipe.
    pub fn read_message(&self) -> io::Result<SocketMessage> {
        let line = read_line_overlapped(self.pipe(), Instant::now() + SERVER_READ_TIMEOUT)?;
        message::decode_message(&line).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "failed to parse message: {:?}",
                    truncate_for_log(&line, LOG_LINE_PREVIEW)
                ),
            )
        })
    }

    /// Write a [`SocketResponse`] to the client as newline-delimited JSON,
    /// bounded by `SERVER_WRITE_TIMEOUT`.
    ///
    /// # Errors
    ///
    /// Returns an error if serialisation or the write fails, or the client
    /// does not drain the response in time (`TimedOut`).
    pub fn write_response(&self, response: &SocketResponse) -> io::Result<()> {
        let wire = message::encode_message(response)?;
        write_all_overlapped(
            self.pipe(),
            wire.as_bytes(),
            Instant::now() + SERVER_WRITE_TIMEOUT,
        )
    }

    /// Disconnect the current client so a new one can connect.
    ///
    /// # Errors
    ///
    /// Returns an error if the disconnect fails.
    pub fn disconnect(&self) -> io::Result<()> {
        unsafe { DisconnectNamedPipe(self.pipe()) }
            .map_err(|e| io::Error::other(format!("DisconnectNamedPipe: {e}")))
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        // Never let a drop block on a pending synchronous accept (see
        // `shutdown`). Idempotent: a server already shut down returns at once.
        if self.accept_pending() {
            self.shutdown();
        }
    }
}

// ── Pipe security ───────────────────────────────────────────────────

/// The SDDL string for the server pipe's security descriptor: a protected
/// DACL (`D:P`) that allows SYSTEM and Builtin Administrators generic all
/// and `user_sid` — the daemon's own user, read from the process token —
/// generic read + write, and nobody else anything. Why the SID is resolved
/// at runtime rather than written as `CO` / `OW` is in
/// `docs/src/dev-guide/ipc-and-watchdog.md`, *Pipe security*.
#[must_use]
pub fn pipe_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{user_sid})")
}

/// The string SID (`S-1-5-21-…`) of the user this process runs as, read
/// from the process token.
fn current_user_sid() -> io::Result<String> {
    let mut token = HANDLE::default();
    // SAFETY: querying our own process token; `token` receives the handle.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| io::Error::other(format!("OpenProcessToken: {e}")))?;
    let token = PipeHandle(token);

    let mut needed = 0u32;
    // SAFETY: size query with a null buffer; failure with
    // ERROR_INSUFFICIENT_BUFFER is the documented first step.
    let _ = unsafe { GetTokenInformation(token.raw(), TokenUser, None, 0, &mut needed) };
    if needed == 0 {
        return Err(io::Error::other("GetTokenInformation(TokenUser): no size"));
    }
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` is `needed` bytes, as the first call asked for.
    unsafe {
        GetTokenInformation(
            token.raw(),
            TokenUser,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            needed,
            &mut needed,
        )
    }
    .map_err(|e| io::Error::other(format!("GetTokenInformation(TokenUser): {e}")))?;
    // SAFETY: the buffer holds a TOKEN_USER written by the call above; the
    // SID it points to lives inside `buf`.
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    let mut sid_str = PWSTR::null();
    // SAFETY: valid SID pointer; `sid_str` receives a LocalAlloc'd string.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_str) }
        .map_err(|e| io::Error::other(format!("ConvertSidToStringSidW: {e}")))?;
    // SAFETY: `sid_str` is a NUL-terminated string from the call above.
    let s = unsafe { sid_str.to_string() }.map_err(|e| io::Error::other(e.to_string()));
    // SAFETY: freeing the buffer ConvertSidToStringSidW allocated for us.
    let _ = unsafe { LocalFree(Some(HLOCAL(sid_str.as_ptr() as *mut core::ffi::c_void))) };
    s
}

/// A self-relative security descriptor built from [`pipe_sddl`], owned
/// (freed with `LocalFree` on drop) and lent to `CreateNamedPipeW` through
/// [`attributes`](Self::attributes).
struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
}

impl PipeSecurity {
    /// Build the descriptor for the current user's SID.
    fn for_current_user() -> io::Result<Self> {
        Self::from_sddl(&pipe_sddl(&current_user_sid()?))
    }

    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let sddl_w = wide(sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl_w` is NUL-terminated; `descriptor` receives a
        // LocalAlloc'd self-relative descriptor that `Drop` frees.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                windows::core::PCWSTR(sddl_w.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|e| io::Error::other(format!("security descriptor from {sddl:?}: {e}")))?;
        Ok(Self { descriptor })
    }

    /// The `SECURITY_ATTRIBUTES` to pass to `CreateNamedPipeW`; valid while
    /// `self` lives.
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated with LocalAlloc by
        // ConvertStringSecurityDescriptorToSecurityDescriptorW.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.descriptor.0))) };
    }
}

/// The first `max` bytes of `s` (cut on a char boundary) with `…` and the
/// omitted length appended — for logging a line we refused to parse
/// without copying 64 KiB of it into the log.
#[must_use]
pub fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… ({} more bytes)", &s[..cut], s.len() - cut)
}

// ── Overlapped (deadline-bounded) I/O ───────────────────────────────
//
// A synchronous `ReadFile` on a byte-mode pipe has no timeout: an
// unresponsive peer hangs the other end for good (the CLI, a test's
// cleanup, or the daemon's main loop).
//
// Both handles are therefore opened with `FILE_FLAG_OVERLAPPED` and every
// I/O is awaited with a deadline and cancelled on expiry. See
// `docs/src/dev-guide/ipc-and-watchdog.md`, *Overlapped I/O and deadlines*.

/// Convert a deadline (`Instant`) into the millisecond argument expected by
/// [`WaitForSingleObject`], clamped at zero. Returns `None` if the deadline
/// has already passed.
fn ms_until(deadline: Instant) -> Option<u32> {
    let dur = deadline.checked_duration_since(Instant::now())?;
    // `WaitForSingleObject` takes a `u32`; cap at u32::MAX (~49 days).
    let ms = dur.as_millis().min(u128::from(u32::MAX)) as u32;
    Some(ms.max(1)) // never pass 0 (would mean "return immediately without waiting")
}

/// Block until the overlapped operation associated with `overlapped` completes
/// or `deadline` expires.
///
/// # Returns
///
/// On completion this calls [`GetOverlappedResult`] to retrieve the number of
/// bytes transferred and returns it. On timeout it cancels the operation with
/// [`CancelIo`] and returns a `TimedOut` error. Any other wait/I/O failure is
/// mapped to a `BrokenPipe` error.
///
/// # Safety
///
/// `overlapped` must reference an operation that was just started on `handle`
/// with the given event, and `event` must be the event recorded in
/// `overlapped.hEvent`.
unsafe fn await_overlapped(
    handle: HANDLE,
    event: HANDLE,
    overlapped: &OVERLAPPED,
    deadline: Instant,
) -> io::Result<u32> {
    let ms = ms_until(deadline).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "IPC operation timed out (deadline passed)",
        )
    })?;

    match unsafe { WaitForSingleObject(event, ms) } {
        // WAIT_OBJECT_0 (0): the overlapped operation's event was signalled —
        // it completed. Retrieve the authoritative byte count.
        WAIT_OBJECT_0 => {
            let mut transferred = 0u32;
            // SAFETY: `overlapped` was just used to start an I/O on `handle`
            // and its event has signalled completion; we only read the result.
            unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, false) }.map_err(
                |e| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        format!("GetOverlappedResult: {e}"),
                    )
                },
            )?;
            Ok(transferred)
        }
        // WAIT_TIMEOUT (258): the deadline expired before any I/O completed.
        // Cancel the in-flight operation so the kernel releases the buffers,
        // then surface a timeout.
        WAIT_TIMEOUT => {
            // SAFETY: cancelling this thread's I/O on our own handle, then
            // reaping it so `overlapped` and the caller's buffer are no
            // longer referenced by the kernel when they are dropped.
            unsafe { cancel_and_reap(handle, overlapped) };
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "IPC operation timed out waiting for the peer",
            ))
        }
        other => {
            // SAFETY: as above.
            unsafe { cancel_and_reap(handle, overlapped) };
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("WaitForSingleObject returned {other:?}"),
            ))
        }
    }
}

/// Cancel the calling thread's pending I/O on `handle` and wait for the
/// cancellation to complete.
///
/// `CancelIo` only *requests* the cancel; the operation finishes
/// asynchronously with `ERROR_OPERATION_ABORTED`. Until then the kernel may
/// still write through `overlapped` and the buffer the I/O was started
/// with, so they must not be freed — `GetOverlappedResult(bWait = true)`
/// blocks until that has happened (a cancel lands within microseconds).
///
/// # Safety
///
/// `overlapped` must reference an operation started on `handle` by this
/// thread.
unsafe fn cancel_and_reap(handle: HANDLE, overlapped: &OVERLAPPED) {
    // SAFETY: as documented above.
    let _ = unsafe { CancelIo(handle) };
    let mut transferred = 0u32;
    // SAFETY: waits for the operation's completion (or aborted status).
    let _ = unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, true) };
}

/// Write all of `data` to an overlapped pipe handle, bounded by `deadline`.
///
/// Loops until every byte is written, each chunk awaited via
/// [`await_overlapped`]. Returns `TimedOut` if the deadline passes before the
/// write completes.
fn write_all_overlapped(handle: HANDLE, data: &[u8], deadline: Instant) -> io::Result<()> {
    let event = EventHandle::new()?;
    let mut total = 0usize;

    while total < data.len() {
        // Manual-reset event: reset before each operation so the signal from a
        // previous chunk doesn't make the next wait return spuriously.
        let _ = unsafe { ResetEvent(event.raw()) };
        let mut overlapped = OVERLAPPED {
            hEvent: event.raw(),
            ..Default::default()
        };
        let mut written = 0u32;

        let result = unsafe {
            WriteFile(
                handle,
                Some(&data[total..]),
                Some(&mut written),
                Some(&mut overlapped),
            )
        };

        match result {
            // Completed synchronously — `written` is already authoritative.
            Ok(()) => {}
            Err(e) if e.code() == ERROR_IO_PENDING_HRESULT => {
                written = unsafe { await_overlapped(handle, event.raw(), &overlapped, deadline)? };
            }
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("WriteFile: {e}"),
                ));
            }
        }

        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "WriteFile wrote zero bytes",
            ));
        }
        total += written as usize;
    }

    Ok(())
}

/// Read a newline-terminated line from an overlapped pipe handle, bounded by
/// `deadline`.
///
/// Each chunk is read asynchronously and awaited via [`await_overlapped`]; the
/// function returns as soon as a `\n` is seen. If `deadline` passes with no
/// data, the outstanding read is cancelled and a `TimedOut` error is returned.
/// The deadline is also checked before every chunk, so a peer that keeps
/// the reads completing synchronously (data trickling in with no newline)
/// cannot hold the call past it, and a line that reaches [`MAX_LINE_BYTES`]
/// without a newline fails with `InvalidData`.
fn read_line_overlapped(handle: HANDLE, deadline: Instant) -> io::Result<String> {
    let event = EventHandle::new()?;
    let mut buf = vec![0u8; BUF_SIZE as usize];
    let mut raw: Vec<u8> = Vec::new();

    loop {
        // Re-checked on every iteration, including after a synchronous
        // completion: `await_overlapped` only sees the pending ones.
        if ms_until(deadline).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "IPC read timed out before the line was complete",
            ));
        }
        let _ = unsafe { ResetEvent(event.raw()) };
        let mut overlapped = OVERLAPPED {
            hEvent: event.raw(),
            ..Default::default()
        };
        let mut bytes_read = 0u32;

        let result = unsafe {
            ReadFile(
                handle,
                Some(buf.as_mut_slice()),
                Some(&mut bytes_read),
                Some(&mut overlapped),
            )
        };

        match result {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_IO_PENDING_HRESULT => {
                bytes_read =
                    unsafe { await_overlapped(handle, event.raw(), &overlapped, deadline)? };
            }
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("ReadFile: {e}"),
                ));
            }
        }

        if bytes_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "peer disconnected",
            ));
        }

        raw.extend_from_slice(&buf[..bytes_read as usize]);
        if raw.contains(&b'\n') {
            break;
        }
        if raw.len() > MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line exceeds {MAX_LINE_BYTES} bytes without a newline; dropping the peer"),
            ));
        }
    }

    String::from_utf8(raw).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("non-UTF-8 data from pipe: {e}"),
        )
    })
}

/// The reply the daemon writes when a client's line did not decode as a
/// [`SocketMessage`] — typically a `flow` CLI newer than the daemon sending a
/// command this daemon does not know. Produced for a read error of kind
/// [`io::ErrorKind::InvalidData`]; any other read error (disconnect, I/O)
/// has no client to answer and returns `None`.
#[must_use]
pub fn unrecognised_message_response(err: &io::Error) -> Option<SocketResponse> {
    (err.kind() == io::ErrorKind::InvalidData).then(|| SocketResponse::Error {
        message: "unrecognised message (client newer than daemon?)".to_owned(),
    })
}

/// Connect to the daemon's named pipe, send a message, and read the response.
///
/// This is the primary function used by the `flow` CLI to communicate with
/// the daemon. The pipe handle is RAII-wrapped — it is closed even if an
/// error occurs mid-transaction.
///
/// # Errors
///
/// - `ConnectionRefused` if the daemon is not running (pipe does not exist).
/// - `InvalidData` if serialisation fails or the daemon sends a malformed response.
/// - Other I/O errors for transport failures.
pub fn send_message(msg: &SocketMessage) -> io::Result<SocketResponse> {
    send_message_to(&message::pipe_name(), msg)
}

/// Connect to a specific named pipe, send a message, and read the response.
///
/// Like [`send_message`] but takes the pipe path directly instead of reading
/// from the `FLOW_PIPE_NAME` environment variable. Thread-safe for concurrent
/// use with different pipe names (no global env-var mutation).
///
/// # Timeout behaviour
///
/// The whole request/response is bounded by `IPC_TIMEOUT`. The client pipe
/// is opened for overlapped I/O and each read/write is awaited with that
/// deadline; if the daemon never replies (or replies too slowly) the call
/// returns [`io::ErrorKind::TimedOut`] instead of blocking forever. This is
/// what stops an unresponsive daemon from hanging the CLI — or, during panic
/// unwinding, a test's `DaemonGuard` drop.
///
/// # Errors
///
/// - `ConnectionRefused` if the daemon is not running (pipe does not exist).
/// - `TimedOut` if the daemon does not complete the round trip within
///   `IPC_TIMEOUT`.
/// - `InvalidData` if serialisation fails or the daemon sends a malformed response.
/// - Other I/O errors for transport failures.
pub fn send_message_to(pipe_name: &str, msg: &SocketMessage) -> io::Result<SocketResponse> {
    let handle = connect_to_named_pipe(pipe_name)?;
    let deadline = Instant::now() + IPC_TIMEOUT;

    // Write the message (overlapped, deadline-bounded).
    let wire = message::encode_message(msg)?;
    write_all_overlapped(handle.raw(), wire.as_bytes(), deadline)?;

    // Read the response (overlapped, same overall deadline).
    let line = read_line_overlapped(handle.raw(), deadline)?;

    message::decode_message(&line).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse response: {line:?}"),
        )
    })
}

/// Check if the daemon is running by attempting to open the named pipe.
///
/// Retries up to three times with a 50 ms sleep between attempts. This handles
/// the brief window after a previous connect + drop cycle where the
/// daemon may be between `DisconnectNamedPipe` and the next `ConnectNamedPipe`.
///
/// A pipe that exists but has no free instance (`ERROR_PIPE_BUSY`, then
/// `ERROR_SEM_TIMEOUT` from `WaitNamedPipeW`) counts as **running**: only a
/// live daemon can hold the pipe busy.
///
/// The handle is RAII-wrapped and closed immediately when dropped — no kernel
/// handle leak.
///
/// Returns `true` if the pipe exists (daemon is running), `false` otherwise.
#[must_use]
pub fn is_daemon_running() -> bool {
    let name = message::pipe_name();
    for _ in 0..3 {
        if pipe_exists(&name) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// One probe of [`is_daemon_running`]: connect, or recognise a busy pipe.
/// Never waits on a busy instance (unlike [`connect_to_named_pipe`]): busy
/// already answers the question.
fn pipe_exists(pipe_name: &str) -> bool {
    match open_named_pipe(pipe_name) {
        Ok(_handle) => true,
        Err(e) if e.code() == HRESULT::from_win32(ERROR_PIPE_BUSY.0) => true,
        Err(_) => {
            // Second opinion: WaitNamedPipeW answers at once when the pipe
            // does not exist (ERROR_FILE_NOT_FOUND) and times out with
            // ERROR_SEM_TIMEOUT when it exists but every instance is busy.
            let name = wide(pipe_name);
            // SAFETY: `name` is a valid null-terminated UTF-16 path.
            let ok = unsafe { WaitNamedPipeW(windows::core::PCWSTR(name.as_ptr()), 1) };
            ok.as_bool()
                || windows::core::Error::from_thread().code()
                    == windows::core::HRESULT::from_win32(ERROR_SEM_TIMEOUT.0)
        }
    }
}

/// Open a named pipe by path, returning an RAII-wrapped handle.
///
/// The handle is opened for **overlapped I/O** (`FILE_FLAG_OVERLAPPED`) so that
/// every read and write on it can be bounded by a deadline (see
/// [`send_message_to`]). This is what prevents an unresponsive daemon from
/// hanging the CLI/test forever: overlapped operations are awaited with
/// [`WaitForSingleObject`] and cancelled with [`CancelIo`] on timeout.
///
/// It is also opened with `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`
/// (the server may learn who the client is but not impersonate it), and a
/// busy pipe is waited for with `WaitNamedPipeW` and retried for up to
/// `BUSY_CONNECT_TIMEOUT`. See `docs/src/dev-guide/ipc-and-watchdog.md`,
/// *Pipe security* and *Overlapped I/O and deadlines*.
///
/// # Errors
///
/// - `ConnectionRefused` ("daemon not running") when the pipe does not
///   exist or cannot be opened for any reason other than being busy; the
///   raw Win32 error goes to the debug log only.
/// - `ResourceBusy` ("daemon is busy …") when it stayed busy for the whole
///   budget.
fn connect_to_named_pipe(pipe_name: &str) -> io::Result<PipeHandle> {
    let deadline = Instant::now() + BUSY_CONNECT_TIMEOUT;
    let busy = HRESULT::from_win32(ERROR_PIPE_BUSY.0);
    loop {
        let err = match open_named_pipe(pipe_name) {
            Ok(handle) => return Ok(handle),
            Err(e) => e,
        };
        if err.code() != busy {
            log::debug!("connect to {pipe_name}: {err}");
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "daemon not running",
            ));
        }
        let Some(ms) = ms_until(deadline) else {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "daemon is busy serving another client (no free pipe instance for {}s); retry shortly",
                    BUSY_CONNECT_TIMEOUT.as_secs()
                ),
            ));
        };
        // Returns as soon as an instance is free (or when `ms` elapse);
        // either way the next CreateFileW decides.
        let name = wide(pipe_name);
        // SAFETY: `name` is a valid null-terminated UTF-16 path.
        let _ = unsafe { WaitNamedPipeW(windows::core::PCWSTR(name.as_ptr()), ms) };
    }
}

/// One `CreateFileW` on a named pipe (overlapped), with the raw Win32 error.
fn open_named_pipe(pipe_name: &str) -> windows::core::Result<PipeHandle> {
    let name = wide(pipe_name);
    // SAFETY: `name` is a valid null-terminated UTF-16 path; the handle is
    // owned by the returned PipeHandle.
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL
                | FILE_FLAG_OVERLAPPED
                | SECURITY_SQOS_PRESENT
                | SECURITY_IDENTIFICATION,
            None,
        )
    }?;
    Ok(PipeHandle(handle))
}

/// Convert a Rust string to a wide (UTF-16) string with null terminator.
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
impl PipeServer {
    /// Construct a dummy pipe server for unit tests.
    ///
    /// The test path never reads from or writes to the pipe — dispatch
    /// handlers that no-op return before touching `self.server`. Both
    /// [`PipeHandle`] and [`EventHandle`] are given real Win32 event handles
    /// so their `Drop` impls (`CloseHandle`) succeed cleanly. No named pipe
    /// is created, so parallel tests don't conflict on `\\.\pipe\flow`.
    pub(crate) fn test_dummy() -> Self {
        let dummy = unsafe {
            CreateEventW(None, true, false, windows::core::PCWSTR::null())
                .expect("CreateEventW should not fail in test_dummy")
        };
        Self {
            handle: Some(PipeHandle(dummy)),
            connected_event: EventHandle::new().expect("EventHandle::new in test_dummy"),
            accept_thread: None,
            stopping: Arc::new(AtomicBool::new(false)),
            pipe_name: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A line that does not decode (a newer CLI's command) is answered with
    /// a recognisable `Error`; a plain disconnect / I/O failure is not.
    #[test]
    fn unparseable_message_gets_an_error_response() {
        let bad = io::Error::new(io::ErrorKind::InvalidData, "failed to parse message");
        assert_eq!(
            unrecognised_message_response(&bad),
            Some(SocketResponse::Error {
                message: "unrecognised message (client newer than daemon?)".to_owned(),
            })
        );
        let gone = io::Error::new(io::ErrorKind::BrokenPipe, "client went away");
        assert_eq!(unrecognised_message_response(&gone), None);
        let eof = io::Error::new(io::ErrorKind::UnexpectedEof, "closed");
        assert_eq!(unrecognised_message_response(&eof), None);
    }

    /// `read_message`'s own decode failure is the `InvalidData` kind the
    /// daemon loop keys on.
    #[test]
    fn decode_failure_is_invalid_data() {
        let line = "{\"type\":\"teleport_to_mars\"}";
        let err = message::decode_message::<SocketMessage>(line)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to parse message: {line:?}"),
                )
            })
            .expect_err("unknown command must not decode");
        assert!(unrecognised_message_response(&err).is_some());
    }

    /// A pipe name unique to this test process and test, so parallel tests
    /// (and a real daemon on `\\.\pipe\flow`) never collide.
    fn isolated_pipe_name(tag: &str) -> String {
        format!("\\\\.\\pipe\\flow-test-{}-{tag}", std::process::id())
    }

    #[test]
    fn shutdown_cancels_pending_accept_and_joins_in_time() {
        // The bug: a server dropped with an accept blocked in a synchronous
        // ConnectNamedPipe hung on CloseHandle until a client connected.
        let mut server = PipeServer::create_named(&isolated_pipe_name("cancel")).expect("pipe");
        server.start_accept();
        // Let the thread get into ConnectNamedPipe.
        std::thread::sleep(Duration::from_millis(50));
        assert!(server.accept_pending());

        let t0 = Instant::now();
        assert!(server.shutdown(), "accept thread must be joined");
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "shutdown took {:?}",
            t0.elapsed()
        );
        assert!(!server.accept_pending());
        // The connect event was not signalled: nobody connected.
        assert_eq!(
            unsafe { WaitForSingleObject(server.connected_event_handle(), 0) },
            WAIT_TIMEOUT
        );
        // Dropping afterwards is instant too.
        let t1 = Instant::now();
        drop(server);
        assert!(t1.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn server_read_times_out_on_a_silent_client() {
        // A client that connects and never writes must not wedge the
        // daemon: read_message returns TimedOut after SERVER_READ_TIMEOUT.
        let name = isolated_pipe_name("silent");
        let mut server = PipeServer::create_named(&name).expect("pipe");
        server.start_accept();
        let _silent_client = connect_to_named_pipe(&name).expect("client connects");
        assert_eq!(
            unsafe { WaitForSingleObject(server.connected_event_handle(), 3_000) },
            WAIT_OBJECT_0,
            "accept thread must signal the connection"
        );

        let t0 = Instant::now();
        let err = server
            .read_message()
            .expect_err("a silent client must time out");
        let elapsed = t0.elapsed();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(
            elapsed >= SERVER_READ_TIMEOUT - Duration::from_millis(200),
            "returned too early: {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");

        // The daemon's loop then disconnects it and accepts the next one.
        server.disconnect().expect("disconnect");
        server.start_accept();
        assert!(server.shutdown());
    }

    #[test]
    fn second_client_connects_at_once_after_the_first_disconnects() {
        // The daemon serves one client per accept: a second client arriving
        // while the server is still between DisconnectNamedPipe and the
        // re-armed ConnectNamedPipe sees ERROR_PIPE_BUSY and must wait for
        // the instance instead of failing.
        let name = isolated_pipe_name("busy");
        let mut server = PipeServer::create_named(&name).expect("pipe");
        server.start_accept();
        let first = connect_to_named_pipe(&name).expect("first client");
        assert_eq!(
            unsafe { WaitForSingleObject(server.connected_event_handle(), 3_000) },
            WAIT_OBJECT_0
        );
        drop(first);

        // Second client starts connecting before the server has re-armed.
        let second = std::thread::spawn({
            let name = name.clone();
            move || {
                let t0 = Instant::now();
                let handle = connect_to_named_pipe(&name);
                (handle.map(|_| ()), t0.elapsed())
            }
        });
        // The server loop: the busy instance is torn down and re-armed.
        std::thread::sleep(Duration::from_millis(10));
        server.disconnect().expect("disconnect");
        server.start_accept();

        let (result, elapsed) = second.join().expect("client thread");
        result.expect("second client must connect");
        assert!(elapsed < Duration::from_millis(100), "took {elapsed:?}");
        assert_eq!(
            unsafe { WaitForSingleObject(server.connected_event_handle(), 3_000) },
            WAIT_OBJECT_0
        );
        server.disconnect().expect("disconnect");
        assert!(server.shutdown());
    }

    #[test]
    fn server_drops_a_client_that_never_sends_a_newline() {
        // 200 KB of JSON-ish bytes with no `\n`: the read must stop at
        // MAX_LINE_BYTES with InvalidData instead of buffering forever, and
        // the error must not carry the payload into the log.
        let name = isolated_pipe_name("flood");
        let mut server = PipeServer::create_named(&name).expect("pipe");
        server.start_accept();
        let flooder = std::thread::spawn({
            let name = name.clone();
            move || {
                let client = connect_to_named_pipe(&name).expect("client connects");
                let payload = vec![b'{'; 200 * 1024];
                // The daemon stops reading at 64 KiB, so this write may be
                // cut short (BrokenPipe) — that is the expected outcome.
                let _ = write_all_overlapped(
                    client.raw(),
                    &payload,
                    Instant::now() + Duration::from_secs(5),
                );
            }
        });
        assert_eq!(
            unsafe { WaitForSingleObject(server.connected_event_handle(), 3_000) },
            WAIT_OBJECT_0
        );
        let t0 = Instant::now();
        let err = server.read_message().expect_err("flood must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().len() < 300, "error must stay short: {err}");
        assert!(
            t0.elapsed() < SERVER_READ_TIMEOUT,
            "refused by size, not by timeout: {:?}",
            t0.elapsed()
        );
        // The daemon loop then disconnects it and serves the next client.
        server.disconnect().expect("disconnect");
        let _ = flooder.join();
        server.start_accept();
        let client = std::thread::spawn({
            let name = name.clone();
            move || send_message_to(&name, &SocketMessage::Ping)
        });
        assert_eq!(
            unsafe { WaitForSingleObject(server.connected_event_handle(), 3_000) },
            WAIT_OBJECT_0
        );
        assert!(matches!(
            server.read_message().expect("read"),
            SocketMessage::Ping
        ));
        server.write_response(&SocketResponse::Ok).expect("write");
        assert_eq!(
            client.join().expect("client").expect("reply"),
            SocketResponse::Ok
        );
        server.disconnect().expect("disconnect");
        assert!(server.shutdown());
    }

    #[test]
    fn unparseable_line_error_is_truncated_for_the_log() {
        // A 10 KiB garbage line that does arrive with a newline is rejected
        // with a message holding a ~200-byte preview, not the whole line.
        let name = isolated_pipe_name("garbage");
        let mut server = PipeServer::create_named(&name).expect("pipe");
        server.start_accept();
        let client = std::thread::spawn({
            let name = name.clone();
            move || {
                let client = connect_to_named_pipe(&name).expect("client connects");
                let mut payload = vec![b'x'; 10 * 1024];
                payload.push(b'\n');
                write_all_overlapped(
                    client.raw(),
                    &payload,
                    Instant::now() + Duration::from_secs(5),
                )
                .expect("write garbage");
                // Hold the handle until the server has read it.
                std::thread::sleep(Duration::from_millis(300));
            }
        });
        assert_eq!(
            unsafe { WaitForSingleObject(server.connected_event_handle(), 3_000) },
            WAIT_OBJECT_0
        );
        let err = server.read_message().expect_err("garbage must not decode");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let text = err.to_string();
        assert!(
            text.len() < 400,
            "error must be truncated: {} bytes",
            text.len()
        );
        assert!(text.contains("more bytes"), "{text}");
        assert!(unrecognised_message_response(&err).is_some());
        server.disconnect().expect("disconnect");
        let _ = client.join();
        assert!(server.shutdown());
    }

    #[test]
    fn truncate_for_log_keeps_short_lines_and_cuts_long_ones_on_char_boundaries() {
        assert_eq!(truncate_for_log("short", 200), "short");
        assert_eq!(truncate_for_log("", 10), "");
        let long = "a".repeat(250);
        let cut = truncate_for_log(&long, 200);
        assert!(cut.starts_with(&"a".repeat(200)));
        assert!(cut.ends_with("… (50 more bytes)"), "{cut}");
        // A multi-byte char straddling the cut is dropped, never split.
        let s = format!("{}é", "b".repeat(199)); // é is 2 bytes at [199, 201)
        let cut = truncate_for_log(&s, 200);
        assert!(cut.starts_with(&"b".repeat(199)));
        assert!(cut.contains("… (2 more bytes)"), "{cut}");
    }

    #[test]
    fn pipe_sddl_grants_system_admins_and_the_user_only() {
        let sddl = pipe_sddl("S-1-5-21-1-2-3-1001");
        assert_eq!(
            sddl,
            "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;S-1-5-21-1-2-3-1001)"
        );
        // No Everyone (WD), Authenticated Users (AU), Users (BU) or
        // Interactive (IU) entries, and the DACL is protected.
        for sid in ["WD", "AU", "BU", "IU"] {
            assert!(!sddl.contains(&format!(";;;{sid})")), "{sddl}");
        }
        assert!(sddl.starts_with("D:P"));
        // The descriptor really parses, and the live SID is a real one.
        let sid = current_user_sid().expect("process token SID");
        assert!(sid.starts_with("S-1-"), "{sid}");
        let _descriptor = PipeSecurity::from_sddl(&pipe_sddl(&sid)).expect("SDDL converts");
        assert!(PipeSecurity::from_sddl("not sddl").is_err());
    }

    #[test]
    fn missing_pipe_is_daemon_not_running() {
        let err = connect_to_named_pipe(&isolated_pipe_name("absent"))
            .expect_err("no server on this name");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(err.to_string(), "daemon not running");
    }

    #[test]
    fn server_round_trip_over_the_overlapped_pipe() {
        // The server's overlapped read and write still carry a normal
        // request/response.
        let name = isolated_pipe_name("roundtrip");
        let mut server = PipeServer::create_named(&name).expect("pipe");
        server.start_accept();
        let client = std::thread::spawn({
            let name = name.clone();
            move || send_message_to(&name, &SocketMessage::Ping)
        });
        assert_eq!(
            unsafe { WaitForSingleObject(server.connected_event_handle(), 3_000) },
            WAIT_OBJECT_0
        );
        let msg = server.read_message().expect("read");
        assert!(matches!(msg, SocketMessage::Ping));
        server.write_response(&SocketResponse::Ok).expect("write");
        let reply = client.join().expect("client thread").expect("reply");
        assert_eq!(reply, SocketResponse::Ok);
        server.disconnect().expect("disconnect");
        assert!(server.shutdown());
    }

    #[test]
    fn shutdown_immediately_after_start_accept_does_not_hang() {
        // Race variant: the thread may not have reached ConnectNamedPipe yet
        // when shutdown is called; the re-issued CancelIoEx (and the
        // self-connect fallback) must still free it.
        let mut server = PipeServer::create_named(&isolated_pipe_name("race")).expect("pipe");
        server.start_accept();
        let t0 = Instant::now();
        assert!(server.shutdown());
        assert!(t0.elapsed() < Duration::from_secs(1));
        // start_accept after shutdown is a no-op.
        server.start_accept();
        assert!(!server.accept_pending());
    }

    #[test]
    fn drop_with_pending_accept_does_not_block() {
        let mut server = PipeServer::create_named(&isolated_pipe_name("drop")).expect("pipe");
        server.start_accept();
        std::thread::sleep(Duration::from_millis(30));
        let t0 = Instant::now();
        drop(server);
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "drop took {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn shutdown_without_accept_is_trivial() {
        let mut server = PipeServer::create_named(&isolated_pipe_name("idle")).expect("pipe");
        assert!(server.shutdown());
        assert!(server.shutdown());
    }

    // ── ms_until: deadline → milliseconds conversion for WaitForSingleObject ──
    //
    // Pure arithmetic extracted from the overlapped-I/O client transport.
    // No Win32, no pipe — just Instant arithmetic. Tests the clamping,
    // flooring, and None-on-expired behaviour that protects against
    // passing 0 (which would mean "return immediately") to WaitForSingleObject.

    #[test]
    fn ms_until_future_deadline_returns_positive_milliseconds() {
        // Positive: a deadline 500 ms in the future yields Some(≥500).
        let deadline = Instant::now() + Duration::from_millis(500);
        let result = ms_until(deadline);
        assert!(result.is_some());
        // Allow ±1 ms tolerance for clock drift during the call.
        let ms = result.unwrap();
        assert!((499..=501).contains(&ms), "expected ~500 ms, got {ms}");
    }

    #[test]
    fn ms_until_past_deadline_returns_none() {
        // Negative: a deadline already passed yields None (caller turns this
        // into a TimedOut error).
        let deadline = Instant::now() - Duration::from_millis(100);
        assert_eq!(ms_until(deadline), None);
    }

    #[test]
    fn ms_until_nearly_expired_floor_at_one() {
        // Positive edge: the function floors at 1 ms so WaitForSingleObject
        // doesn't get 0 (which means "don't wait at all"). When the deadline
        // is a few microseconds away, the result is Some(1), not Some(0).
        let deadline = Instant::now() + Duration::from_micros(500);
        let result = ms_until(deadline);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn ms_until_large_deadline_capped_at_u32_max() {
        // Positive: a 60-day deadline (far beyond u32::MAX ms ≈ 49 days)
        // is capped, not truncated.
        let deadline = Instant::now() + Duration::from_secs(60 * 24 * 3600);
        let result = ms_until(deadline);
        assert_eq!(result, Some(u32::MAX));
    }

    #[test]
    fn ms_until_zero_remaining_floors_to_one() {
        // Positive edge: when the deadline is effectively now (within a few
        // microseconds), checked_duration_since returns Some(~0), which is
        // floored to 1 ms by the `max(1)` guard so WaitForSingleObject
        // doesn't receive 0 (which would mean "don't wait at all").
        let deadline = Instant::now();
        let result = ms_until(deadline);
        // By the time ms_until runs, some sub-microsecond time has passed,
        // so we get Some(0ms) floored to Some(1). An exact-zero instant
        // would give None (checked_duration_since returns None when equal),
        // but that timing is not reproducible in a test.
        assert!(
            result.is_some(),
            "deadline at now should yield Some(1), got {result:?}"
        );
        assert_eq!(result.unwrap(), 1);
    }

    // ── wide: Rust string → UTF-16 null-terminated ────────────────────────

    #[test]
    fn wide_empty_string_produces_single_null() {
        // Positive: empty input → only the null terminator.
        let result = wide("");
        assert_eq!(result, vec![0u16]);
    }

    #[test]
    fn wide_ascii_string_includes_null_terminator() {
        // Positive: ASCII stays single-code-unit + null.
        let result = wide("abc");
        assert_eq!(result, vec![b'a' as u16, b'b' as u16, b'c' as u16, 0]);
    }

    #[test]
    fn wide_unicode_produces_correct_code_units() {
        // Positive: a multi-byte UTF-8 character (é = U+00E9) is one UTF-16
        // code unit; a surrogate-pair character (𐍈 = U+10348) is two.
        let result = wide("é");
        assert_eq!(result, vec![0x00E9, 0]); // single code unit + null

        let result_surrogate = wide("𐍈");
        // U+10348 → UTF-16 surrogate pair: 0xD800, 0xDF48
        assert_eq!(result_surrogate, vec![0xD800, 0xDF48, 0]);
    }

    #[test]
    fn wide_never_produces_empty_vec() {
        // Negative edge: even for empty input, the null terminator is always
        // present. This is a Win32 invariant — null-terminated strings must
        // have at least one element.
        let result = wide("");
        assert!(!result.is_empty());
        assert_eq!(*result.last().unwrap(), 0);
    }
}
