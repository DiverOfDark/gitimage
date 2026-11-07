# Isolation modes

`buildPolicy.isolation.mode` is required and has no default. At startup the controller runs a
check Job with exactly the build pod's isolation settings (`buildctl-daemonless.sh debug
workers`); if it fails, the controller exits with code 3 and logs `IsolationUnavailable`.
Nothing falls back to a weaker mode, and nothing builds.

| Mode | Values | Build pod | Verified in CI |
| --- | --- | --- | --- |
| Kata | `runtimeClass`, `runtimeClassName: kata-qemu`, `privilegedInSandbox: true` | rootful BuildKit, privileged inside the VM | kind + kata-deploy (KVM runners) |
| gVisor | `runtimeClass`, `runtimeClassName: gvisor` | rootless BuildKit, seccomp/AppArmor unconfined inside the sandbox | kind + runsc |
| Sysbox | `runtimeClass`, `runtimeClassName: sysbox-runc`, `rootfulInSandbox: true` | rootful BuildKit, unprivileged, `hostUsers: false`; Pod Security `baseline` | no; kind + Sysbox 0.7.1 locally (`hack/e2e/sysbox.sh`) |
| User namespaces | `userns` | rootless BuildKit, `hostUsers: false` | not on kind (see below) |
| Privileged | `privileged` + `allowPrivileged: true` | rootful BuildKit, privileged on the node | kind |

**Kata and `privilegedInSandbox`.** Kata's shared filesystem drops the file capabilities that
rootless BuildKit's `newuidmap` needs, so BuildKit runs rootful and privileged *inside the
guest*. That is only safe when the runtime keeps privileged in the guest: containerd's
`privileged_without_host_devices = true` for the Kata handler (kata-deploy sets it). Check your
node's containerd config before enabling it.

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
sys caused: operation not permitted`). It is not in the default CI matrix; add `sysbox` to the
repository variable `E2E_ISOLATIONS` to run it (and to `E2E_GITHUB_ISOLATIONS` if the
self-hosted runners disable user namespaces).

**userns in kind.** runc cannot mount sysfs for a user-namespaced pod when the node itself is a
container, so kind cannot test this mode; use it on VM or bare-metal nodes with Kubernetes 1.33+.

**Hosts without user namespaces.** gVisor's gofer, rootless BuildKit and `userns` mode all
create user namespaces; with `user.max_user_namespaces=0` (the Talos default) they fail
(`fork/exec /proc/self/exe: no space left on device`). Kata with `privilegedInSandbox` and
`privileged` mode do not need them.

Use the RuntimeClass's `scheduling.nodeSelector` to keep Kata builds on nodes that have it.
