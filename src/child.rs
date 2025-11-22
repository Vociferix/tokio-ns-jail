use super::stdio::{ChildStderr, ChildStdin, ChildStdout};
use nix::unistd::Pid;
use std::io::Result;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

#[derive(Debug)]
pub struct Child {
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    pub(super) pid: Pid,
    pub(super) child_pid: Pid,
    pub(super) pipe: tokio::net::UnixStream,
    pub(super) buf: [u8; 5],
    pub(super) len: u8,
    pub(super) status: Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Exited(i32),
    Signaled(i32, bool),
    Stopped(i32),
    Continued,
    StillAlive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExitStatus {
    inner: InnerExitStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum InnerExitStatus {
    Exited(i32),
    Signaled(i32, bool),
}

#[derive(Debug, Clone)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Child {
    pub fn send_signal(&self, signal: i32) -> std::io::Result<()> {
        let signal = nix::sys::signal::Signal::try_from(signal).map_err(std::io::Error::from)?;
        nix::sys::signal::kill(self.child_pid, signal).map_err(std::io::Error::from)
    }

    pub fn kill(&self) -> std::io::Result<()> {
        self.send_signal(nix::sys::signal::Signal::SIGKILL as i32)
    }

    pub fn id(&self) -> u32 {
        self.pid.as_raw() as u32
    }

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
                self.status = Status::StillAlive;
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

    pub fn status(&self) -> Status {
        self.status
    }

    pub async fn next_event(&mut self) -> std::io::Result<Status> {
        std::future::poll_fn(move |cx| self.poll_next_event(cx)).await
    }

    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        std::future::poll_fn(move |cx| self.poll_wait(cx)).await
    }

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
    pub fn success(&self) -> bool {
        matches!(self.inner, InnerExitStatus::Exited(0))
    }

    pub fn code(&self) -> Option<i32> {
        if let InnerExitStatus::Exited(code) = self.inner {
            Some(code)
        } else {
            None
        }
    }

    pub fn signal(&self) -> Option<i32> {
        if let InnerExitStatus::Signaled(sig, _) = self.inner {
            Some(sig)
        } else {
            None
        }
    }

    pub fn core_dumped(&self) -> bool {
        matches!(self.inner, InnerExitStatus::Signaled(_, true))
    }
}

impl Status {
    pub fn exited(&self) -> bool {
        matches!(self, &Self::Exited(_))
    }

    pub fn signaled(&self) -> bool {
        matches!(self, &Self::Signaled(_, _))
    }

    pub fn core_dumped(&self) -> bool {
        matches!(*self, Self::Signaled(_, true))
    }

    pub fn stopped(&self) -> bool {
        matches!(self, &Self::Stopped(_))
    }

    pub fn continued(&self) -> bool {
        matches!(self, &Self::Continued)
    }

    pub fn running(&self) -> bool {
        matches!(self, &Self::Continued | &Self::StillAlive)
    }

    pub fn alive(&self) -> bool {
        matches!(&self, &Self::StillAlive)
    }

    pub fn code(&self) -> Option<i32> {
        if let &Self::Exited(code) = self {
            Some(code)
        } else {
            None
        }
    }

    pub fn signal(&self) -> Option<i32> {
        if let &Self::Signaled(sig, _) = self {
            Some(sig)
        } else {
            None
        }
    }

    pub fn stopped_signal(&self) -> Option<i32> {
        if let &Self::Stopped(sig) = self {
            Some(sig)
        } else {
            None
        }
    }
}
