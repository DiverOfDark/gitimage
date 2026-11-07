# GitImage

GitImage is a Kubernetes operator that turns a git URL into a running pod. You point a
`GitImage` at a repository; every commit is built in-cluster by BuildKit inside an isolated pod,
stored in an OCI registry or in S3, and injected into your workloads by digest.

```yaml
apiVersion: gitimage.dev/v1alpha1
kind: GitImage
metadata: { name: myapp, namespace: team-a }
spec:
  url: git@github.com:you/myapp.git
  ref: main
  secretRef: { name: myapp-git }
```

```yaml
# in any Deployment, StatefulSet, DaemonSet, Job or CronJob pod template
metadata:
  annotations:
    gitimage.dev/image.web: myapp     # container "web" runs GitImage "myapp"
spec:
  containers:
    - name: web
      image: placeholder              # replaced by the built digest at admission
```

What you get:

- **No CI, no registry required.** Builds run in your cluster; with S3 storage, nodes pull from a
  node-local read-only server and no registry exists at all.
- **Pods never fail on a missing image.** A pod created before its build finishes waits as
  `SchedulingGated`, then starts with the digest.
- **Every commit rolls out.** Deployments, StatefulSets and DaemonSets get the new digest
  automatically; each pod is pinned to an exact commit.
- **Builds are isolated.** Kata VMs, gVisor, user namespaces or (opt-in) privileged; the
  controller refuses to start rather than fall back to a weaker mode.
- **Builds never hold credentials they do not need.** Git credentials stay in the tenant
  namespace; storage credentials never leave the controller.

Current release: **{{VERSION}}**. Source: [github.com/DiverOfDark/gitimage](https://github.com/DiverOfDark/gitimage).
