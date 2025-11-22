use pin_project_lite::pin_project;
use std::io::{IoSlice, Result};
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::unix::pipe::{Receiver, Sender};

/// An object repsenting the redirection of stdio for a jailed process.
///
/// An instance of [`Stdio`] can be passed to
/// [`Command::stdin`](super::Command::stdin),
/// [`Command::stdout`](super::Command::stdout),
/// or [`Command::stderr`](super::Command::stderr) in order to redirect
/// the corresponding stdio stream.
#[derive(Debug)]
pub struct Stdio {
    pub(super) inner: StdioInner,
}

#[derive(Debug)]
pub(super) enum StdioInner {
    Inherit,
    Pipe,
    Null,
    Fd(OwnedFd),
    TxPipe(Sender),
    RxPipe(Receiver),
}

pin_project! {
    /// A writable pipe to a jailed process's `stdin`
    #[derive(Debug)]
    pub struct ChildStdin {
        #[pin]
        pub(super) pipe: Sender,
    }
}

pin_project! {
    /// A readable pipe from a jailed process's `stdout`
    #[derive(Debug)]
    pub struct ChildStdout {
        #[pin]
        pub(super) pipe: Receiver,
    }
}

pin_project! {
    /// A readable pipe from a jailed process's `stderr`
    #[derive(Debug)]
    pub struct ChildStderr {
        #[pin]
        pub(super) pipe: Receiver,
    }
}

impl Stdio {
    /// Constructs a [`Stdio`] that will cause a jailed process to inherit stdio from the parent.
    ///
    /// In this case, stdio will not be redirected at all and the jailed process will share
    /// `stdin`, `stdout`, or `stderr`, respectively, with the parent process.
    pub fn inherit() -> Self {
        Self {
            inner: StdioInner::Inherit,
        }
    }

    /// Constructs a [`Stdio`] that will cause a jailed process redirect stdio to a pipe.
    ///
    /// In this case, stdio of the jailed process will become available to the caller via an
    /// [`ChildStdin`], [`ChildStdout`], or [`ChildStderr`] instance.
    pub fn piped() -> Self {
        Self {
            inner: StdioInner::Pipe,
        }
    }

    /// Constructs a [`Stdio`] that will cause a jailed process to drop stdio input or output.
    ///
    /// In this case, stdio of the jailed process will become no-ops. The stream is effectively
    /// redirected to `/dev/null`.
    pub fn null() -> Self {
        Self {
            inner: StdioInner::Null,
        }
    }

    pub(super) fn into_stdio(self) -> Result<std::process::Stdio> {
        Ok(match self.inner {
            StdioInner::Null | StdioInner::Pipe => std::process::Stdio::null(),
            StdioInner::Inherit => std::process::Stdio::inherit(),
            StdioInner::Fd(fd) => std::process::Stdio::from(fd),
            StdioInner::RxPipe(pipe) => std::process::Stdio::from(pipe.into_blocking_fd()?),
            StdioInner::TxPipe(pipe) => std::process::Stdio::from(pipe.into_blocking_fd()?),
        })
    }
}

impl From<OwnedFd> for Stdio {
    fn from(fd: OwnedFd) -> Self {
        Self {
            inner: StdioInner::Fd(fd),
        }
    }
}

impl From<std::fs::File> for Stdio {
    fn from(f: std::fs::File) -> Self {
        Self {
            inner: StdioInner::Fd(f.into()),
        }
    }
}

impl From<std::io::PipeReader> for Stdio {
    fn from(pipe: std::io::PipeReader) -> Self {
        Self {
            inner: StdioInner::Fd(pipe.into()),
        }
    }
}

impl From<std::io::PipeWriter> for Stdio {
    fn from(pipe: std::io::PipeWriter) -> Self {
        Self {
            inner: StdioInner::Fd(pipe.into()),
        }
    }
}

impl From<Sender> for Stdio {
    fn from(pipe: Sender) -> Self {
        Self {
            inner: StdioInner::TxPipe(pipe),
        }
    }
}

impl From<Receiver> for Stdio {
    fn from(pipe: Receiver) -> Self {
        Self {
            inner: StdioInner::RxPipe(pipe),
        }
    }
}

impl From<std::process::ChildStdin> for Stdio {
    fn from(stdin: std::process::ChildStdin) -> Self {
        Self {
            inner: StdioInner::Fd(stdin.into()),
        }
    }
}

impl From<std::process::ChildStdout> for Stdio {
    fn from(stdout: std::process::ChildStdout) -> Self {
        Self {
            inner: StdioInner::Fd(stdout.into()),
        }
    }
}

impl From<std::process::ChildStderr> for Stdio {
    fn from(stderr: std::process::ChildStderr) -> Self {
        Self {
            inner: StdioInner::Fd(stderr.into()),
        }
    }
}

impl From<ChildStdin> for Stdio {
    fn from(stdin: ChildStdin) -> Self {
        Self {
            inner: StdioInner::TxPipe(stdin.pipe),
        }
    }
}

impl From<ChildStdout> for Stdio {
    fn from(stdout: ChildStdout) -> Self {
        Self {
            inner: StdioInner::RxPipe(stdout.pipe),
        }
    }
}

impl From<ChildStderr> for Stdio {
    fn from(stderr: ChildStderr) -> Self {
        Self {
            inner: StdioInner::RxPipe(stderr.pipe),
        }
    }
}

impl std::os::fd::AsFd for ChildStdin {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.pipe.as_fd()
    }
}

impl std::os::fd::AsFd for ChildStdout {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.pipe.as_fd()
    }
}

impl std::os::fd::AsFd for ChildStderr {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.pipe.as_fd()
    }
}

impl std::os::fd::AsRawFd for ChildStdin {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.pipe.as_raw_fd()
    }
}

impl std::os::fd::AsRawFd for ChildStdout {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.pipe.as_raw_fd()
    }
}

impl std::os::fd::AsRawFd for ChildStderr {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.pipe.as_raw_fd()
    }
}

impl AsyncWrite for ChildStdin {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        self.project().pipe.poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        self.project().pipe.poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.pipe.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.project().pipe.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.project().pipe.poll_shutdown(cx)
    }
}

impl AsyncRead for ChildStdout {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        self.project().pipe.poll_read(cx, buf)
    }
}

impl AsyncRead for ChildStderr {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        self.project().pipe.poll_read(cx, buf)
    }
}
