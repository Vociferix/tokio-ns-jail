# `tokio-ns-jail`

`tokio-ns-jail` is a Rust crate that implements Linux namespace jailing
built on Tokio. `tokio_ns_jail::Command` provides an interface similar
to `std::process::Command` and `tokio::process::Command`, which allows
spawning a subprocess isolated by Linux namespaces.

Currently, `tokio-ns-jail` directly supports rootless mounts and user
and group ID mapping. There is _not_ currently built-in support for
management of other resources, such as networking, CPU pinning, or
memory limits. This crate is intended for simple process isolation.
For more advanced features, consider using Docker or Podman. However,
some such features may be added in the future. Feel free to open a
pull request or an issue.

More details are available in the code documentation. Try the command
line application to test out its capabilities (requires an existing
unpacked filesystem for the jail to run in).

```shell
$ cargo run -- \
    --rootfs /my/custom/rootfs \
    --devfs-mount /dev \
    --procfs-mount /proc \
    --sysfs-mount /sys \
    --tmpfs-mount /tmp \
    --bind-mount /my/data:/mnt \
    --unshare all \
    /bin/sh
```
