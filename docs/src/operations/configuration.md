# Helm values

| Value | Default | Meaning |
| --- | --- | --- |
| `buildPolicy.isolation.mode` | required | `runtimeClass`, `userns` or `privileged` |
| `buildPolicy.isolation.runtimeClassName` | | RuntimeClass for `runtimeClass` mode |
| `buildPolicy.isolation.allowPrivileged` | `false` | must be `true` for `privileged` |
| `buildPolicy.isolation.privilegedInSandbox` | `false` | rootful BuildKit inside the sandbox (Kata) |
| `buildPolicy.isolation.rootfulInSandbox` | `false` | rootful BuildKit, unprivileged, `hostUsers: false` (Sysbox); runtimeClass only, excludes `privilegedInSandbox` |
| `buildPolicy.egress.allowInternet` | `true` | build pods may reach the internet |
| `buildPolicy.egress.denyClusterCIDRs` | `true` | except `network.clusterCIDRs` |
| `network.clusterCIDRs` | `[]` | pod and service CIDRs, IPv4 and IPv6 |
| `storage.type` | `registry` | `registry` or `s3` |
| `storage.registry.url`, `.pushSecret`, `.pullSecret`, `.insecure` | | registry mode |
| `storage.s3.bucket`, `.endpoint`, `.region`, `.secret`, `.readOnlySecret`, `.pathStyle` | | s3 mode |
| `build.image` / `build.rootfulImage` | `moby/buildkit:v0.34.2-rootless` / `moby/buildkit:v0.34.2` | BuildKit images |
| `build.activeDeadline` | `1h` | per build |
| `build.platforms` | `[linux/amd64]` | more than one builds a multi-arch index |
| `build.cache` | `max` | `max`, `min` or `off` |
| `build.labels`, `build.extraOpts`, `build.defaultResources` | | applied to every build |
| `build.stateVolume.medium`, `.sizeLimit` | `""` | emptyDir for BuildKit's state; `Memory` = tmpfs |
| `build.stateVolume.block` | unset | `{storageClassName, size, fsType: ext4\|ext3, formatImage}`: raw block ephemeral PVC per build, formatted and mounted in the pod (Kata on Talos) |
| `build.stateVolume.labels`, `.annotations` | `{}` | extra metadata on the state PVCs (`block` or `persistent`), e.g. `velero.io/exclude-from-backup: "true"` |
| `build.stateVolume.persistent` | unset | `{size, storageClassName}`: one PVC per GitImage kept between builds; see [Persistent build cache](#persistent-build-cache) |
| `build.buildkitdConfig` | `""` | appended to the build pods' `buildkitd.toml` (registry mirrors, private registries) |
| `hooks.baseURL`, `hooks.ingress.*` | | push hooks |
| `ui.enabled`, `ui.baseURL`, `ui.auth.*`, `ui.ingress.*` | `false` | [status UI](ui.md) on port 8082; `ui.auth.mode` is `none` or `oidc` |
| `controller.replicas`, `webhook.replicas` | `2` | |
| `controller.extraEnv` | `[]` | extra env for the controller |
| `controller.pinTemplateImage` | `false` | also write the built image into workload templates, for scanners ([Workloads](../guide/workloads.md#image-scanners)) |
| `webhook.crossTenantNamespaces` | `[]` | namespaces whose pods may use any namespace's GitImage images (image scanners) |
| `node.port`, `node.healthPort` | `25050`, `25051` | gitimage-node host ports |
| `podMonitor.enabled` | `false` | Prometheus Operator PodMonitor |
| `logFormat`, `logLevel` | `json`, `info,kube=warn` | |
| `extraObjects` | `[]` | extra manifests installed with the release |

## Persistent build cache

By default every build starts BuildKit on an empty state volume and gets its cache from the
registry (`build.cache`): a step that misses the cache first downloads every layer below it,
and `RUN --mount=type=cache` (cargo, npm, ccache) always starts empty. With
`build.stateVolume.persistent` each GitImage gets a PVC, `<name>-buildkit`, that its builds
reuse:

```yaml
build:
  stateVolume:
    persistent:
      size: 20Gi
      storageClassName: local-path   # default class if unset
```

- **Cache.** Cache hits are local, nothing is downloaded, and cache mounts keep their contents.
  BuildKit's garbage collection keeps the state within 80% of `size`. Keep `build.cache` on: it
  fills a new PVC (first build, after a reset or a settings change).
- **One build at a time.** The PVC is `ReadWriteOnce` and BuildKit locks its state dir, so with
  `onNewCommit: cancel` the next build starts once the cancelled build's pod is gone (status:
  `waiting for the cancelled build of <sha> to stop`).
- **Placement.** Builds of a GitImage run where its PVC can be attached: its zone, or its node
  with `local-path`. Network block storage adds a few seconds of attach time per build.
- **Reset.** `kubectl delete pvc <name>-buildkit`; the controller creates a new one for the next
  build. It also recreates the PVC itself when the isolation layout (rootful or rootless),
  `size`, `storageClassName` or `block.fsType` change, with a `StateVolumeRecreated` Event.
- **Block mode.** With `block` too, the PVC is `volumeMode: Block`, sized by `persistent`, and
  formatted only when it holds no filesystem; `e2fsck -p` repairs it after a killed build. Give
  `block` only `fsType`/`formatImage` then: `block: { fsType: ext4 }`.
- **Backups.** The PVCs hold only a cache. Keep them out of backups with
  `build.stateVolume.labels` (Velero: `velero.io/exclude-from-backup: "true"`) or
  `.annotations` (k8up: `k8up.io/backup: "false"`); PVCs that already exist get them on their
  next build. A PVC restored anyway points at the old GitImage's UID: the controller replaces
  it rather than reuse it.
- **Security.** A commit can leave files in the state that later builds of the **same**
  GitImage use, as it already can through the registry cache; other GitImages never see it.
  Reset the PVC after building an untrusted commit.

Not combinable with `medium` or `sizeLimit`. The PVC is deleted with its GitImage.

## extraObjects

Anything else the install needs, as maps or templated strings (`tpl` is applied):

```yaml
extraObjects:
  - apiVersion: objectbucket.io/v1alpha1
    kind: ObjectBucketClaim
    metadata: { name: gitimage, namespace: "{{ .Release.Namespace }}" }
    spec: { bucketName: gitimage, storageClassName: ceph-bucket }
  - |
    apiVersion: v1
    kind: ConfigMap
    metadata:
      name: notes
      labels: {{- include "gitimage.labels" . | nindent 8 }}
    data: { pushHost: {{ include "gitimage.pushHost" . }} }
```

The chart writes `gitimage-config` from these values; `gitimage check-config --config <file>`
validates a rendered config with the controller's strict parser.
