# Helm values

| Value | Default | Meaning |
| --- | --- | --- |
| `buildPolicy.isolation.mode` | required | `runtimeClass`, `userns` or `privileged` |
| `buildPolicy.isolation.runtimeClassName` | | RuntimeClass for `runtimeClass` mode |
| `buildPolicy.isolation.allowPrivileged` | `false` | must be `true` for `privileged` |
| `buildPolicy.isolation.privilegedInSandbox` | `false` | rootful BuildKit inside the sandbox (Kata) |
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
| `hooks.baseURL`, `hooks.ingress.*` | | push hooks |
| `controller.replicas`, `webhook.replicas` | `2` | |
| `controller.extraEnv` | `[]` | extra env for the controller |
| `node.port`, `node.healthPort` | `25050`, `25051` | gitimage-node host ports |
| `podMonitor.enabled` | `false` | Prometheus Operator PodMonitor |
| `logFormat`, `logLevel` | `json`, `info,kube=warn` | |
| `extraObjects` | `[]` | extra manifests installed with the release |

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
