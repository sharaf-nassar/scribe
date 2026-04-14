use std::io;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
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
}

/// Set the `O_NONBLOCK` flag on `fd` via `fcntl(F_GETFL)` / `fcntl(F_SETFL)`.
///
/// # Errors
///
/// Returns an [`io::Error`] if either `fcntl` call fails.
fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let flags = fcntl_getfl(fd).map_err(io::Error::from)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(io::Error::from)
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
    let n = rustix::io::read(fd, unfilled).map_err(io::Error::from)?;
    buf.advance(n);
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
    rustix::io::write(fd, buf).map_err(io::Error::from)
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
