# GitImage

Kubernetes operator that turns a git URL into a running pod. Each commit is built in-cluster by
rootless BuildKit inside an isolated pod (Kata, gVisor or user namespaces). The image lands in any
OCI registry or in S3, with no external CI.

```yaml
apiVersion: gitimage.dev/v1alpha1
kind: GitImage
metadata: { name: myapp, namespace: team-a }
spec:
  url: git@github.com:you/myapp.git
  ref: main
  secretRef: { name: myapp-git }
---
# any Deployment, StatefulSet, Job...
template:
  metadata:
    annotations:
      gitimage.dev/image.web: myapp   # container "web" runs GitImage "myapp"
  spec:
    containers:
      - name: web
        image: placeholder            # the webhook patches in the built digest
```

**Status:** pre-alpha. Milestone M1 (CRD, build Jobs, registry storage) is in progress.

## Layout

| Path | What |
| --- | --- |
| `crates/gitimage-api` | CRD types, annotation keys, controller `Config` |
| `crates/gitimage-git` | gix-based ref listing and clone, russh ssh transport |
| `crates/gitimage-oci` | OCI distribution routes, S3 and upstream-registry storage, push tokens |
| `crates/gitimage-controller` | Reconcilers, build Job builder, gate releaser, roller, hooks, GC |
| `crates/gitimage-webhook` | Admission: scheduling gates and digest injection |
| `crates/gitimage-node` | Read-only OCI server on `127.0.0.1:25050` for s3 mode |
| `crates/gitimage` | The binary: `controller`, `webhook`, `node`, `clone`, `crdgen` |
| `charts/gitimage` | Helm chart; `crds/` is generated |
| `tests/e2e` | End-to-end suite against kind |

## Develop

```sh
cargo test --workspace
cargo run -p gitimage -- crdgen --out charts/gitimage/crds/gitimage.yaml   # after changing API types
```

## License

Apache-2.0
