# Isolation modes

`buildPolicy.isolation.mode` is required and has no default. At startup the controller runs a
check Job with exactly the build pod's isolation settings (`buildctl-daemonless.sh debug
workers`); if it fails, the controller exits with code 3 and logs `IsolationUnavailable`.
Nothing falls back to a weaker mode, and nothing builds.

| Mode | Values | Build pod | Verified in CI |
| --- | --- | --- | --- |
| Kata | `runtimeClass`, `runtimeClassName: kata-qemu`, `privilegedInSandbox: true` | rootful BuildKit, privileged inside the VM | kind + kata-deploy (KVM runners) |
| gVisor | `runtimeClass`, `runtimeClassName: gvisor` | rootless BuildKit, seccomp/AppArmor unconfined inside the sandbox | kind + runsc |
| User namespaces | `userns` | rootless BuildKit, `hostUsers: false` | not on kind (see below) |
| Privileged | `privileged` + `allowPrivileged: true` | rootful BuildKit, privileged on the node | kind |

**Kata and `privilegedInSandbox`.** Kata's shared filesystem drops the file capabilities that
rootless BuildKit's `newuidmap` needs, so BuildKit runs rootful and privileged *inside the
guest*. That is only safe when the runtime keeps privileged in the guest: containerd's
`privileged_without_host_devices = true` for the Kata handler (kata-deploy sets it). Check your
node's containerd config before enabling it.

**userns in kind.** runc cannot mount sysfs for a user-namespaced pod when the node itself is a
container, so kind cannot test this mode; use it on VM or bare-metal nodes with Kubernetes 1.33+.

Use the RuntimeClass's `scheduling.nodeSelector` to keep Kata builds on nodes that have it.
