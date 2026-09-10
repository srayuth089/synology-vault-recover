//! Opening a root-owned device through macOS's own authorization dialog.
//!
//! Raw disk nodes are `root:operator`, so a normal user cannot read them. Rather
//! than asking the user to run `chmod` in a terminal, this asks
//! `/usr/libexec/authopen` — a setuid helper shipped with macOS — to present the
//! standard password prompt and hand back an already-open file descriptor over a
//! unix socket using `SCM_RIGHTS`.
//!
//! The request is for `sys.openfile.readonly`, so the descriptor that comes back
//! cannot be written to even if some later code tried: the kernel refuses it.

use std::os::unix::io::{FromRawFd, RawFd};
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("cannot start /usr/libexec/authopen: {0}")]
    Spawn(std::io::Error),
    #[error("authorization was declined or cancelled")]
    Declined,
    #[error("authopen did not return a file descriptor")]
    NoDescriptor,
    #[error("socket error while receiving the descriptor: {0}")]
    Socket(std::io::Error),
}

/// Whether this path needs elevation, i.e. we cannot already read it.
pub fn needs_authorization(path: impl AsRef<Path>) -> bool {
    std::fs::File::open(path.as_ref()).is_err()
}

/// Open `path` read-only via `authopen`, showing the system password dialog.
///
/// Returns the raw descriptor; the caller wraps it in a `File`.
pub fn open_readonly(path: impl AsRef<Path>) -> Result<RawFd, AuthError> {
    let path = path.as_ref();

    // A socketpair: authopen writes the descriptor into its stdout, which is
    // this socket, using SCM_RIGHTS.
    let (ours, theirs) = unix_socketpair().map_err(AuthError::Socket)?;

    let mut child = Command::new("/usr/libexec/authopen")
        .arg("-stdoutpipe")
        .arg(path)
        // SAFETY: dup2 onto stdout in the child before exec; the fd is valid
        // and closed on drop in the parent.
        .stdout(unsafe { Stdio::from_raw_fd(theirs) })
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(AuthError::Spawn)?;

    let fd = recv_fd(ours);
    // Close our copy of the child's end so the child sees EOF.
    unsafe { libc::close(ours) };

    let status = child.wait().map_err(AuthError::Spawn)?;
    match fd {
        Ok(fd) if status.success() => Ok(fd),
        // authopen exits non-zero when the user cancels the dialog.
        _ if !status.success() => Err(AuthError::Declined),
        Ok(_) => Err(AuthError::NoDescriptor),
        Err(e) => Err(AuthError::Socket(e)),
    }
}

fn unix_socketpair() -> std::io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: fds is a valid 2-element array for the duration of the call.
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

/// Receive a single descriptor sent with `SCM_RIGHTS`.
fn recv_fd(sock: RawFd) -> std::io::Result<RawFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    // Space for exactly one descriptor.
    let mut cmsg_buf = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_buf.len() as libc::socklen_t;

    // SAFETY: msg points at live locals sized as declared above.
    let n = unsafe { libc::recvmsg(sock, &mut msg, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: the kernel filled msg_control with a well-formed cmsghdr.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no control message: authopen sent no descriptor",
        ));
    }
    let cmsg = unsafe { &*cmsg };
    if cmsg.cmsg_level != libc::SOL_SOCKET || cmsg.cmsg_type != libc::SCM_RIGHTS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "control message was not SCM_RIGHTS",
        ));
    }
    // SAFETY: SCM_RIGHTS payload is one RawFd.
    let fd = unsafe { std::ptr::read(libc::CMSG_DATA(cmsg) as *const RawFd) };
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_readable_file_does_not_need_authorization() {
        // Any path this process can already open must not trigger a prompt.
        assert!(!needs_authorization("/usr/libexec/authopen"));
    }

    #[test]
    fn a_root_owned_device_needs_authorization() {
        // /dev/rdisk0 is root:operator on every Mac; if this process could
        // already read it, the check must say so rather than prompting.
        let expected = std::fs::File::open("/dev/rdisk0").is_err();
        assert_eq!(needs_authorization("/dev/rdisk0"), expected);
    }

    #[test]
    fn a_missing_path_reports_as_needing_authorization() {
        // Distinguishing "absent" from "forbidden" is the caller's job; the
        // probe only answers "can I open this right now".
        assert!(needs_authorization("/dev/definitely-not-a-real-device"));
    }
}
