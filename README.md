# GitImage

Kubernetes operator that turns a git URL into a running pod. Each commit is built in-cluster by
BuildKit inside an isolated pod (Kata, gVisor, user namespaces), stored in any OCI registry or
in S3, and injected into your pods by digest. No external CI, no registry required.

```yaml
apiVersion: gitimage.dev/v1alpha1
kind: GitImage
metadata: { name: myapp, namespace: team-a }
spec:
  url: git@github.com:you/myapp.git      # ssh or https
  ref: main                              # branch, tag or full sha
  secretRef: { name: myapp-git }         # ssh: identity + known_hosts | https: username + password
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: myapp, namespace: team-a }
spec:
  selector: { matchLabels: { app: myapp } }
  template:
    metadata:
      labels: { app: myapp }
      annotations:
        gitimage.dev/image.web: myapp    # container "web" runs GitImage "myapp"
    spec:
      containers:
        - name: web
          image: placeholder             # replaced by the built digest at admission
```

A pod created before its image exists waits as `SchedulingGated` (never `ImagePullBackOff`),
then starts with the digest. Every new commit rolls the Deployment.

**Docs:** [diverofdark.github.io/gitimage](https://diverofdark.github.io/gitimage/) ·
**Releases:** every green push to `main` ships an image, a chart and a GitHub release.

**Status:** pre-alpha. All v1 milestones implemented; e2e suite green on kind with registry and
S3 storage under privileged, gVisor and Kata isolation.

## Install

```sh
helm install gitimage oci://ghcr.io/diverofdark/charts/gitimage -n gitimage-system --create-namespace \
  --version <latest release> \
  --set buildPolicy.isolation.mode=runtimeClass --set buildPolicy.isolation.runtimeClassName=gvisor \
  --set storage.type=registry --set storage.registry.url=ghcr.io/acme/builds \
  --set storage.registry.pushSecret=registry-push
```

`buildPolicy.isolation.mode` is required and never falls back (PRD N1): if the chosen mode does
not work, the controller exits with `IsolationUnavailable` and stays in CrashLoopBackOff.
See [`examples/homelab/values.yaml`](examples/homelab/values.yaml) for Talos + Kata + Ceph RGW.

### Isolation modes

| Mode | Values | Verified |
| --- | --- | --- |
| Kata (VM per build) | `mode: runtimeClass`, `runtimeClassName: kata-qemu`, `privilegedInSandbox: true` | kind + kata-deploy 4.3.1, all e2e tests |
| gVisor | `mode: runtimeClass`, `runtimeClassName: gvisor` | kind + runsc, all e2e tests |
| User namespaces | `mode: userns` (rootless BuildKit, `hostUsers: false`) | not in kind: runc cannot mount sysfs in a userns pod on a containerized node |
| Privileged | `mode: privileged`, `allowPrivileged: true` | kind, all e2e tests; trusted repos only |

Kata needs `privilegedInSandbox: true`: its shared filesystem drops the file capabilities that
rootless BuildKit's `newuidmap` needs, so BuildKit runs rootful and privileged *inside the VM*.
That is only safe when the runtime keeps privileged in the guest (containerd
`privileged_without_host_devices = true`, which kata-deploy sets).

### Storage modes

| `storage.type` | Images live in | Pods pull from |
| --- | --- | --- |
| `registry` | `<url>/<namespace>/<gitimage>:<sha>` in any OCI registry | the registry; `pullSecret` is copied into tenant namespaces |
| `s3` | an S3 bucket (Ceph RGW, SeaweedFS, AWS, R2...) | `localhost:25050` on every node, served by the `gitimage-node` DaemonSet; containerd allows plain HTTP to localhost with no node config (verified on containerd 2.3) |

Builds never see storage credentials: BuildKit pushes to the controller's push endpoint with a
token scoped to `<namespace>/<gitimage>` that dies with the build Job.

## Use

| Want | Do |
| --- | --- |
| Watch a build | `kubectl get gi` (columns: Revision, Ready, Status = current BuildKit step); waiting pods get `Building myapp@3f2c9e1: step 4/12 RUN npm ci` Events |
| Private package registries | `build.secrets: [{ id: npmrc, secretRef: { name: npm-token } }]`, then `RUN --mount=type=secret,id=npmrc,target=/root/.npmrc npm ci` |
| Build args | `build.args: { NODE_ENV: { value: production }, TOKEN: { valueFrom: { secretKeyRef: { name: t, key: k } } } }` |
| Monorepo | one GitImage per image with `build.context: services/api`; commits that do not touch the context reuse the last image |
| Build on push, not on poll | set `hooks.baseURL` (+ `hooks.ingress`) and paste `status.webhookURL` into GitHub/GitLab/Gitea |
| Retry a failed build | `kubectl annotate gi myapp gitimage.dev/rebuild=$(date +%s) --overwrite` |
| Cancel stale builds | `build.onNewCommit: cancel` (default `wait`) |
| Pin a workload | `gitimage.dev/rollout: manual` on the Deployment |
| ArgoCD | see [`examples/argocd/application.yaml`](examples/argocd/application.yaml) (`ignoreDifferences` for the roller and caBundle) |

## What runs where

| Namespace | Workload | Role |
| --- | --- | --- |
| `gitimage-system` | `gitimage-controller` (2 replicas, Lease) | reconciles GitImages, push endpoint (:5000), hooks (:8080), GC, gate release |
| `gitimage-system` | `gitimage-webhook` (2 replicas) | admission: gates pods, injects digests, denies cross-tenant image refs |
| `gitimage-system` | `gitimage-node` (DaemonSet, s3 mode only) | read-only OCI server on `127.0.0.1:25050` |
| tenant namespaces | build Jobs, `gitimage-build` ConfigMap, `gitimage-build-egress` NetworkPolicy | one Job per commit; git credentials and build secrets stay here |

Anything else the install needs (an ObjectBucketClaim, ExternalSecret, RuntimeClass...) goes in
`extraObjects`: a list of manifests, maps or `tpl`-rendered strings.

## Develop

```sh
cargo test --workspace                       # unit + storage + router + OCI tests
./hack/check-chart-config.sh                 # chart renders a config the controller accepts
./hack/oci-conformance.sh                    # OCI distribution-spec conformance (74/74)
./hack/e2e/run.sh s3 gvisor                  # kind e2e: registry|s3 x privileged|gvisor|kata-qemu
cargo run -p gitimage -- crdgen --out charts/gitimage/crds/gitimage.yaml   # after API changes
```

The e2e environment uses its own kubeconfig (`hack/.kubeconfig`); it never touches yours.

| Path | What |
| --- | --- |
| `crates/gitimage-api` | CRD types, annotation keys, controller `Config` |
| `crates/gitimage-git` | gix ref listing and clone, russh ssh transport, host tree APIs |
| `crates/gitimage-oci` | OCI distribution routes, S3 and upstream-registry storage, push tokens |
| `crates/gitimage-controller` | reconciler, Job builder, gate releaser, roller, hooks, GC, isolation check |
| `crates/gitimage-webhook` | admission: scheduling gates, digest injection, cross-tenant guard |
| `crates/gitimage-node` | read-only OCI server for s3 mode |
| `crates/gitimage-tls` | self-managed CA, serving certs, hot-reloading listeners |
| `crates/gitimage` | the binary: `controller`, `webhook`, `node`, `clone`, `crdgen`, `check-config` |

## Known limitations

- Passphrase-protected ssh keys and git submodules are not supported.
- S3 blobs over 5 GiB fail (single CopyObject after multipart upload); add a bucket lifecycle
  rule for the `uploads/` prefix to clean abandoned uploads.
- Registry mode GC deletes manifests only; blob GC is the registry's job.
- Talos, k3s and managed clouds are not yet covered by automated e2e; Kata e2e needs runners
  with `/dev/kvm` (repository variables `E2E_RUNS_ON`, `E2E_ISOLATIONS`).

## License

Apache-2.0
