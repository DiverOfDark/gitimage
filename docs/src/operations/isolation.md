# Isolation modes

`buildPolicy.isolation.mode` is required and has no default. At startup the controller runs a
check Job with exactly the build pod's isolation settings: BuildKit lists its workers, then
builds a two-layer image (from its own busybox, nothing is pulled) whose second layer
hard-links a file of the first while it is mapped. If either fails, the controller exits with
code 3 and logs `IsolationUnavailable`; a failed hard link names the fix (gVisor: exclusive
file access).
Nothing falls back to a weaker mode, and nothing builds.

| Mode | Values | Build pod | Verified in CI |
| --- | --- | --- | --- |
| Kata | `runtimeClass`, `runtimeClassName: kata-qemu`, `privilegedInSandbox: true` | rootful BuildKit, privileged inside the VM | kind + kata-deploy (KVM runners) |
| gVisor | `runtimeClass`, `runtimeClassName: gvisor-build` (a runsc handler with `file-access-mounts = "exclusive"`, see below) | rootless BuildKit, seccomp/AppArmor unconfined inside the sandbox | kind + runsc |
| Sysbox | `runtimeClass`, `runtimeClassName: sysbox-runc`, `rootfulInSandbox: true` | rootful BuildKit, unprivileged, `hostUsers: false`; Pod Security `baseline` | kind + Sysbox 0.7.1 (`hack/e2e/sysbox.sh`) |
| User namespaces | `userns` | rootless BuildKit, `hostUsers: false` | not on kind (see below) |
| Privileged | `privileged` + `allowPrivileged: true` | rootful BuildKit, privileged on the node | kind |

**Kata and `privilegedInSandbox`.** Kata's shared filesystem drops the file capabilities that
rootless BuildKit's `newuidmap` needs, so BuildKit runs rootful and privileged *inside the
guest*. That is only safe when the runtime keeps privileged in the guest: containerd's
`privileged_without_host_devices = true` for the Kata handler (kata-deploy sets it). Check your
node's containerd config before enabling it.

**gVisor needs exclusive file access for build volumes.** BuildKit keeps its overlay layers on
the build pod's state `emptyDir`. gVisor treats volumes as shared with the outside world by
default (`--file-access-mounts=shared`), and in that mode it cannot copy up a lower-layer file
that a running process has memory-mapped: the copy-up fails with `ENODEV`. Package managers hit
this routinely, e.g. `apt-get` upgrading a library that `apt-get` itself has loaded stops with
`unable to make backup link of '...libcrypto.so.3' before installing new version: No such
device`. Give builds a runsc handler with exclusive file access; build volumes are `emptyDir`s
private to the pod, which is exactly what exclusive assumes (it also lets gVisor cache their
pages). containerd:

```toml
[plugins."io.containerd.cri.v1.runtime".containerd.runtimes.runsc-build]
  runtime_type = "io.containerd.runsc.v1"

[plugins."io.containerd.cri.v1.runtime".containerd.runtimes.runsc-build.options]
  TypeUrl = "io.containerd.runsc.v1.options"
  ConfigPath = "/etc/containerd/runsc-build.toml"   # [runsc_config] file-access-mounts = "exclusive"
```

plus a RuntimeClass `gvisor-build` with `handler: runsc-build`. Where the runtime cannot be
configured, `snapshotter: native` also works: it copies every layer instead of stacking them,
which avoids copy-up but makes builds several times slower and multiplies their disk use.
(`fuse-overlayfs` does not run in gVisor at all; per-pod gVisor mount hints turn an `emptyDir`
into a RAM-backed tmpfs.) The isolation check builds exactly this case, so a gVisor handler
with shared file access stops the controller at startup rather than failing the first build
that upgrades a package.

**Sysbox and `rootfulInSandbox`.** [Sysbox](https://github.com/nestybox/sysbox) runs the pod in
a user namespace, gives root in the pod its capabilities there and virtualizes procfs/sysfs, so
rootful BuildKit works without `privileged: true` or unconfined seccomp/AppArmor (Sysbox applies
its own seccomp filter and ignores AppArmor profiles). `rootfulInSandbox: true` selects the
rootful image and sets `hostUsers: false`; it needs mode `runtimeClass` and excludes
`privilegedInSandbox`. Sysbox must not be combined with privileged containers. Requirements
(Sysbox docs): Sysbox 0.7+ (`sysbox-deploy-k8s`), Kubernetes user namespaces (on by default
from 1.33) and containerd 2.0.5+, so `hostUsers: false` reaches the runtime; GitImage does not
set the CRI-O-only `io.kubernetes.cri-o.userns-mode` annotation older setups used.
`sysbox-deploy-k8s` refuses Kubernetes versions it was not tested with (1.32-1.35 for Sysbox 0.7.1), documents Ubuntu nodes, and sets host sysctls.

**Sysbox in kind.** `hack/e2e/sysbox.sh` installs the Sysbox 0.7.1 package by hand into the
kind worker (Kubernetes 1.37, containerd 2.3; the installer would refuse 1.37 and change the
sysctls of the host kernel, which kind nodes share), remounts the node's `/sys`
read-write and removes kind's bind mounts over `/sys/devices/virtual/dmi/id/product_*`: with
either in place the kernel refuses the container's sysfs mount (`sysfs mount through procfd to
sys caused: operation not permitted`). CI always runs it (matrix `include`, independent of
`E2E_ISOLATIONS`); the nodes need user namespaces enabled.

**Kata on Talos (virtiofs without xattrs).** Kata shares filesystem pod volumes (emptyDir,
and PVCs in `Filesystem` mode) into the VM over virtiofs. When that share lacks
extended-attribute support (Talos' Kata extension), BuildKit fails with
`failed to create hash for ...: operation not supported`. Use a raw block volume instead: Kata
attaches `volumeMode: Block` PVCs to the VM as a virtio-blk disk, so BuildKit gets a real
filesystem.

```yaml
build:
  stateVolume:
    block:
      storageClassName: ceph-block   # any class that supports volumeMode: Block
      size: 20Gi
      fsType: ext4                   # or ext3
```

Each build pod gets a fresh ephemeral PVC (deleted with the pod). A `format-state` init container
(`formatImage`, default `debian:bookworm-slim`) runs `mkfs`, and the BuildKit container mounts
the device at its state dir before starting, so this needs `privileged` or
`privilegedInSandbox`. The isolation check runs with the same volume. `medium: Memory` (tmpfs)
also works but counts against the build's memory limit.

**userns in kind.** runc cannot mount sysfs for a user-namespaced pod when the node itself is a
container, so kind cannot test this mode; use it on VM or bare-metal nodes with Kubernetes 1.33+.

**Hosts without user namespaces.** gVisor's gofer, rootless BuildKit and `userns` mode all
create user namespaces; with `user.max_user_namespaces=0` (the Talos default) they fail
(`fork/exec /proc/self/exe: no space left on device`). Kata with `privilegedInSandbox` and
`privileged` mode do not need them.

Use the RuntimeClass's `scheduling.nodeSelector` to keep Kata builds on nodes that have it.
