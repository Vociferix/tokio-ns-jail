use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::exit;
use tokio_ns_jail::{Command, GidRange, Namespace, Stdio, UidRange};

#[derive(Debug)]
enum Mount {
    Bind {
        src: PathBuf,
        dst: PathBuf,
        writable: bool,
    },
    DevFs(PathBuf),
    ProcFs(PathBuf),
    SysFs(PathBuf),
    TmpFs(PathBuf),
}

#[derive(Debug)]
struct Args {
    rootfs: Option<PathBuf>,
    writable_root: bool,
    mounts: Vec<Mount>,
    uid_map: Vec<(UidRange, UidRange)>,
    gid_map: Vec<(GidRange, GidRange)>,
    unshare: Namespace,

    program: OsString,
    args: Vec<OsString>,
    env: HashMap<OsString, OsString>,
    curr_dir: Option<PathBuf>,
}

fn main() {
    let rc = {
        let args = Args::parse();

        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                eprintln!("{err}");
                exit(1);
            }
        };

        rt.block_on(async move {
            let mut cmd = Command::new(&args.program);
            cmd.args(&args.args)
                .envs(&args.env)
                .writable_rootfs(args.writable_root)
                .unshare(args.unshare)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());

            if let Some(rootfs) = &args.rootfs {
                cmd.rootfs(rootfs);
            }

            for mnt in &args.mounts {
                match mnt {
                    Mount::Bind { src, dst, writable } => {
                        cmd.bind_mount(src, dst, *writable);
                    }
                    Mount::DevFs(dst) => {
                        cmd.devfs_mount(dst);
                    }
                    Mount::ProcFs(dst) => {
                        cmd.procfs_mount(dst);
                    }
                    Mount::SysFs(dst) => {
                        cmd.sysfs_mount(dst);
                    }
                    Mount::TmpFs(dst) => {
                        cmd.tmpfs_mount(dst);
                    }
                }
            }

            if args.uid_map.is_empty() && args.gid_map.is_empty() {
                cmd.user_as_root();
            }

            for (host, jail) in &args.uid_map {
                cmd.map_uids(*host, *jail);
            }

            for (host, jail) in &args.gid_map {
                cmd.map_gids(*host, *jail);
            }

            if let Some(curr_dir) = &args.curr_dir {
                cmd.current_dir(curr_dir);
            }

            match cmd.status().await {
                Ok(status) => {
                    if let Some(code) = status.code() {
                        return code;
                    }

                    if let Some(sig) = status.signal() {
                        return sig + 128;
                    }

                    1
                }
                Err(err) => {
                    eprintln!("{err}");
                    1
                }
            }
        })
    };

    exit(rc);
}

impl Args {
    fn parse() -> Self {
        let mut out = Args {
            rootfs: None,
            writable_root: false,
            mounts: Vec::new(),
            uid_map: Vec::new(),
            gid_map: Vec::new(),
            unshare: Namespace::empty(),
            program: OsString::new(),
            args: Vec::new(),
            env: HashMap::new(),
            curr_dir: None,
        };

        let mut args = std::env::args_os();
        let _ = args.next();
        let mut first = true;

        loop {
            let Some(arg) = args.next() else {
                if first {
                    Self::print_help();
                    exit(0);
                }
                eprintln!("Expected command to execute inside of jail\n");
                Self::eprint_help();
                exit(2);
            };
            first = false;

            match arg.as_encoded_bytes() {
                b"-h" | b"--help" => {
                    Self::print_help();
                    exit(0);
                }
                b"-R" | b"--rootfs" => {
                    let Some(rootfs) = args.next() else {
                        eprintln!("Expected path following '{}' flag\n", arg.display());
                        Self::eprint_help();
                        exit(2);
                    };
                    out.rootfs = Some(rootfs.into());
                }
                argb if argb.starts_with(b"-R") => {
                    out.rootfs = Some(unsafe { trim_front(&arg, 2) }.to_os_string().into());
                }
                argb if argb.starts_with(b"--rootfs=") => {
                    out.rootfs = Some(unsafe { trim_front(&arg, 9) }.to_os_string().into());
                }
                b"-w" | b"--writable-rootfs" => {
                    out.writable_root = true;
                }
                b"-m" | b"--bind-mount" => {
                    let Some(arg) = args.next() else {
                        eprintln!(
                            "Expected path or a pair of paths following '{}' flag\n",
                            arg.display()
                        );
                        Self::eprint_help();
                        exit(2);
                    };
                    out.bind_mount(&arg, true);
                }
                argb if argb.starts_with(b"-m") => {
                    out.bind_mount(unsafe { trim_front(&arg, 2) }, true);
                }
                argb if argb.starts_with(b"--bind-mount=") => {
                    out.bind_mount(unsafe { trim_front(&arg, 13) }, true);
                }
                b"-r" | b"--ro-bind-mount" => {
                    let Some(arg) = args.next() else {
                        eprintln!(
                            "Expected path or a pair of paths following '{}' flag\n",
                            arg.display()
                        );
                        Self::eprint_help();
                        exit(2);
                    };
                    out.bind_mount(&arg, false);
                }
                argb if argb.starts_with(b"-r") => {
                    out.bind_mount(unsafe { trim_front(&arg, 2) }, false);
                }
                argb if argb.starts_with(b"--ro-bind-mount=") => {
                    out.bind_mount(unsafe { trim_front(&arg, 16) }, false);
                }
                b"-d" | b"--devfs-mount" => {
                    let Some(arg) = args.next() else {
                        eprintln!(
                            "Expected path or a pair of paths following '{}' flag\n",
                            arg.display()
                        );
                        Self::eprint_help();
                        exit(2);
                    };
                    out.mounts.push(Mount::DevFs(arg.into()));
                }
                argb if argb.starts_with(b"-d") => {
                    out.mounts.push(Mount::DevFs(
                        unsafe { trim_front(&arg, 2) }.to_os_string().into(),
                    ));
                }
                argb if argb.starts_with(b"--devfs-mount=") => {
                    out.mounts.push(Mount::DevFs(
                        unsafe { trim_front(&arg, 14) }.to_os_string().into(),
                    ));
                }
                b"-p" | b"--procfs-mount" => {
                    let Some(arg) = args.next() else {
                        eprintln!(
                            "Expected path or a pair of paths following '{}' flag\n",
                            arg.display()
                        );
                        Self::eprint_help();
                        exit(2);
                    };
                    out.mounts.push(Mount::ProcFs(arg.into()));
                }
                argb if argb.starts_with(b"-p") => {
                    out.mounts.push(Mount::ProcFs(
                        unsafe { trim_front(&arg, 2) }.to_os_string().into(),
                    ));
                }
                argb if argb.starts_with(b"--procfs-mount=") => {
                    out.mounts.push(Mount::ProcFs(
                        unsafe { trim_front(&arg, 15) }.to_os_string().into(),
                    ));
                }
                b"-s" | b"--sysfs-mount" => {
                    let Some(arg) = args.next() else {
                        eprintln!(
                            "Expected path or a pair of paths following '{}' flag\n",
                            arg.display()
                        );
                        Self::eprint_help();
                        exit(2);
                    };
                    out.mounts.push(Mount::SysFs(arg.into()));
                }
                argb if argb.starts_with(b"-s") => {
                    out.mounts.push(Mount::SysFs(
                        unsafe { trim_front(&arg, 2) }.to_os_string().into(),
                    ));
                }
                argb if argb.starts_with(b"--sysfs-mount=") => {
                    out.mounts.push(Mount::SysFs(
                        unsafe { trim_front(&arg, 14) }.to_os_string().into(),
                    ));
                }
                b"-t" | b"--tmpfs-mount" => {
                    let Some(arg) = args.next() else {
                        eprintln!(
                            "Expected path or a pair of paths following '{}' flag\n",
                            arg.display()
                        );
                        Self::eprint_help();
                        exit(2);
                    };
                    out.mounts.push(Mount::TmpFs(arg.into()));
                }
                argb if argb.starts_with(b"-t") => {
                    out.mounts.push(Mount::TmpFs(
                        unsafe { trim_front(&arg, 2) }.to_os_string().into(),
                    ));
                }
                argb if argb.starts_with(b"--tmpfs-mount=") => {
                    out.mounts.push(Mount::TmpFs(
                        unsafe { trim_front(&arg, 14) }.to_os_string().into(),
                    ));
                }
                b"-u" | b"--uid-map" => {
                    let Some(arg) = args.next() else {
                        eprintln!(
                            "Expected UID range mapping following '{}' flag\n",
                            arg.display()
                        );
                        Self::eprint_help();
                        exit(2);
                    };
                    out.uid_map(&arg);
                }
                argb if argb.starts_with(b"-u") => {
                    out.uid_map(unsafe { trim_front(&arg, 2) });
                }
                argb if argb.starts_with(b"--uid-map=") => {
                    out.uid_map(unsafe { trim_front(&arg, 10) })
                }
                b"-g" | b"--gid-map" => {
                    let Some(arg) = args.next() else {
                        eprintln!(
                            "Expected GID range mapping following '{}' flag\n",
                            arg.display()
                        );
                        Self::eprint_help();
                        exit(2);
                    };
                    out.gid_map(&arg);
                }
                argb if argb.starts_with(b"-g") => {
                    out.gid_map(unsafe { trim_front(&arg, 2) });
                }
                argb if argb.starts_with(b"--gid-map=") => {
                    out.gid_map(unsafe { trim_front(&arg, 10) })
                }
                b"-U" | b"--unshare" => {
                    let Some(arg) = args.next() else {
                        eprintln!(
                            "Expected one of 'cgroup', 'uts', 'ipc', 'pid', or 'net' following '{}' flag\n",
                            arg.display()
                        );
                        Self::eprint_help();
                        exit(2);
                    };
                    out.unshare(&arg);
                }
                argb if argb.starts_with(b"-U") => {
                    out.unshare(unsafe { trim_front(&arg, 2) });
                }
                argb if argb.starts_with(b"--unshare=") => {
                    out.unshare(unsafe { trim_front(&arg, 10) });
                }
                b"-e" | b"--env" => {
                    let Some(arg) = args.next() else {
                        eprintln!(
                            "Expected an environment variable following '{}' flag\n",
                            arg.display()
                        );
                        Self::eprint_help();
                        exit(2);
                    };
                    out.env(&arg);
                }
                argb if argb.starts_with(b"-e") => {
                    out.env(unsafe { trim_front(&arg, 2) });
                }
                argb if argb.starts_with(b"--env=") => {
                    out.env(unsafe { trim_front(&arg, 6) });
                }
                b"-c" | b"--current-dir" => {
                    let Some(arg) = args.next() else {
                        eprintln!("Expected path following '{}' flag\n", arg.display());
                        Self::eprint_help();
                        exit(2);
                    };
                    out.curr_dir = Some(arg.into());
                }
                argb if argb.starts_with(b"-c") => {
                    out.curr_dir = Some(unsafe { trim_front(&arg, 2) }.to_os_string().into());
                }
                argb if argb.starts_with(b"--current-dir=") => {
                    out.curr_dir = Some(unsafe { trim_front(&arg, 14) }.to_os_string().into());
                }
                b"--" => {
                    let Some(program) = args.next() else {
                        eprintln!("Expected a command to run in the jail\n");
                        Self::eprint_help();
                        exit(2);
                    };
                    out.program = program;
                    break;
                }
                argb if argb.starts_with(b"-") => {
                    eprintln!("Unexpected option '{}'\n", arg.display());
                    Self::eprint_help();
                    exit(2);
                }
                _ => {
                    out.program = arg;
                    break;
                }
            }
        }

        out.args = args.collect();

        out
    }

    const HELP: &str = r#"Usage: ns-jail [OPTIONS...] [--] <COMMAND> [ARGS...]

Options:
  -R|--rootfs PATH
      Specifies a directory to use as the root filesystem
      for the jail.

  -w|--writable-rootfs
      Mount the root filesystem in read-write mode.

  -m|--bind-mount HOST_PATH[:JAIL_PATH]
      Mount a host directory to a path within the jail in
      read-write mode. If no jail path is provided the jail
      path is assumed to be the same as the host path.

  -r|--ro-bind-mount HOST_PATH[:JAIL_PATH]
      Mount a host directory to a path within the jail in
      read-only mode. If no jail path is provided the jail
      path is assumed to be the same as the host path.

  -d|--devfs-mount JAIL_PATH
      Mount a devfs to a path within the jail.

  -p|--procfs-mount JAIL_PATH
      Mount a procfs to a path within the jail. The 'pid'
      namespace must be unshared.

  -s|--sysfs-mount JAIL_PATH
      Mount a sysfs to a path within the jail.

  -t|--tmpfs-mount JAIL_PATH
      Mount a tmpfs to a path within the jail.

  -u|--uid-map HOST_UID:JAIL_UID[:RANGE_LEN]
      Map one or more host UIDs to UIDs within the jail.
      Multiple comma delimited mappings can be specified in
      a single argument.

  -g|--gid-map HOST_GID:JAIL_GID[:RANGE_LEN]
      Map one or more host GIDs to GIDs within the jail.
      Multiple comma delimited mappings can be specified in
      a single argument.

  -U|--unshare NAMESPACE
      Unshare a Linux namespace. Unsharing a namespace
      isolates the jailed process with respect to system
      information and resources covered by the namespace.
      Multiple comma delimited namespaces can be specified
      in a single argument. 'all' can be specified to
      unshare all namespaces.

  -e|--env VAR=VALUE
      Define an environment variable that will be set
      within the jail. No environent variables are captured
      automatically.

  -c|--current-dir JAIL_PATH
      Specify the directory that will be the current
      working directory for the command executed within the
      jail.

Namespaces:
  'pid'     PID isolation. Process IDs will be unique to
            the jail.

  'cgroup'  Control Group isolation. Cgroup directories and
            information will be unique to the jail.

  'uts'     UTS isolation. Host and domain names will be
            unique to the jail.

  'ipc'     IPC isolation. Interprocedural communication
            resources will be unique to the jail.

  'net'     Network isolation. Networking resources will be
            unique to the jail.

  NOTE: Unsharing the 'mount' and 'user' namespaces is
        required for ns-jail to operate, so they are
        unshared unconditionally and are not available as
        arguments to '--unshare'.
"#;

    fn print_help() {
        println!("{}", Self::HELP);
    }

    fn eprint_help() {
        eprintln!("{}", Self::HELP);
    }

    fn bind_mount(&mut self, arg: &OsStr, writable: bool) {
        let argb = arg.as_encoded_bytes();
        if let Some(pos) = argb.iter().position(|b| *b == b':') {
            let host = &argb[..pos];
            let jail = &argb[(pos + 1)..];
            let host = unsafe { OsStr::from_encoded_bytes_unchecked(host) };
            let jail = unsafe { OsStr::from_encoded_bytes_unchecked(jail) };
            self.mounts.push(Mount::Bind {
                src: host.to_os_string().into(),
                dst: jail.to_os_string().into(),
                writable,
            });
        } else {
            let arg = unsafe { OsStr::from_encoded_bytes_unchecked(argb) };
            self.mounts.push(Mount::Bind {
                src: arg.to_os_string().into(),
                dst: arg.to_os_string().into(),
                writable,
            });
        }
    }

    fn uid_map(&mut self, arg: &OsStr) {
        for argb in arg.as_encoded_bytes().split(|b| *b == b',') {
            let Some(pos) = argb.iter().position(|b| *b == b':') else {
                eprintln!(
                    "Expected ':' separating host and jail UIDs in UID mapping '{}'\n",
                    unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                );
                Self::eprint_help();
                exit(2);
            };
            let host = &argb[..pos];
            let Ok(host) = str::from_utf8(host) else {
                eprintln!(
                    "Expected UID in UID mapping '{}'\n",
                    unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                );
                Self::eprint_help();
                exit(2);
            };
            let Ok(host) = host.parse::<u32>() else {
                eprintln!(
                    "Expected UID in UID mapping '{}'\n",
                    unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                );
                Self::eprint_help();
                exit(2);
            };
            let jail = &argb[(pos + 1)..];
            if let Some(pos) = jail.iter().position(|b| *b == b':') {
                let len = &jail[(pos + 1)..];
                let jail = &jail[..pos];
                let Ok(jail) = str::from_utf8(jail) else {
                    eprintln!(
                        "Expected UID in UID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                let Ok(jail) = jail.parse::<u32>() else {
                    eprintln!(
                        "Expected UID in UID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                let Ok(len) = str::from_utf8(len) else {
                    eprintln!(
                        "Expected UID range length in UID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                let Ok(len) = len.parse::<u32>() else {
                    eprintln!(
                        "Expected UID range length in UID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                if len == 0 {
                    return;
                }
                let Some(host_end) = host.checked_add(len) else {
                    eprintln!(
                        "Invalid host UID range in '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                let Some(jail_end) = jail.checked_add(len) else {
                    eprintln!(
                        "Invalid jail UID range in '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                self.uid_map
                    .push(((host..=host_end).into(), (jail..=jail_end).into()));
            } else {
                let Ok(jail) = str::from_utf8(jail) else {
                    eprintln!(
                        "Expected UID in UID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                let Ok(jail) = jail.parse::<u32>() else {
                    eprintln!(
                        "Expected UID in UID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                self.uid_map.push((host.into(), jail.into()));
            };
        }
    }

    fn gid_map(&mut self, arg: &OsStr) {
        for argb in arg.as_encoded_bytes().split(|b| *b == b',') {
            let Some(pos) = argb.iter().position(|b| *b == b':') else {
                eprintln!(
                    "Expected ':' separating host and jail GIDs in GID mapping '{}'\n",
                    unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                );
                Self::eprint_help();
                exit(2);
            };
            let host = &argb[..pos];
            let Ok(host) = str::from_utf8(host) else {
                eprintln!(
                    "Expected GID in GID mapping '{}'\n",
                    unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                );
                Self::eprint_help();
                exit(2);
            };
            let Ok(host) = host.parse::<u32>() else {
                eprintln!(
                    "Expected GID in GID mapping '{}'\n",
                    unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                );
                Self::eprint_help();
                exit(2);
            };
            let jail = &argb[(pos + 1)..];
            if let Some(pos) = jail.iter().position(|b| *b == b':') {
                let len = &jail[(pos + 1)..];
                let jail = &jail[..pos];
                let Ok(jail) = str::from_utf8(jail) else {
                    eprintln!(
                        "Expected GID in GID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                let Ok(jail) = jail.parse::<u32>() else {
                    eprintln!(
                        "Expected GID in GID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                let Ok(len) = str::from_utf8(len) else {
                    eprintln!(
                        "Expected GID range length in GID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                let Ok(len) = len.parse::<u32>() else {
                    eprintln!(
                        "Expected GID range length in GID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                if len == 0 {
                    return;
                }
                let Some(host_end) = host.checked_add(len) else {
                    eprintln!(
                        "Invalid host GID range in '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                let Some(jail_end) = jail.checked_add(len) else {
                    eprintln!(
                        "Invalid jail GID range in '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                self.gid_map
                    .push(((host..=host_end).into(), (jail..=jail_end).into()));
            } else {
                let Ok(jail) = str::from_utf8(jail) else {
                    eprintln!(
                        "Expected GID in GID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                let Ok(jail) = jail.parse::<u32>() else {
                    eprintln!(
                        "Expected GID in GID mapping '{}'\n",
                        unsafe { OsStr::from_encoded_bytes_unchecked(argb) }.display()
                    );
                    Self::eprint_help();
                    exit(2);
                };
                self.gid_map.push((host.into(), jail.into()));
            };
        }
    }

    fn unshare(&mut self, arg: &OsStr) {
        let arg = arg.as_encoded_bytes();
        for arg in arg.split(|b| *b == b',') {
            if arg.eq_ignore_ascii_case(b"cgroup") {
                self.unshare |= Namespace::CGROUP;
            } else if arg.eq_ignore_ascii_case(b"uts") {
                self.unshare |= Namespace::UTS;
            } else if arg.eq_ignore_ascii_case(b"ipc") {
                self.unshare |= Namespace::IPC;
            } else if arg.eq_ignore_ascii_case(b"pid") {
                self.unshare |= Namespace::PID;
            } else if arg.eq_ignore_ascii_case(b"net") {
                self.unshare |= Namespace::NET;
            } else if arg.eq_ignore_ascii_case(b"all") {
                self.unshare = Namespace::all();
            } else {
                eprintln!(
                    "Expected one of 'cgroup', 'uts', 'ipc', 'pid', 'net', or 'all' instead of '{}'\n",
                    unsafe { OsStr::from_encoded_bytes_unchecked(arg) }.display()
                );
                Self::eprint_help();
                exit(2);
            }
        }
    }

    fn env(&mut self, arg: &OsStr) {
        let argb = arg.as_encoded_bytes();
        let Some(pos) = argb.iter().position(|b| *b == b'=') else {
            eprintln!(
                "Expected '=' to set environment variable value in '{}'\n",
                arg.display()
            );
            Self::eprint_help();
            exit(2);
        };
        let var = &argb[..pos];
        let val = &argb[(pos + 1)..];
        let var = unsafe { OsStr::from_encoded_bytes_unchecked(var) };
        let val = unsafe { OsStr::from_encoded_bytes_unchecked(val) };
        self.env.insert(var.into(), val.into());
    }
}

unsafe fn trim_front(s: &OsStr, prefix_len: usize) -> &OsStr {
    let s = &s.as_encoded_bytes()[prefix_len..];
    unsafe { OsStr::from_encoded_bytes_unchecked(s) }
}
