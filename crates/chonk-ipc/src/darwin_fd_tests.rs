//! Host policy/ownership tests. These do not execute the Darwin ABI on Linux.
use super::*;
use std::io::Read;
use std::os::unix::net::UnixStream;

fn pair() -> (OwnedFd, UnixStream) {
    let (a, b) = UnixStream::pair().unwrap();
    (a.into(), b)
}

/// Whether our end of the pair has been closed, asked of the peer: once the
/// last descriptor of our end is gone, the peer reads end-of-file.
///
/// The obvious check, `fcntl(raw, F_GETFD) == -1` with `EBADF`, races with
/// every other test in this process. Tests run on parallel threads, and the
/// kernel hands out the lowest free number, so another thread's socket or
/// file can take `raw` between the close and the check (CI saw exactly that:
/// `left: 1, right: -1`). The peer's view is tied to the socket, not to the
/// number. The wait is bounded because a process another thread forked
/// while our end was briefly inheritable holds a copy until it execs or exits.
fn peer_sees_close(peer: &UnixStream) -> bool {
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut byte = [0u8; 1];
    matches!((&*peer).read(&mut byte), Ok(0))
}

#[test]
fn adapter_sets_and_verifies_both_flags_before_returning() {
    let (fd, peer) = pair();
    let raw = fd.as_raw_fd();
    // SAFETY: integer-only libc calls on a descriptor number this test owns or has just closed.
    unsafe {
        assert_eq!(libc::fcntl(raw, libc::F_SETFL, 0), 0);
        assert_eq!(libc::fcntl(raw, libc::F_SETFD, 0), 0);
    }
    let configured = configure_owned(fd).unwrap();
    assert_eq!(configured.as_raw_fd(), raw);
    // SAFETY: integer-only libc calls on a descriptor number this test owns or has just closed.
    unsafe {
        assert_ne!(libc::fcntl(raw, libc::F_GETFL) & libc::O_NONBLOCK, 0);
        assert_ne!(libc::fcntl(raw, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
    }
    drop(configured);
    assert!(peer_sees_close(&peer), "dropping the configured descriptor closes it");
}

#[test]
fn every_flag_operation_failure_closes_the_owned_descriptor() {
    for failure in 1..=6 {
        let (fd, peer) = pair();
        let mut calls = 0;
        let result = configure_owned_with(fd, |fd, command, value| {
            calls += 1;
            if calls == failure {
                Err(io::Error::from_raw_os_error(libc::EIO))
            } else {
                // SAFETY: integer-only libc calls on a descriptor number this test owns or has just closed.
                cvt(unsafe { libc::fcntl(fd, command, value) })
            }
        });
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
        assert_eq!(calls, failure, "no retry after the first failure");
        assert!(peer_sees_close(&peer), "failure {failure} closes the owned descriptor");
    }
}

#[test]
fn successful_noop_setters_cannot_return_an_unconfigured_fd() {
    for missing_command in [libc::F_SETFL, libc::F_SETFD] {
        let (fd, peer) = pair();
        let raw = fd.as_raw_fd();
        // SAFETY: integer-only libc calls on a descriptor number this test owns or has just closed.
        unsafe {
            assert_eq!(libc::fcntl(raw, libc::F_SETFL, 0), 0);
            assert_eq!(libc::fcntl(raw, libc::F_SETFD, 0), 0);
        }
        let result = configure_owned_with(fd, |fd, command, value| {
            if command == missing_command { Ok(0) }
            // SAFETY: integer-only libc calls on a descriptor number this test owns or has just closed.
            else { cvt(unsafe { libc::fcntl(fd, command, value) }) }
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(peer_sees_close(&peer), "an unconfigured descriptor is closed, not returned");
    }
}

#[test]
fn live_stream_listener_has_no_blocking_or_inheritable_fd() {
    let base = std::env::temp_dir().join(format!("chonk-ipc-native-port-{}-{}",
        std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir(&base).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = base.join("test.sock");
    let listener = StreamListener::bind(&path).unwrap();
    assert!(listener.accept().unwrap().is_none());
    let client = Stream::connect(&path).unwrap();
    let accepted = listener.accept().unwrap().expect("queued connection");
    for fd in [listener.as_raw_fd(), client.as_raw_fd(), accepted.as_raw_fd()] {
        // SAFETY: integer-only libc calls on a descriptor number this test owns or has just closed.
        unsafe {
            assert_ne!(libc::fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK, 0);
            assert_ne!(libc::fcntl(fd, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
        }
    }
    // SAFETY: integer-only libc calls on a descriptor number this test owns or has just closed.
    assert_eq!(peer_credentials_of(accepted.as_raw_fd()).unwrap().uid, unsafe { libc::geteuid() });
    // SAFETY: integer-only libc calls on a descriptor number this test owns or has just closed.
    assert_eq!(peer_credentials_of(client.as_raw_fd()).unwrap().pid, unsafe { libc::getpid() });
    assert!(StreamListener::bind(&path).is_err(), "do not steal a live listener");
    drop(accepted); drop(client); drop(listener);
    std::fs::remove_dir(base).unwrap();
}

#[test]
fn darwin_ambiguous_connect_errors_preserve_a_real_listener_path() {
    // Inject the errno reported by XNU for a full live accept queue. This is
    // the actual Darwin error-policy helper, with a real owned socket path;
    // it is not host execution of the XNU connect syscall.
    let base = std::env::temp_dir().join(format!("chonk-ipc-stale-policy-{}-{}",
        std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir(&base).unwrap();
    let path = base.join("live.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    for code in [libc::ECONNREFUSED, libc::EINPROGRESS, libc::ENOMEM, libc::EACCES] {
        let error = io::Error::from_raw_os_error(code);
        let kind = error.kind();
        assert_eq!(refuse_ambiguous_darwin_endpoint(&path, error).unwrap_err().kind(), kind);
        assert!(path.exists(), "an ambiguous connect error must not unlink a live owner");
    }
    let connected = UnixStream::connect(&path).unwrap();
    let accepted = listener.accept().unwrap();
    drop(connected); drop(accepted); drop(listener);
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(base).unwrap();
}
