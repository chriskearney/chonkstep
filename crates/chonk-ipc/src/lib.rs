//! Nonblocking, same-user Unix stream sockets for compositor IPC.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
const BACKLOG: libc::c_int = 16;
#[derive(Debug, Clone, Copy)]
pub struct PeerCredentials { pub pid: libc::pid_t, pub uid: libc::uid_t, pub gid: libc::gid_t }
fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn cvt_size(ret: libc::ssize_t) -> io::Result<usize> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

fn sockaddr_un(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: this C socket-address structure contains only integer and
    // byte-array fields, for which all-zero is a valid initial value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("socket path is {} bytes, the kernel's limit is {}: {}", bytes.len(), addr.sun_path.len() - 1, path.display()),
        ));
    }
    if bytes.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket path contains a NUL byte"));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, &byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = byte as libc::c_char;
    }
    #[cfg(target_os = "macos")]
    {
        // XNU sys/un.h: sun_len includes the terminating NUL.
        let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
        addr.sun_len = u8::try_from(len).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Unix address too long"))?;
        Ok((addr, len as libc::socklen_t))
    }
    #[cfg(not(target_os = "macos"))]
    Ok((addr, std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t))
}

pub fn socket_dir() -> io::Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "XDG_RUNTIME_DIR is not set; IPC sockets need a private per-user runtime directory and will not fall back to /tmp",
        )
    })?;
    Ok(PathBuf::from(runtime).join("chonkstep"))
}

pub fn sanitize_display(display: &str) -> String {
    let cleaned: String = display
        .trim_start_matches(':')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(32)
        .collect();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

pub fn control_socket_path(display: &str) -> io::Result<PathBuf> {
    Ok(socket_dir()?.join(format!("control-{}.sock", sanitize_display(display))))
}

pub fn ensure_socket_dir(dir: &Path) -> io::Result<()> {
    let c_dir = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket directory path contains a NUL byte"))?;
    // SAFETY: `c_dir` is NUL-terminated and remains live for the call.
    let made = unsafe { libc::mkdir(c_dir.as_ptr(), 0o700) };
    if made < 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::AlreadyExists {
            return Err(err);
        }
        // SAFETY: all-zero is a valid initial representation for this C
        // output structure; `lstat` fills it before any field is read.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the path is a live NUL-terminated string and `stat` points
        // to writable storage of the exact structure size.
        cvt(unsafe { libc::lstat(c_dir.as_ptr(), &mut stat) })?;
        let is_dir = stat.st_mode & libc::S_IFMT == libc::S_IFDIR;
        // SAFETY: `geteuid` takes no arguments and has no memory preconditions.
        let is_ours = stat.st_uid == unsafe { libc::geteuid() };
        let is_private = stat.st_mode & 0o077 == 0;
        if !is_dir || !is_ours || !is_private {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} exists but is not a private directory owned by this user (dir={is_dir}, ours={is_ours}, mode={:o})",
                    dir.display(),
                    stat.st_mode & 0o777
                ),
            ));
        }
    }
    Ok(())
}

// Darwin lacks socket/accept4 creation flags. Complete both flags before
// publishing an owned descriptor. This is not atomic with concurrent fork/exec;
// the desktop session must serialize socket creation and application spawning.
#[cfg(any(target_os = "macos", test))]
fn configure_owned_with(
    fd: OwnedFd,
    mut operation: impl FnMut(RawFd, libc::c_int, libc::c_int) -> io::Result<libc::c_int>,
) -> io::Result<OwnedFd> {
    let raw = fd.as_raw_fd();
    let flags = operation(raw, libc::F_GETFL, 0)?;
    operation(raw, libc::F_SETFL, flags | libc::O_NONBLOCK)?;
    let descriptor_flags = operation(raw, libc::F_GETFD, 0)?;
    operation(raw, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC)?;
    let actual_flags = operation(raw, libc::F_GETFL, 0)?;
    let actual_descriptor_flags = operation(raw, libc::F_GETFD, 0)?;
    if actual_flags & libc::O_NONBLOCK == 0 || actual_descriptor_flags & libc::FD_CLOEXEC == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "socket flags did not stick"));
    }
    Ok(fd)
}

#[cfg(any(target_os = "macos", test))]
fn configure_owned(fd: OwnedFd) -> io::Result<OwnedFd> {
    configure_owned_with(fd, |fd, command, value| {
        // SAFETY: fd is owned and live throughout this call. All used fcntl
        // commands take an integer third argument (ignored by the getters).
        cvt(unsafe { libc::fcntl(fd, command, value) })
    })
}

fn new_nonblocking_socket(kind: libc::c_int) -> io::Result<OwnedFd> {
    #[cfg(not(target_os = "macos"))]
    // SAFETY: `socket` takes only integer arguments and returns a new raw fd,
    // whose error sentinel is checked by `cvt`.
    let raw = cvt(unsafe { libc::socket(libc::AF_UNIX, kind | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) })?;
    #[cfg(target_os = "macos")]
    // SAFETY: as above; Darwin has no creation flags, see `configure_owned`.
    let raw = cvt(unsafe { libc::socket(libc::AF_UNIX, kind, 0) })?;
    // SAFETY: socket returned a fresh, checked, nonnegative descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(target_os = "macos")]
    let fd = configure_owned(fd)?;
    Ok(fd)
}

fn connect_nonblocking(path: &Path, kind: libc::c_int) -> io::Result<OwnedFd> {
    let (addr, len) = sockaddr_un(path)?;
    let fd = new_nonblocking_socket(kind)?;
    // SAFETY: `addr` remains live for the computed initialized length and the
    // owned descriptor remains open for the duration of the syscall.
    cvt(unsafe { libc::connect(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) })?;
    Ok(fd)
}

fn bind_listener(path: &Path, kind: libc::c_int) -> io::Result<OwnedFd> {
    if let Some(parent) = path.parent() {
        ensure_socket_dir(parent)?;
    }
    clear_stale_socket(path, kind)?;

    let (addr, len) = sockaddr_un(path)?;
    // Linux sets flags atomically at creation; Darwin configures and verifies
    // them before publication. Neither exposes a blocking listener to callers.
    let fd = new_nonblocking_socket(kind)?;
    // SAFETY: `addr` remains live for the computed initialized length and the
    // owned descriptor remains open for the duration of the syscall.
    cvt(unsafe { libc::bind(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) })?;

    // `bind()` applied the process umask, so tighten explicitly.
    // The window between bind and chmod is real; the 0700 parent
    // directory is what actually makes it unreachable, and this is
    // the second lock.
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path contains a NUL byte"))?;
    // SAFETY: `c_path` is NUL-terminated and remains live for the call.
    cvt(unsafe { libc::chmod(c_path.as_ptr(), 0o600) })?;

    // SAFETY: `fd` owns the live socket; `listen` has no userspace pointer
    // arguments or additional memory-safety preconditions.
    cvt(unsafe { libc::listen(fd.as_raw_fd(), BACKLOG) })?;
    Ok(fd)
}

#[cfg(any(target_os = "macos", test))]
fn refuse_ambiguous_darwin_endpoint(path: &Path, error: io::Error) -> io::Result<()> {
    // XNU also returns ECONNREFUSED when a live accept queue is full.
    // Connect errno cannot establish that we own an abandoned pathname.
    Err(io::Error::new(error.kind(), format!(
        "cannot establish that {} is abandoned; refusing to unlink: {error}", path.display()
    )))
}

fn clear_stale_socket(path: &Path, kind: libc::c_int) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match connect_nonblocking(path, kind) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("{} is already accepting connections; another chonkstep session owns this display", path.display()),
        )),
        // `EAGAIN` from the now-non-blocking probe means the socket
        // *is* listening and its backlog is momentarily full — a
        // live owner, not debris. This case has to be named
        // explicitly: falling into the `Err` arm below would unlink
        // a socket somebody is still serving, which is precisely
        // the "second shell silently steals the first one's
        // dockapps" outcome the probe exists to prevent. (Before
        // `connect` was made non-blocking this case could not
        // return at all; it hung instead, which is why the arm is
        // new and not merely rearranged.)
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "{} is listening but its backlog is full; something owns this display and is not accepting",
                path.display()
            ),
        )),
        // `EPROTOTYPE` means something *is* listening here, just not
        // with the socket type we probed with — a stream listener seen
        // through a SEQPACKET probe or vice versa. Two flavours now
        // share this directory (the dock socket and the control
        // socket), so the arm exists to make "wrong kind" read as
        // "live owner" rather than as debris to be unlinked. The two
        // never share a filename, so in practice this only fires when
        // something else has put its own socket where ours goes; the
        // right answer is still to refuse, not to steal.
        Err(e) if e.raw_os_error() == Some(libc::EPROTOTYPE) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("{} is a live socket of another type; refusing to replace it", path.display()),
        )),
        // No Darwin connect error, including ECONNREFUSED, proves staleness.
        #[cfg(target_os = "macos")]
        Err(e) => refuse_ambiguous_darwin_endpoint(path, e),
        #[cfg(not(target_os = "macos"))]
        Err(_) => std::fs::remove_file(path),
    }
}

fn accept_nonblocking(listener: RawFd) -> io::Result<Option<OwnedFd>> {
    #[cfg(not(target_os = "macos"))]
    // SAFETY: `listener` is used only as a kernel handle; both optional
    // address outputs are null, and `accept4` returns a new fd or -1.
    let raw = unsafe {
        libc::accept4(listener, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
    };
    #[cfg(target_os = "macos")]
    // SAFETY: as above, with `accept`: Darwin has no `accept4`. The listener
    // was verified nonblocking; the accepted descriptor is configured below
    // before a caller can see it.
    let raw = unsafe { libc::accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) };
    if raw < 0 {
        let err = io::Error::last_os_error();
        return match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
            _ => Err(err),
        };
    }
    // SAFETY: successful accept returned a new nonnegative descriptor with
    // no other Rust owner; ownership transfers exactly once.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(target_os = "macos")]
    let fd = configure_owned(fd)?;
    Ok(Some(fd))
}

#[cfg(not(target_os = "macos"))]
pub fn peer_credentials_of(fd: RawFd) -> io::Result<PeerCredentials> {
    // SAFETY: all-zero is a valid initial representation for this C output
    // structure, and the kernel fills it before fields are read.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` points to writable storage of `len` bytes and `len`
    // itself is a live writable socklen; `fd` is only a kernel handle.
    cvt(unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) })?;
    Ok(PeerCredentials { pid: cred.pid, uid: cred.uid, gid: cred.gid })
}

/// Real Darwin AF_UNIX credentials. PID is XNU's peer socket last_pid,
/// not a claim of Linux SO_PEERCRED's stable creator-PID semantics. UID/GID
/// come from the connection credentials via getpeereid; these separate reads
/// are not an atomic process-identity snapshot or a liveness lease.
#[cfg(target_os = "macos")]
pub fn peer_credentials_of(fd: RawFd) -> io::Result<PeerCredentials> {
    let mut uid = libc::uid_t::MAX;
    let mut gid = libc::gid_t::MAX;
    // SAFETY: both output pointers have the exact target uid/gid types.
    cvt(unsafe { libc::getpeereid(fd, &mut uid, &mut gid) })?;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
    // SAFETY: pid and len are live writable outputs of the declared sizes.
    // XNU (uipc_usrreq.c) supports SOL_LOCAL/LOCAL_PEERPID on connected sockets.
    cvt(unsafe { libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERPID,
        (&mut pid as *mut libc::pid_t).cast(), &mut len) })?;
    if len as usize != std::mem::size_of_val(&pid) || pid <= 0 || uid == libc::uid_t::MAX || gid == libc::gid_t::MAX {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "incomplete local peer credentials"));
    }
    Ok(PeerCredentials { pid, uid, gid })
}

pub fn peer_is_this_user_on(fd: RawFd) -> io::Result<bool> {
    // SAFETY: `geteuid` takes no arguments and has no memory preconditions.
    Ok(peer_credentials_of(fd)?.uid == unsafe { libc::geteuid() })
}

/// Whether the peer on `fd` is this user, or root.
///
/// For a socket that takes *commands* and lives under a `0700`
/// directory. Refusing uid 0 there protects nothing: root walks into
/// the directory regardless, can read the process's memory and can
/// `kill` it, so the only effect of dropping its connection is that a
/// root-run `hyprctl` — Omarchy's pacman hooks, `sudo hyprctl reload`
/// — gets silence instead of an answer or a refusal. Every other uid
/// stays refused: they are exactly who the directory mode keeps out.
pub fn peer_is_this_user_or_root_on(fd: RawFd) -> io::Result<bool> {
    Ok(uid_is_this_user_or_root(peer_credentials_of(fd)?.uid))
}

/// The rule behind [`peer_is_this_user_or_root_on`], on a bare uid so
/// it can be checked without a socket.
pub fn uid_is_this_user_or_root(uid: libc::uid_t) -> bool {
    // SAFETY: `geteuid` takes no arguments and has no memory preconditions.
    uid == 0 || uid == unsafe { libc::geteuid() }
}

pub fn send_on(fd: RawFd, message: &[u8]) -> io::Result<usize> {
    // SAFETY: `message` is readable for the supplied length and remains live;
    // the raw descriptor is passed only as a kernel handle.
    cvt_size(unsafe { libc::send(fd, message.as_ptr().cast(), message.len(), libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) })
}

pub fn wait_readable(fd: RawFd, timeout: Option<Duration>) -> io::Result<bool> {
    let mut pollfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let timeout_ms = match timeout {
        Some(d) => d.as_millis().min(libc::c_int::MAX as u128) as libc::c_int,
        None => -1,
    };
    // SAFETY: `pollfd` is one initialized value, writable for the exact
    // element count supplied, and remains live until `poll` returns.
    let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
    if ready < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(err);
    }
    Ok(ready > 0)
}

#[derive(Debug)]
pub struct StreamListener {
    fd: OwnedFd,
    path: PathBuf,
}

impl StreamListener {
    /// See `Stream`; identical apart from the socket
    /// type, including the refusal to evict a live owner.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let fd = bind_listener(path, libc::SOCK_STREAM)?;
        Ok(Self { fd, path: path.to_path_buf() })
    }

    /// Accepts one pending connection as a socket that is already
    /// `O_NONBLOCK` and `CLOEXEC`, or `Ok(None)` if none is waiting.
    /// Linux sets both flags atomically with accept4. Darwin configures and
    /// verifies them before publication; concurrent spawning needs the separate
    /// audited session-spawn contract described by configure_owned_with.
    pub fn accept(&self) -> io::Result<Option<Stream>> {
        Ok(accept_nonblocking(self.fd.as_raw_fd())?.map(Stream::from_fd))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRawFd for StreamListener {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Drop for StreamListener {
    /// Unlinks the socket file — the same reasoning as
    /// `Stream`'s `Drop`.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One connected `SOCK_STREAM` socket, either end, non-blocking on
/// every call by construction.
#[derive(Debug)]
pub struct Stream {
    fd: OwnedFd,
}

impl Stream {
    /// A non-blocking connect — see the private `connect_nonblocking`. Used by
    /// tests and by anything in this workspace that wants to read the
    /// control socket without being a shell.
    pub fn connect(path: &Path) -> io::Result<Self> {
        Ok(Self { fd: connect_nonblocking(path, libc::SOCK_STREAM)? })
    }

    pub fn from_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// Whether `O_NONBLOCK` is actually set on this fd right now — the
    /// subject of an assertion, as for `Stream`.
    pub fn is_nonblocking(&self) -> io::Result<bool> {
        // SAFETY: this object owns a live descriptor; `F_GETFL` has no third
        // argument and does not dereference userspace memory.
        let flags = cvt(unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GETFL) })?;
        Ok(flags & libc::O_NONBLOCK != 0)
    }

    /// Writes as much of `bytes` as the socket will take right now and
    /// returns how much that was. `MSG_DONTWAIT | MSG_NOSIGNAL`, for
    /// the reasons `Stream` spells out; the one difference
    /// from the datagram send is that a stream *may* take part of the
    /// buffer, so the caller keeps the remainder for the next pass
    /// rather than treating a short write as a failure.
    pub fn send(&self, bytes: &[u8]) -> io::Result<usize> {
        send_on(self.fd.as_raw_fd(), bytes)
    }

    /// Reads whatever is available. `Ok(0)` is EOF: the peer closed.
    /// `MSG_DONTWAIT`, so an idle socket answers `WouldBlock` rather
    /// than parking the caller.
    pub fn recv(&self, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buffer` is writable for the supplied length and remains
        // exclusively borrowed; the object owns the live descriptor.
        cvt_size(unsafe { libc::recv(self.fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), libc::MSG_DONTWAIT) })
    }

    pub fn peer_is_this_user(&self) -> io::Result<bool> {
        peer_is_this_user_on(self.fd.as_raw_fd())
    }

    /// See [`peer_is_this_user_or_root_on`].
    pub fn peer_is_this_user_or_root(&self) -> io::Result<bool> {
        peer_is_this_user_or_root_on(self.fd.as_raw_fd())
    }

    /// Whether the peer has fully closed the connection.
    ///
    /// This is the server-side liveness check for a write-only event
    /// stream: when there are no bytes queued, `send` has nothing to
    /// call and `recv` would violate the protocol's direction, but a
    /// dead peer still leaves `POLLHUP` permanently asserted. Zero
    /// timeout means this is a query, never a wait. A failed or
    /// interrupted poll is deliberately "not known dead"; keeping an
    /// ambiguous connection is safer than dropping a live one.
    pub fn peer_gone(&self) -> bool {
        let mut fd = libc::pollfd { fd: self.fd.as_raw_fd(), events: 0, revents: 0 };
        // SAFETY: `fd` is one initialized `pollfd`, writable for the exact
        // element count supplied, and remains live for this nonblocking call.
        let ready = unsafe { libc::poll(&mut fd, 1, 0) };
        ready > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR) != 0
    }

    /// Blocks (with `poll`) until bytes arrive or `deadline` passes —
    /// for a *client* only, exactly as `Stream` is.
    /// `Ok(None)` on timeout, `Ok(Some(0))` on EOF.
    pub fn recv_until(&self, buffer: &mut [u8], deadline: std::time::Instant) -> io::Result<Option<usize>> {
        loop {
            match self.recv(buffer) {
                Ok(n) => return Ok(Some(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            if !wait_readable(self.as_raw_fd(), Some(deadline - now))? {
                continue;
            }
        }
    }

    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }
}

impl AsRawFd for Stream {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// The command socket's peer rule: this user and root are the
    /// callers that can reach the socket anyway, and nobody else is.
    #[test]
    fn the_command_peer_rule_admits_this_user_and_root_and_nobody_else() {
        // SAFETY: `geteuid` takes no arguments and has no memory preconditions.
        let me = unsafe { libc::geteuid() };
        assert!(uid_is_this_user_or_root(me), "the compositor's own uid");
        assert!(uid_is_this_user_or_root(0), "root, who owns the machine regardless");
        // A uid that is neither: `nobody` unless that is who is running
        // the tests, in which case one below it.
        let stranger = if me == 65534 { 65533 } else { 65534 };
        assert!(!uid_is_this_user_or_root(stranger), "any other uid stays refused");
        if me != 0 && me != u32::MAX - 1 {
            assert!(!uid_is_this_user_or_root(me + 1), "off by one is another user");
        }
    }

    /// The same rule read off a live socket through `SO_PEERCRED`, so
    /// the predicate the server calls is exercised end to end, not
    /// only the arithmetic behind it.
    #[test]
    fn a_socket_peer_of_this_user_passes_both_predicates() {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        assert!(peer_is_this_user_on(ours.as_raw_fd()).expect("SO_PEERCRED"));
        assert!(peer_is_this_user_or_root_on(ours.as_raw_fd()).expect("SO_PEERCRED"));
        assert!(peer_is_this_user_or_root_on(theirs.as_raw_fd()).expect("SO_PEERCRED"));
    }
}

#[cfg(test)]
#[path = "darwin_fd_tests.rs"]
mod darwin_fd_tests;
