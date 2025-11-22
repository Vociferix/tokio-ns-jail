use super::stdio::{ChildStderr, ChildStdin, ChildStdout};
use nix::unistd::Pid;
use std::io::Result;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

/// A handle to a jailed subprocess.
///
/// A [`Child`] provides an interface into a jailed process spawned by
/// [`Command::spawn`](super::Command::spawn). It provides access to
/// piped stdio, if any, allows sending signals to the jailed process,
/// and provides methods to monitor the status of the jailed process.
#[derive(Debug)]
pub struct Child {
    /// Writable pipe to the jailed process's `stdin`.
    ///
    /// This will be [`None`] if the process's `stdin` was not configured to be piped.
    pub stdin: Option<ChildStdin>,

    /// Readable pipe from the jailed process's `stdout`.
    ///
    /// This will be [`None`] if the process's `stdout` was not configured to be piped.
    pub stdout: Option<ChildStdout>,

    /// Readable pipe from the jailed process's `stderr`.
    ///
    /// This will be [`None`] if the process's `stderr` was not configured to be piped.
    pub stderr: Option<ChildStderr>,

    pub(super) pid: Pid,
    pub(super) child_pid: Pid,
    pub(super) pipe: tokio::net::UnixStream,
    pub(super) buf: [u8; 5],
    pub(super) len: u8,
    pub(super) status: Status,
}

/// An object representing status of a jailed process
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    /// The process has terminated normally with the contained exit code
    Exited(i32),

    /// The process was killed by the contained signal.
    ///
    /// The second field of this variant denotes whether the signal
    /// resulted in a core dump.
    Signaled(i32, bool),

    /// The process was stopped by the contained signal.
    ///
    /// A stopped process can be resumed by sending the `SIGCONT` signal.
    Stopped(i32),

    /// The process was continued after previously being stopped
    Continued,

    /// The process is currently running
    Running,
}

/// An object representing the exit condition of a terminated jailed process
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExitStatus {
    inner: InnerExitStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum InnerExitStatus {
    Exited(i32),
    Signaled(i32, bool),
}

/// Exit status and stdio data of a terminated jailed process
#[derive(Debug, Clone)]
pub struct Output {
    /// The [`ExitStatus`] for the jailed process
    pub status: ExitStatus,

    /// The `stdout` data of the jailed process.
    ///
    /// This will be empty unless `stdout` was piped.
    pub stdout: Vec<u8>,

    /// The `stderr` data of the jailed process.
    ///
    /// This will be empty unless `sterr` was piped.
    pub stderr: Vec<u8>,
}

impl Child {
    /// Sends the provided signal to the jailed process
    pub fn send_signal(&self, signal: i32) -> std::io::Result<()> {
        let signal = nix::sys::signal::Signal::try_from(signal).map_err(std::io::Error::from)?;
        nix::sys::signal::kill(self.child_pid, signal).map_err(std::io::Error::from)
    }

    /// Sends `SIGKILL` to the jailed process.
    ///
    /// This method does not wait for the signal to take effect.
    /// [`child.wait()`](Self::wait) can be called to wait for the
    /// jailed process to fully terminate.
    pub fn kill(&self) -> std::io::Result<()> {
        self.send_signal(nix::sys::signal::Signal::SIGKILL as i32)
    }

    /// Returns the host PID of the jailed process.
    ///
    /// Note that due to quirks of Linux namespaces, two subprocesses are
    /// associated with a jailed process. The PID returned by this function
    /// is for the control process which manages the process executing the
    /// [`Command`](super::Command).
    pub fn id(&self) -> u32 {
        self.pid.as_raw() as u32
    }

    /// Polls for the next event, or change in status, for the jailed process.
    ///
    /// See [`next_event`](Self::next_event) for more details.
    pub fn poll_next_event(&mut self, cx: &mut Context<'_>) -> Poll<Result<Status>> {
        match self.status {
            status @ Status::Exited(_) => return Poll::Ready(Ok(status)),
            status @ Status::Signaled(_, _) => return Poll::Ready(Ok(status)),
            _ => {}
        }

        let mut tmpbuf = self.buf;
        loop {
            let cnt = {
                let mut buf = ReadBuf::new(&mut tmpbuf[(self.len as usize)..]);
                let old_rem = buf.remaining();
                match Pin::new(&mut self.pipe).poll_read(cx, &mut buf) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Ready(Ok(())) => {}
                }
                old_rem - buf.remaining()
            };
            let new_len = self.len as usize + cnt;
            if new_len == 5 {
                self.len = 0;
                break;
            }
            self.len = new_len as u8;
            self.buf = tmpbuf;
        }

        let kind = tmpbuf[0];
        let code = i32::from_ne_bytes([tmpbuf[1], tmpbuf[2], tmpbuf[3], tmpbuf[4]]);

        Poll::Ready(Ok(match kind {
            0 => {
                self.status = Status::Exited(code);
                Status::Exited(code)
            }
            1 => {
                self.status = Status::Signaled(code, false);
                Status::Signaled(code, false)
            }
            2 => {
                self.status = Status::Signaled(code, true);
                Status::Signaled(code, true)
            }
            3 => {
                self.status = Status::Stopped(code);
                Status::Stopped(code)
            }
            4 => {
                self.status = Status::Running;
                Status::Continued
            }
            5 => {
                return Poll::Ready(Err(std::io::Error::from_raw_os_error(code)));
            }
            _ => {
                return Poll::Ready(Err(std::io::ErrorKind::InvalidData.into()));
            }
        }))
    }

    /// Polls for the termination of the jailed process.
    ///
    /// See [`wait`](Self::wait) for more details.
    pub fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<Result<ExitStatus>> {
        match self.poll_next_event(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Ready(Ok(Status::Exited(code))) => Poll::Ready(Ok(ExitStatus {
                inner: InnerExitStatus::Exited(code),
            })),
            Poll::Ready(Ok(Status::Signaled(sig, dumped))) => Poll::Ready(Ok(ExitStatus {
                inner: InnerExitStatus::Signaled(sig, dumped),
            })),
            _ => Poll::Pending,
        }
    }

    /// Gets the current status of the jailed process.
    ///
    /// This function does not block. To wait for a change in status, call
    /// [`next_event`](Self::next_event).
    pub fn status(&mut self) -> Result<Status> {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        if let Poll::Ready(res) = self.poll_next_event(&mut cx) {
            return res;
        }
        Ok(self.status)
    }

    /// Waits for the next change in status of the jailed process.
    ///
    /// The returned status may represent termination, suspension, or continuation
    /// of the process. To wait strictly for termination of the process, call
    /// [`wait`](Self::wait), or to check the current status without waiting, call
    /// [`status`](Self::status).
    ///
    /// This function is most useful when the jailed process may be stopped and
    /// continued by the host process.
    pub async fn next_event(&mut self) -> std::io::Result<Status> {
        std::future::poll_fn(move |cx| self.poll_next_event(cx)).await
    }

    /// Waits for the jailed process to terminate and returns the [`ExitStatus`].
    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        std::future::poll_fn(move |cx| self.poll_wait(cx)).await
    }

    /// Waits for the jailed process to terminate and returns the [`Output`].
    ///
    /// This function is similar to [`wait`](Self::wait), but the `stdout` and
    /// `stderr` output are also returned with the [`ExitStatus`]. However,
    /// the stdio data will only be available if the corresponding stream is
    /// piped via [`Stdio::piped()`](super::Stdio::piped).
    pub async fn wait_with_output(&mut self) -> std::io::Result<Output> {
        let (stdout, stderr) = match (self.stdout.take(), self.stderr.take()) {
            (None, None) => (Vec::new(), Vec::new()),
            (Some(mut stdout), None) => {
                let mut data = Vec::new();
                stdout.read_to_end(&mut data).await?;
                (data, Vec::new())
            }
            (None, Some(mut stderr)) => {
                let mut data = Vec::new();
                stderr.read_to_end(&mut data).await?;
                (data, Vec::new())
            }
            (Some(mut stdout), Some(mut stderr)) => {
                let mut stdout_data = Vec::new();
                let mut stderr_data = Vec::new();
                tokio::try_join!(
                    stdout.read_to_end(&mut stdout_data),
                    stderr.read_to_end(&mut stderr_data),
                )?;
                (stdout_data, stderr_data)
            }
        };

        let status = self.wait().await?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

impl ExitStatus {
    /// Returns whether the jailed process exited normally with 0 exit code
    pub fn success(&self) -> bool {
        matches!(self.inner, InnerExitStatus::Exited(0))
    }

    /// Returns the exit code of the jailed process if it terminated normally.
    ///
    /// This will return [`None`] if the process was killed by a signal.
    pub fn code(&self) -> Option<i32> {
        if let InnerExitStatus::Exited(code) = self.inner {
            Some(code)
        } else {
            None
        }
    }

    /// Returns the signal that killed the jailed process if it did not terminate normally.
    ///
    /// This will return [`None`] if the process terminated normally.
    pub fn signal(&self) -> Option<i32> {
        if let InnerExitStatus::Signaled(sig, _) = self.inner {
            Some(sig)
        } else {
            None
        }
    }

    /// Returns `true` if the jailed process was killed by a signal and resulted in a core dump.
    pub fn core_dumped(&self) -> bool {
        matches!(self.inner, InnerExitStatus::Signaled(_, true))
    }
}

impl Status {
    /// Returns `true` if the process terminated normally with an exit code
    pub fn exited(&self) -> bool {
        matches!(self, &Self::Exited(_))
    }

    /// Returns `true` if the process was killed by a signal
    pub fn signaled(&self) -> bool {
        matches!(self, &Self::Signaled(_, _))
    }

    /// Returns `true` if the process was killed by a signal and resulted in a core dump
    pub fn core_dumped(&self) -> bool {
        matches!(*self, Self::Signaled(_, true))
    }

    /// Returns `true` if the process was stopped by a signal
    pub fn stopped(&self) -> bool {
        matches!(self, &Self::Stopped(_))
    }

    /// Returns `true` if the process was continued after being stopped
    pub fn continued(&self) -> bool {
        matches!(self, &Self::Continued)
    }

    /// Returns `true` if the process is currently running normally.
    ///
    /// Running normally means that the process has not terminated and has not been
    /// stopped.
    pub fn running(&self) -> bool {
        matches!(self, &Self::Continued | &Self::Running)
    }

    /// Gets the exit code of the process if it terminated normally
    pub fn code(&self) -> Option<i32> {
        if let &Self::Exited(code) = self {
            Some(code)
        } else {
            None
        }
    }

    /// Gets the signal that killed the process, if any
    pub fn signal(&self) -> Option<i32> {
        if let &Self::Signaled(sig, _) = self {
            Some(sig)
        } else {
            None
        }
    }

    /// Gets the signal that stopped the process, if any
    pub fn stopped_signal(&self) -> Option<i32> {
        if let &Self::Stopped(sig) = self {
            Some(sig)
        } else {
            None
        }
    }
}
