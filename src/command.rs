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

    pub fn rootfs(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.rootfs = Some(path.as_ref().into());
        self
    }

    pub fn writable_rootfs(&mut self, writable: bool) -> &mut Self {
        self.ro_root = !writable;
        self
    }

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

    pub fn devfs_mount(&mut self, jail_path: impl AsRef<Path>) -> &mut Self {
        self.mounts.push(Mount {
            src: MountSrc::DevFs,
            dst: jail_path.as_ref().to_path_buf(),
            writable: true,
        });
        self
    }

    pub fn procfs_mount(&mut self, jail_path: impl AsRef<Path>) -> &mut Self {
        self.mounts.push(Mount {
            src: MountSrc::ProcFs,
            dst: jail_path.as_ref().to_path_buf(),
            writable: true,
        });
        self
    }

    pub fn sysfs_mount(&mut self, jail_path: impl AsRef<Path>) -> &mut Self {
        self.mounts.push(Mount {
            src: MountSrc::SysFs,
            dst: jail_path.as_ref().to_path_buf(),
            writable: true,
        });
        self
    }

    pub fn tmpfs_mount(&mut self, jail_path: impl AsRef<Path>) -> &mut Self {
        self.mounts.push(Mount {
            src: MountSrc::TmpFs,
            dst: jail_path.as_ref().to_path_buf(),
            writable: true,
        });
        self
    }

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

    pub fn unshare(&mut self, namespaces: Namespace) -> &mut Self {
        self.unshare = namespaces;
        self
    }

    pub fn user_as_root(&mut self) -> &mut Self {
        self.map_uids(Uid::effective(), 0u32);
        self.map_gids(Gid::effective(), 0u32);
        self
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().into());
        self
    }

    pub fn args<I>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator,
        I::Item: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().into()));
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.env.insert(key.as_ref().into(), val.as_ref().into());
        self
    }

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

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env.remove(key.as_ref());
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.env.clear();
        self
    }

    pub fn current_dir(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.curr_dir = Some(path.as_ref().into());
        self
    }

    pub fn stdin(&mut self, cfg: impl Into<Option<Stdio>>) -> &mut Self {
        self.stdin = cfg.into();
        self
    }

    pub fn stdout(&mut self, cfg: impl Into<Option<Stdio>>) -> &mut Self {
        self.stdout = cfg.into();
        self
    }

    pub fn stderr(&mut self, cfg: impl Into<Option<Stdio>>) -> &mut Self {
        self.stderr = cfg.into();
        self
    }

    pub fn arg0(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args[0] = arg.as_ref().into();
        self
    }

    pub async fn spawn(&mut self) -> Result<Child> {
        self.spawn_impl(Stdio::inherit()).await
    }

    pub async fn output(&mut self) -> Result<Output> {
        self.spawn_impl(Stdio::piped())
            .await?
            .wait_with_output()
            .await
    }

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

        let mut stack: Vec<u8> = Vec::new();
        stack.resize(STACK_SIZE, 0);

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
            status: super::child::Status::StillAlive,
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
                            let _ = pipe.write_all(&msg.as_bytes());
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
                            let _ = pipe.write_all(&msg.as_bytes());
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
                            let _ = pipe.write_all(&msg.as_bytes());
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
                    let _ = pipe.write_all(&msg.as_bytes());
                }
                return 1;
            }
        };

        if let Err(_) = pipe.write_all(&[0u8; 4]) {
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
