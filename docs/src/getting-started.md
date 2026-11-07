# Getting started

You need Kubernetes 1.30+ with containerd, Helm 3, and either an OCI registry or an S3 bucket.

## 1. Pick an isolation mode

| Cluster | Use |
| --- | --- |
| Nodes with `/dev/kvm` and Kata installed | `mode: runtimeClass`, `runtimeClassName: kata-qemu`, `privilegedInSandbox: true` |
| gVisor installed (GKE Sandbox, `runsc`) | `mode: runtimeClass`, `runtimeClassName: gvisor` |
| Kubernetes 1.33+ on VMs or bare metal | `mode: userns` |
| Trusted repositories only | `mode: privileged`, `allowPrivileged: true` |

See [Isolation modes](operations/isolation.md).

## 2. Install

Registry storage:

```sh
kubectl create namespace gitimage-system
kubectl -n gitimage-system create secret docker-registry registry-push \
  --docker-server=ghcr.io --docker-username=<user> --docker-password=<token>

helm install gitimage oci://ghcr.io/diverofdark/charts/gitimage --version {{VERSION}} \
  -n gitimage-system \
  --set buildPolicy.isolation.mode=runtimeClass \
  --set buildPolicy.isolation.runtimeClassName=gvisor \
  --set storage.type=registry \
  --set storage.registry.url=ghcr.io/<you>/builds \
  --set storage.registry.pushSecret=registry-push
```

S3 storage (no registry):

```sh
kubectl -n gitimage-system create secret generic gitimage-s3 \
  --from-literal=AWS_ACCESS_KEY_ID=... --from-literal=AWS_SECRET_ACCESS_KEY=...

helm install gitimage oci://ghcr.io/diverofdark/charts/gitimage --version {{VERSION}} \
  -n gitimage-system \
  --set buildPolicy.isolation.mode=userns \
  --set storage.type=s3 --set storage.s3.bucket=gitimage \
  --set storage.s3.endpoint=https://s3.example.com --set storage.s3.secret=gitimage-s3
```

The controller runs an isolation check on start. `kubectl -n gitimage-system get pods` shows
`Running` when the mode works; `CrashLoopBackOff` with `IsolationUnavailable` in the log when it
does not.

## 3. Build something

Build pods need the `privileged` Pod Security level in their namespace (see
[Security model](operations/security.md)).

```sh
kubectl create namespace demo
kubectl label namespace demo pod-security.kubernetes.io/enforce=privileged
kubectl apply -n demo -f - <<'YAML'
apiVersion: gitimage.dev/v1alpha1
kind: GitImage
metadata: { name: hello }
spec:
  url: https://github.com/<you>/<public-repo-with-a-Dockerfile>.git
  ref: main
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: hello }
spec:
  selector: { matchLabels: { app: hello } }
  template:
    metadata:
      labels: { app: hello }
      annotations: { gitimage.dev/image.web: hello }
    spec:
      containers: [{ name: web, image: placeholder }]
YAML

kubectl -n demo get gi -w        # Revision, Ready, Status (current BuildKit step)
kubectl -n demo get pods -w      # SchedulingGated, then Running
```
