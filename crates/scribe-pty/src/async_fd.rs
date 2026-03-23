#![allow(unsafe_code, reason = "PTY I/O requires unsafe libc calls (read, write, fcntl)")]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Zero-thread non-blocking PTY I/O wrapper.
///
/// Wraps a PTY master [`OwnedFd`] in tokio's [`AsyncFd`] so reads and writes
/// are driven by epoll/kqueue rather than a `spawn_blocking` thread per session.
///
/// # Construction
///
/// The fd is set to `O_NONBLOCK` during [`AsyncPtyFd::new`] so that the kernel
/// returns `EAGAIN`/`EWOULDBLOCK` instead of blocking when no data is ready.
pub struct AsyncPtyFd {
    inner: AsyncFd<OwnedFd>,
}

impl AsyncPtyFd {
    /// Wrap a PTY master fd for async I/O.
    ///
    /// Sets the fd to non-blocking mode and registers it with the current
    /// tokio reactor. Must be called from within a tokio runtime context.
    ///
    /// # Errors
    ///
    /// Returns an error if `fcntl` fails or if the reactor registration fails.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        set_nonblocking(&fd)?;
        let inner = AsyncFd::new(fd)?;
        Ok(Self { inner })
    }

    /// Return the raw file descriptor for use with ioctls (e.g. `TIOCSWINSZ`).
    ///
    /// The returned fd is valid for the lifetime of `self`.
    pub fn raw_fd(&self) -> RawFd {
        self.inner.get_ref().as_raw_fd()
    }
}

/// Set the `O_NONBLOCK` flag on `fd` via `fcntl(F_GETFL)` / `fcntl(F_SETFL)`.
///
/// # Errors
///
/// Returns an [`io::Error`] if either `fcntl` call fails.
fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: `fd` is a valid, open file descriptor owned by the caller.
    // `F_GETFL` takes no additional argument and returns flags on success.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `fd` is valid and `flags | O_NONBLOCK` is a well-formed flag
    // set. `F_SETFL` with an integer argument is the documented usage.
    let ret = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Attempt a single non-blocking `read(2)` from `fd` into the unfilled part of `buf`.
///
/// Returns `WouldBlock` when the kernel has no data ready.
///
/// # Errors
///
/// Returns an [`io::Error`] on any `read(2)` failure.
fn try_read(fd: &OwnedFd, buf: &mut ReadBuf<'_>) -> io::Result<()> {
    let unfilled = buf.initialize_unfilled();

    // SAFETY: `fd` is a valid, open PTY master fd. `unfilled` is a live,
    // writable slice of the declared length. The pointer cast from `*mut u8`
    // to `*mut libc::c_void` is safe because `c_void` has no alignment
    // requirement that `u8` does not also satisfy.
    let n = unsafe {
        libc::read(fd.as_raw_fd(), unfilled.as_mut_ptr().cast::<libc::c_void>(), unfilled.len())
    };

    if n < 0 {
        return Err(io::Error::last_os_error());
    }

    #[allow(clippy::cast_sign_loss, reason = "n >= 0 is guaranteed by the branch above")]
    buf.advance(n as usize);
    Ok(())
}

/// Attempt a single non-blocking `write(2)` of `buf` to `fd`.
///
/// Returns the number of bytes written, or `WouldBlock` when the kernel
/// send buffer is full.
///
/// # Errors
///
/// Returns an [`io::Error`] on any `write(2)` failure.
fn try_write(fd: &OwnedFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: `fd` is a valid, open PTY master fd. `buf` is a live, readable
    // slice of the declared length. The pointer cast from `*const u8` to
    // `*const libc::c_void` is safe because `c_void` has no alignment
    // requirement that `u8` does not also satisfy.
    let n = unsafe { libc::write(fd.as_raw_fd(), buf.as_ptr().cast::<libc::c_void>(), buf.len()) };

    if n < 0 {
        return Err(io::Error::last_os_error());
    }

    #[allow(clippy::cast_sign_loss, reason = "n >= 0 is guaranteed by the branch above")]
    Ok(n as usize)
}

impl AsyncRead for AsyncPtyFd {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = match self.inner.poll_read_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };

            match try_read(self.inner.get_ref(), buf) {
                Ok(()) => return Poll::Ready(Ok(())),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    guard.clear_ready();
                    // Loop back to re-register waker via poll_read_ready.
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

impl AsyncWrite for AsyncPtyFd {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = match self.inner.poll_write_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };

            match try_write(self.inner.get_ref(), buf) {
                Ok(n) => return Poll::Ready(Ok(n)),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    guard.clear_ready();
                    // Loop back to re-register waker via poll_write_ready.
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }

    /// PTY master fds have no user-space write buffer; flush is a no-op.
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// The fd is closed when [`AsyncPtyFd`] is dropped; no explicit shutdown needed.
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Wrap a raw file descriptor received via `SCM_RIGHTS` into an [`OwnedFd`].
///
/// # Safety guarantee
///
/// This function encapsulates the `unsafe` call to [`OwnedFd::from_raw_fd`] so
/// that callers in crates that `deny(unsafe_code)` can use it safely. The caller
/// must ensure that `fd` is a valid, open file descriptor that is not owned by
/// any other `OwnedFd` or `File`.
///
/// `scribe-pty` already allows `unsafe_code` for PTY I/O, so the `unsafe` lives
/// here rather than leaking into `scribe-server`.
pub fn wrap_raw_fd(fd: RawFd) -> OwnedFd {
    // SAFETY: The caller guarantees that `fd` is a valid, open file descriptor
    // received from `recvmsg` via `SCM_RIGHTS`. The kernel duplicated the fd
    // during `sendmsg`, so this process owns it exclusively.
    unsafe { OwnedFd::from_raw_fd(fd) }
}
