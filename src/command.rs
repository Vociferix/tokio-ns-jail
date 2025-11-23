use super::child::{Child, ExitStatus, Output};
use super::id_range::{GidRange, UidRange};
use super::stdio::{ChildStderr, ChildStdin, ChildStdout, Stdio, StdioInner};
use nix::mount::{MsFlags, mount};
use nix::sched::CloneFlags;
use nix::unistd::{Gid, Pid, Uid};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{Read, Result, Write};
use std::os::fd::RawFd;
use std::path::{Component, Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe::{Receiver, Sender};

/// Builder for constructing a jailed process.
///
/// [`Command`] is modeled after [`std::process::Command`]. However, it
/// provides additional options specific to Linux namespace jailing.
///
/// A jailed process runs in an isolated environment, typically for the
/// purpose of protecting the host environment from effects of the
/// process to be jailed. It is also useful for running a command in
/// an easily reproduceable environment across distinct systems.
#[derive(Debug)]
pub struct Command {
    rootfs: Option<PathBuf>,
    ro_root: bool,
    mounts: Vec<Mount>,
    uid_map: Vec<(UidRange, UidRange)>,
    gid_map: Vec<(GidRange, GidRange)>,
    unshare: Namespace,

    program: OsString,
    args: Vec<OsString>,
    env: HashMap<OsString, OsString>,
    curr_dir: Option<PathBuf>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

bitflags::bitflags! {
    /// Bit flags representing Linux namespaces.
    ///
    /// The namespaces represented here can be passed to a [`Command`] to
    /// unshare those namespaces in the jail, where unsharing means that
    /// the system information and resources associated with the namespace
    /// will be separate and isolated from the corresponding resources on
    /// the host system.
    ///
    /// Note that the mount and user namespaces are not represented by
    /// [`Namespace`]. These namespaces unshared conditionally by
    /// [`Command`] because they are required for the jail to operate. As
    /// such, they are not included in [`Namespace`], since they would
    /// serve no purpose.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Namespace: std::ffi::c_int {
        const CGROUP = CloneFlags::CLONE_NEWCGROUP.bits();
        const UTS = CloneFlags::CLONE_NEWUTS.bits();
        const IPC = CloneFlags::CLONE_NEWIPC.bits();
        const PID = CloneFlags::CLONE_NEWPID.bits();
        const NET = CloneFlags::CLONE_NEWNET.bits();
    }
}

#[derive(Debug)]
enum MountSrc {
    Bind(PathBuf),
    DevFs,
    ProcFs,
    SysFs,
    TmpFs,
}

#[derive(Debug)]
struct Mount {
    pub src: MountSrc,
    pub dst: PathBuf,
    pub writable: bool,
}

impl Command {
    /// Creates a new [`Command`] that will run `program` in the jail.
    ///
    /// `program` must correspond to an executable inside the jail.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            rootfs: None,
            ro_root: true,
            mounts: Vec::new(),
            uid_map: Vec::new(),
            gid_map: Vec::new(),
            unshare: Namespace::empty(),
            program: program.as_ref().into(),
            args: vec![program.as_ref().into()],
            env: HashMap::new(),
            curr_dir: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    /// Sets the root filesystem for the jail.
    ///
    /// `path` must be a host system path to a directory. If not provided,
    /// the current working directory will be used.
    pub fn rootfs(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.rootfs = Some(path.as_ref().into());
        self
    }

    /// Sets whether the root filesystem is writable.
    ///
    /// By default, the root filesystem is mounted in read-only mode.
    pub fn writable_rootfs(&mut self, writable: bool) -> &mut Self {
        self.ro_root = !writable;
        self
    }

    /// Specifies a directory to be bind mounted in the jail.
    ///
    /// `host_path` must be a path to directory on the host system.
    /// `jail_path` will be create inside the jail if it doesn't already
    /// exist. `writable` specifies whether the mounted directory
    /// will be writable from within the jail.
    pub fn bind_mount(
        &mut self,
        host_path: impl AsRef<Path>,
        jail_path: impl AsRef<Path>,
        writable: bool,
    ) -> &mut Self {
        self.mounts.push(Mount {
            src: MountSrc::Bind(host_path.as_ref().to_path_buf()),
            dst: jail_path.as_ref().to_path_buf(),
            writable,
        });
        self
    }

    /// Specifies a path within the jail to mount a pseudo-devfs filesystem.
    ///
    /// Note that the mounted devfs filesystem will not be a true `devfs` or
    /// `devtmpfs`. Instead, a mock devfs filesystem will be created including
    /// commonly needed utilities such as `/dev/null` and `/dev/urandom`.
    ///
    /// Full listing of the contents of the pseudo-devfs filesystem:
    /// * `null`
    /// * `full`
    /// * `zero`
    /// * `random`
    /// * `urandom`
    /// * `tty`
    /// * `stdin`
    /// * `stdout`
    /// * `stderr`
    /// * `fd`
    /// * `core`
    /// * `shm`
    /// * `pts`
    /// * `ptmx`
    /// * `console`*
    ///
    /// *`console` is only included if stdio is a TTY.
    ///
    /// Some resources in the pseudo-devfs may require a procfs mount to
    /// function properly.
    pub fn devfs_mount(&mut self, jail_path: impl AsRef<Path>) -> &mut Self {
        self.mounts.push(Mount {
            src: MountSrc::DevFs,
            dst: jail_path.as_ref().to_path_buf(),
            writable: true,
        });
        self
    }

    /// Specifies a path within the jail to mount a procfs filesystem.
    pub fn procfs_mount(&mut self, jail_path: impl AsRef<Path>) -> &mut Self {
        self.mounts.push(Mount {
            src: MountSrc::ProcFs,
            dst: jail_path.as_ref().to_path_buf(),
            writable: true,
        });
        self
    }

    /// Specifies a path within the jail to mount a sysfs filesystem.
    pub fn sysfs_mount(&mut self, jail_path: impl AsRef<Path>) -> &mut Self {
        self.mounts.push(Mount {
            src: MountSrc::SysFs,
            dst: jail_path.as_ref().to_path_buf(),
            writable: true,
        });
        self
    }

    /// Specifies a path within the jail to mount a tmpfs filesystem.
    pub fn tmpfs_mount(&mut self, jail_path: impl AsRef<Path>) -> &mut Self {
        self.mounts.push(Mount {
            src: MountSrc::TmpFs,
            dst: jail_path.as_ref().to_path_buf(),
            writable: true,
        });
        self
    }

    /// Maps a range of jail UIDs to a range of host UIDs.
    ///
    /// Typically, it is desired to map the caller's UID to UID 0 (root),
    /// which allows the process operate as if it were running as root
    /// within the jail. However, any UID can be mapped to any other UID
    /// as desired.
    ///
    /// The `host_uids` range must be the same size as the `jail_uids`
    /// range.
    ///
    /// Multiple range pairs can be specified by calling this function
    /// multiple times, but it is required for no single host or jail
    /// UID to be specified more than once.
    pub fn map_uids(
        &mut self,
        host_uids: impl Into<UidRange>,
        jail_uids: impl Into<UidRange>,
    ) -> &mut Self {
        let src = host_uids.into();
        let dst = jail_uids.into();
        if !src.is_empty() {
            self.uid_map.push((src, dst));
        }
        self
    }

    /// Maps a range of jail GIDs to a range of host GIDs.
    ///
    /// Typically, it is desired to map the caller's GID to GID 0 (root).
    /// However, any GID can be mapped to any other GID as desired.
    ///
    /// The `host_gids` range must be the same size as the `jail_gids`
    /// range.
    ///
    /// Multiple range pairs can be specified by calling this function
    /// multiple times, but it is required for no single host or jail
    /// GID to be specified more than once.
    pub fn map_gids(
        &mut self,
        host_gids: impl Into<GidRange>,
        jail_gids: impl Into<GidRange>,
    ) -> &mut Self {
        let src = host_gids.into();
        let dst = jail_gids.into();
        if !src.is_empty() {
            self.gid_map.push((src, dst));
        }
        self
    }

    /// Maps the current host UID and GID to 0 (root) in the jail.
    ///
    /// This function is a convenience wrapper around
    /// [`map_uids`](Self::map_uids) and [`map_gids`](Self::map_gids)
    /// to map the current effective UID and GID to 0.
    pub fn user_as_root(&mut self) -> &mut Self {
        self.map_uids(Uid::effective(), 0u32);
        self.map_gids(Gid::effective(), 0u32);
        self
    }

    /// Unshares a set of namespaces in the jail.
    ///
    /// See [`Namespace`] for the effect of unsharing each namespace.
    pub fn unshare(&mut self, namespaces: Namespace) -> &mut Self {
        self.unshare = namespaces;
        self
    }

    /// Appends an argument to be passed to the program executed in the jail
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().into());
        self
    }

    /// Appends multiple arguments to be passed to the program executed in the jail
    pub fn args<I>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator,
        I::Item: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().into()));
        self
    }

    /// Defines an environment variable to be set in the jail.
    ///
    /// Unlike [`std::process::Command`], the host environment is not captured
    /// automatically, to ensure host information is not unintentionally leaked
    /// into the jail.
    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.env.insert(key.as_ref().into(), val.as_ref().into());
        self
    }

    /// Defines multiple environment variables to be set in the jail.
    ///
    /// Unlike [`std::process::Command`], the host environment is not captured
    /// automatically, to ensure host information is not unintentionally leaked
    /// into the jail.
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.env.extend(
            vars.into_iter()
                .map(|(key, val)| (key.as_ref().into(), val.as_ref().into())),
        );
        self
    }

    /// Specifies a directory within the jail to execute the program from.
    ///
    /// `path` must be an existing path within the jail. It will be the
    /// current working directory when the jailed process starts.
    pub fn current_dir(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.curr_dir = Some(path.as_ref().into());
        self
    }

    /// Specifies the redirection of `stdin` for the jailed process.
    ///
    /// `stdin` can be redirected to an I/O object, piped, inherited
    /// from the host process, or redirected to `/dev/null`. If not set,
    /// `stdin` is inherited if [`spawn`](Self::spawn) or
    /// [`status`](Self::status) is called, and is piped if
    /// [`output`](Self::output) is called.
    ///
    /// See [`Stdio`] for more details.
    pub fn stdin(&mut self, cfg: impl Into<Option<Stdio>>) -> &mut Self {
        self.stdin = cfg.into();
        self
    }

    /// Specifies the redirection of `stdout` for the jailed process.
    ///
    /// `stdout` can be redirected to an I/O object, piped, inherited
    /// from the host process, or redirected to `/dev/null`. If not set,
    /// `stdout` is inherited if [`spawn`](Self::spawn) or
    /// [`status`](Self::status) is called, and is piped if
    /// [`output`](Self::output) is called.
    ///
    /// See [`Stdio`] for more details.
    pub fn stdout(&mut self, cfg: impl Into<Option<Stdio>>) -> &mut Self {
        self.stdout = cfg.into();
        self
    }

    /// Specifies the redirection of `stderr` for the jailed process.
    ///
    /// `stderr` can be redirected to an I/O object, piped, inherited
    /// from the host process, or redirected to `/dev/null`. If not set,
    /// `stderr` is inherited if [`spawn`](Self::spawn) or
    /// [`status`](Self::status) is called, and is piped if
    /// [`output`](Self::output) is called.
    ///
    /// See [`Stdio`] for more details.
    pub fn stderr(&mut self, cfg: impl Into<Option<Stdio>>) -> &mut Self {
        self.stderr = cfg.into();
        self
    }

    /// Overrides the first argument as seen by the program.
    pub fn arg0(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args[0] = arg.as_ref().into();
        self
    }

    /// Starts the jailed process and returns a handle to it.
    ///
    /// The returned future completes immediately after the jailed process
    /// is started and does not wait for it to terminate.
    pub async fn spawn(&mut self) -> Result<Child> {
        self.spawn_impl(Stdio::inherit()).await
    }

    /// Runs the jailed process and returns the [`ExitStatus`] and stdio output.
    ///
    /// The returned future does not complete until the jailed process terminates.
    pub async fn output(&mut self) -> Result<Output> {
        self.spawn_impl(Stdio::piped())
            .await?
            .wait_with_output()
            .await
    }

    /// Runs the jailed process and returns the [`ExitStatus`].
    ///
    /// The returned future does not complete until the jailed process terminates.
    pub async fn status(&mut self) -> Result<ExitStatus> {
        self.spawn_impl(Stdio::inherit()).await?.wait().await
    }
}

impl Command {
    async fn write_uid_map(&self, pid: Pid) -> Result<()> {
        if self.uid_map.is_empty() {
            return Ok(());
        }

        let mut map = String::new();

        for (src_range, dst_range) in &self.uid_map {
            if let (Some(src), Some(dst)) = (src_range.start(), dst_range.start()) {
                writeln!(map, "{} {} {}", dst.as_raw(), src.as_raw(), src_range.len(),).unwrap();
            }
        }

        let path = format!("/proc/{}/uid_map", pid.as_raw());
        tokio::fs::write(path, map).await?;
        Ok(())
    }

    async fn write_gid_map(&self, pid: Pid) -> Result<()> {
        if self.gid_map.is_empty() {
            return Ok(());
        }

        let mut map = String::new();

        for (src_range, dst_range) in &self.gid_map {
            if let (Some(src), Some(dst)) = (src_range.start(), dst_range.start()) {
                writeln!(map, "{} {} {}", dst.as_raw(), src.as_raw(), src_range.len(),).unwrap();
            }
        }

        let path = format!("/proc/{}/setgroups", pid.as_raw());
        tokio::fs::write(path, "deny").await?;

        let path = format!("/proc/{}/gid_map", pid.as_raw());
        tokio::fs::write(path, map).await?;

        Ok(())
    }

    async fn spawn_impl(&mut self, stdio_default: Stdio) -> Result<Child> {
        use std::os::fd::IntoRawFd;

        const STACK_SIZE: usize = 16 * 1024;

        let (mut pipe, child_pipe) = tokio::net::UnixStream::pair()?;
        let (child_stdin, stdin) = self.child_stdio(&stdio_default)?;
        let (stdout, child_stdout) = self.child_stdio(&stdio_default)?;
        let (stderr, child_stderr) = self.child_stdio(&stdio_default)?;

        let child_pipe = child_pipe.into_std()?;
        child_pipe.set_nonblocking(false)?;
        let child_pipe = child_pipe.into_raw_fd();

        let stdin = stdin
            .map(|stdin| stdin.into_nonblocking_fd())
            .transpose()?
            .map(|stdin| stdin.into_raw_fd());
        let stdout = stdout
            .map(|stdout| stdout.into_blocking_fd())
            .transpose()?
            .map(|stdout| stdout.into_raw_fd());
        let stderr = stderr
            .map(|stderr| stderr.into_blocking_fd())
            .transpose()?
            .map(|stderr| stderr.into_raw_fd());

        let mut stack: Vec<u8> = vec![0; STACK_SIZE];

        let flags = unsafe { CloneFlags::from_bits(self.unshare.bits()).unwrap_unchecked() };

        let pid = unsafe {
            let cmd = &mut *self;
            nix::sched::clone(
                Box::new(move || -> isize { cmd.run_child(child_pipe, stdin, stdout, stderr) }),
                &mut stack,
                flags | CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUSER,
                None,
            )
            .map_err(std::io::Error::from)?
        };

        match self.write_ids_maps(pid).await {
            Ok(fd) => fd,
            Err(err) => {
                let _ = pipe.write_u8(1u8).await;
                return Err(err);
            }
        }
        pipe.write_u8(0u8).await?;

        let err = pipe.read_i32().await?;
        if err != 0 {
            return Err(std::io::Error::from_raw_os_error(err));
        }

        let child_pid = Self::get_child_pid(pid).await?;

        Ok(Child {
            stdin: child_stdin.map(|pipe| ChildStdin { pipe }),
            stdout: child_stdout.map(|pipe| ChildStdout { pipe }),
            stderr: child_stderr.map(|pipe| ChildStderr { pipe }),
            pid,
            child_pid,
            pipe,
            buf: [0u8; 5],
            len: 0,
            status: super::child::Status::Running,
        })
    }

    async fn get_child_pid(pid: Pid) -> Result<Pid> {
        let pid = tokio::fs::read(format!("/proc/{pid}/task/{pid}/children")).await?;
        let pid = match String::from_utf8(pid) {
            Ok(pid) => pid,
            Err(err) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
        };
        let pid = match pid.trim().parse::<i32>() {
            Ok(pid) => pid,
            Err(err) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
        };
        Ok(Pid::from_raw(pid))
    }

    async fn write_ids_maps(&mut self, pid: Pid) -> Result<()> {
        self.write_uid_map(pid).await?;
        self.write_gid_map(pid).await?;
        Ok(())
    }

    fn child_stdio(&self, default_stdio: &Stdio) -> Result<(Option<Sender>, Option<Receiver>)> {
        if let Some(stdio) = &self.stdin {
            if !matches!(&stdio.inner, &StdioInner::Pipe) {
                return Ok((None, None));
            }
        } else if !matches!(&default_stdio.inner, &StdioInner::Pipe) {
            return Ok((None, None));
        }

        let (tx, rx) = tokio::net::unix::pipe::pipe()?;

        Ok((Some(tx), Some(rx)))
    }

    fn run_child(
        &mut self,
        pipe: RawFd,
        stdin: Option<RawFd>,
        stdout: Option<RawFd>,
        stderr: Option<RawFd>,
    ) -> isize {
        use std::os::fd::FromRawFd;

        let mut pipe = unsafe { std::os::unix::net::UnixStream::from_raw_fd(pipe) };

        let stdin = stdin.map(|stdin| unsafe { std::os::fd::OwnedFd::from_raw_fd(stdin) });
        let stdout = stdout.map(|stdout| unsafe { std::os::fd::OwnedFd::from_raw_fd(stdout) });
        let stderr = stderr.map(|stderr| unsafe { std::os::fd::OwnedFd::from_raw_fd(stderr) });

        let mut buf = [0u8];
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => return 1,
            Ok(_) => {}
        }
        if buf[0] != 0 {
            return 1;
        }

        if let Err(err) = self.new_root() {
            let errc = err
                .raw_os_error()
                .unwrap_or(nix::errno::Errno::EINVAL as i32);
            let errc = errc.to_be_bytes();
            let _ = pipe.write_all(&errc);
            return 1;
        }

        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args[1..]);
        cmd.arg0(&self.args[0]);
        cmd.env_clear();
        cmd.envs(&self.env);
        if let Some(curr_dir) = &self.curr_dir {
            cmd.current_dir(curr_dir);
        }

        if let Some(stdin) = stdin {
            cmd.stdin(stdin);
        } else {
            let stdin = match self.stdin.take() {
                Some(stdin) => match stdin.into_stdio() {
                    Ok(stdin) => stdin,
                    Err(err) => {
                        let errc = err
                            .raw_os_error()
                            .unwrap_or(nix::errno::Errno::EINVAL as i32);
                        let errc = errc.to_be_bytes();
                        match pipe.write_all(&errc) {
                            Ok(()) => {}
                            Err(_) => return 1,
                        }
                        if let Some(err) = err.into_inner() {
                            let msg = err.to_string();
                            let _ = pipe.write_all(msg.as_bytes());
                        }
                        return 1;
                    }
                },
                None => std::process::Stdio::inherit(),
            };
            cmd.stdin(stdin);
        }

        if let Some(stdout) = stdout {
            cmd.stdout(stdout);
        } else {
            let stdout = match self.stdout.take() {
                Some(stdout) => match stdout.into_stdio() {
                    Ok(stdout) => stdout,
                    Err(err) => {
                        let errc = err
                            .raw_os_error()
                            .unwrap_or(nix::errno::Errno::EINVAL as i32);
                        let errc = errc.to_be_bytes();
                        match pipe.write_all(&errc) {
                            Ok(()) => {}
                            Err(_) => return 1,
                        }
                        if let Some(err) = err.into_inner() {
                            let msg = err.to_string();
                            let _ = pipe.write_all(msg.as_bytes());
                        }
                        return 1;
                    }
                },
                None => std::process::Stdio::inherit(),
            };
            cmd.stdout(stdout);
        }

        if let Some(stderr) = stderr {
            cmd.stderr(stderr);
        } else {
            let stderr = match self.stderr.take() {
                Some(stderr) => match stderr.into_stdio() {
                    Ok(stderr) => stderr,
                    Err(err) => {
                        let errc = err
                            .raw_os_error()
                            .unwrap_or(nix::errno::Errno::EINVAL as i32);
                        let errc = errc.to_be_bytes();
                        match pipe.write_all(&errc) {
                            Ok(()) => {}
                            Err(_) => return 1,
                        }
                        if let Some(err) = err.into_inner() {
                            let msg = err.to_string();
                            let _ = pipe.write_all(msg.as_bytes());
                        }
                        return 1;
                    }
                },
                None => std::process::Stdio::inherit(),
            };
            cmd.stderr(stderr);
        }

        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let errc = err
                    .raw_os_error()
                    .unwrap_or(nix::errno::Errno::EINVAL as i32);
                let errc = errc.to_be_bytes();
                match pipe.write_all(&errc) {
                    Ok(()) => {}
                    Err(_) => return 1,
                }
                if let Some(err) = err.into_inner() {
                    let msg = err.to_string();
                    let _ = pipe.write_all(msg.as_bytes());
                }
                return 1;
            }
        };

        if pipe.write_all(&[0u8; 4]).is_err() {
            return 1;
        }

        Self::monitor_child(pipe, child)
    }

    fn monitor_child(
        mut pipe: std::os::unix::net::UnixStream,
        mut child: std::process::Child,
    ) -> isize {
        loop {
            match child.wait() {
                Ok(status) => {
                    use std::os::unix::process::ExitStatusExt;

                    if let Some(code) = status.code() {
                        let c = code.to_ne_bytes();
                        let msg = [0, c[0], c[1], c[2], c[3]];
                        let _ = pipe.write_all(&msg);
                        return code as isize;
                    }

                    if let Some(sig) = status.signal() {
                        let c = sig.to_ne_bytes();
                        let kind = if status.core_dumped() { 2 } else { 1 };
                        let msg = [kind, c[0], c[1], c[2], c[3]];
                        let _ = pipe.write_all(&msg);
                        return (sig + 128) as isize;
                    }

                    if let Some(sig) = status.stopped_signal() {
                        let c = sig.to_ne_bytes();
                        let msg = [3, c[0], c[1], c[2], c[3]];
                        if pipe.write_all(&msg).is_err() {
                            return 1;
                        }
                    }

                    if status.continued() {
                        let msg = [4u8, 0, 0, 0, 0];
                        if pipe.write_all(&msg).is_err() {
                            return 1;
                        }
                    }
                }
                Err(err) => {
                    if let Some(err) = err.raw_os_error() {
                        let c = err.to_ne_bytes();
                        let msg = [5, c[0], c[1], c[2], c[3]];
                        let _ = pipe.write_all(&msg);
                        return 1;
                    }
                }
            }
        }
    }

    fn new_root(&mut self) -> Result<()> {
        let console = if let Some(stdio) = &self.stdout
            && matches!(&stdio.inner, &StdioInner::Inherit)
        {
            nix::unistd::ttyname(std::io::stdout()).ok()
        } else if let Some(stdio) = &self.stdin
            && matches!(&stdio.inner, &StdioInner::Inherit)
        {
            nix::unistd::ttyname(std::io::stdin()).ok()
        } else if let Some(stdio) = &self.stderr
            && matches!(&stdio.inner, &StdioInner::Inherit)
        {
            nix::unistd::ttyname(std::io::stderr()).ok()
        } else {
            None
        };

        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            None::<&str>,
        )?;

        let rootfs = if let Some(rootfs) = self.rootfs.take() {
            rootfs
        } else {
            std::env::current_dir()?
        };

        mount(
            Some(&rootfs),
            &rootfs,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )?;

        for mnt in &self.mounts {
            let mut dst_path = rootfs.clone();
            dst_path.extend(
                mnt.dst
                    .components()
                    .filter(|comp| !matches!(comp, &Component::Prefix(_) | &Component::RootDir)),
            );
            if !dst_path.try_exists()? {
                std::fs::create_dir_all(&dst_path)?;
            }

            if let MountSrc::Bind(src) = &mnt.src {
                let flag = if mnt.writable {
                    MsFlags::MS_PRIVATE
                } else {
                    MsFlags::MS_RDONLY | MsFlags::MS_PRIVATE
                };
                mount(
                    Some(src),
                    &dst_path,
                    None::<&str>,
                    flag | MsFlags::MS_BIND,
                    None::<&str>,
                )?;
            }
        }

        let old_root = rootfs.join("old_root");
        std::fs::create_dir_all(&old_root)?;

        nix::unistd::pivot_root(&rootfs, &old_root)?;
        nix::unistd::chdir("/")?;

        for mnt in &self.mounts {
            match &mnt.src {
                MountSrc::DevFs => {
                    self.mount_devfs(&mnt.dst, console.as_deref())?;
                }
                MountSrc::ProcFs => {
                    mount(
                        None::<&str>,
                        &mnt.dst,
                        Some("proc"),
                        MsFlags::empty(),
                        None::<&str>,
                    )?;
                }
                MountSrc::SysFs => {
                    mount(
                        None::<&str>,
                        &mnt.dst,
                        Some("sysfs"),
                        MsFlags::empty(),
                        None::<&str>,
                    )?;
                }
                MountSrc::TmpFs => {
                    mount(
                        None::<&str>,
                        &mnt.dst,
                        Some("tmpfs"),
                        MsFlags::empty(),
                        None::<&str>,
                    )?;
                }
                MountSrc::Bind(_) => {
                    let rdonly = if mnt.writable {
                        MsFlags::empty()
                    } else {
                        MsFlags::MS_RDONLY
                    };
                    mount(
                        Some(&mnt.dst),
                        &mnt.dst,
                        None::<&str>,
                        MsFlags::MS_REMOUNT | MsFlags::MS_BIND | rdonly,
                        None::<&str>,
                    )?;
                }
            }
        }

        nix::mount::umount2("/old_root", nix::mount::MntFlags::MNT_DETACH)?;
        std::fs::remove_dir("/old_root")?;

        if self.ro_root {
            mount(
                Some("/"),
                "/",
                None::<&str>,
                MsFlags::MS_RDONLY | MsFlags::MS_REMOUNT | MsFlags::MS_BIND,
                None::<&str>,
            )?;
        }

        Ok(())
    }

    fn mount_devfs(&self, dst: &Path, console: Option<&Path>) -> Result<()> {
        mount(
            None::<&str>,
            dst,
            Some("tmpfs"),
            MsFlags::MS_NOSUID,
            None::<&str>,
        )?;

        for f in ["null", "full", "zero", "random", "urandom", "tty"] {
            let src_path = Path::new("/old_root/dev").join(f);
            let dst_path = dst.join(f);
            Self::touch(&dst_path)?;
            mount(
                Some(&src_path),
                &dst_path,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_NOSUID,
                None::<&str>,
            )?;
        }

        let symlinks = vec![
            (PathBuf::from("/proc/self/fd/0"), dst.join("stdin")),
            (PathBuf::from("/proc/self/fd/1"), dst.join("stdout")),
            (PathBuf::from("/proc/self/fd/2"), dst.join("stderr")),
            (PathBuf::from("/proc/self/fd"), dst.join("fd")),
            (PathBuf::from("/proc/kcore"), dst.join("core")),
        ];

        for (src, dst) in symlinks {
            std::os::unix::fs::symlink(src, dst)?;
        }

        std::fs::create_dir_all(dst.join("shm"))?;

        let pts_path = dst.join("pts");
        std::fs::create_dir_all(&pts_path)?;
        mount(
            None::<&str>,
            &pts_path,
            Some("devpts"),
            MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID,
            Some("newinstance,ptmxmode=0666,mode=620"),
        )?;

        std::os::unix::fs::symlink("pts/ptmx", dst.join("ptmx"))?;

        if let Some(console) = console {
            use std::path::Component;
            let mut src = PathBuf::from("/old_root");
            src.extend(
                console
                    .components()
                    .filter(|comp| !matches!(comp, Component::Prefix(_) | Component::RootDir)),
            );
            let dst = dst.join("console");
            Self::touch(&dst)?;
            mount(
                Some(&src),
                &dst,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID,
                None::<&str>,
            )?;
        }

        Ok(())
    }

    fn touch(path: &Path) -> Result<()> {
        drop(std::fs::File::create(path)?);
        Ok(())
    }
}
