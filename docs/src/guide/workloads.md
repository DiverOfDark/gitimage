# Running workloads

Annotate the **pod template** (not the workload's own metadata):

```yaml
spec:
  template:
    metadata:
      annotations:
        gitimage.dev/image.web: myapp      # container "web" -> GitImage "myapp"
        gitimage.dev/image.worker: myapp   # several containers may use one GitImage
    spec:
      containers:
        - { name: web, image: placeholder }
        - { name: worker, image: placeholder, command: [worker] }
```

Init containers can be bound the same way. The GitImage must be in the pod's namespace.

| You want | Do |
| --- | --- |
| Automatic rollouts | nothing: Deployments, StatefulSets and DaemonSets get `gitimage.dev/revision.<container>` pinned on each new commit, and `gitimage.dev/digest.<container>` on each new image (a [rebuild](gitimage.md#rebuilding-a-commit) of the same commit rolls too) |
| No automatic rollouts | `gitimage.dev/rollout: manual` on the workload's metadata |
| Jobs and CronJobs | supported; each new pod gets the current revision at creation |
| A specific commit | set `ref` to the sha (or a tag) |

A pod whose template pins a digest (`gitimage.dev/digest.<container>`) that the GitImage's
status does not show yet, because a rebuild just finished, waits gated for a moment rather than
start on the commit's previous image. A pin that never matches (you changed
`gitimage.dev/revision.<container>` by hand, or a `manual` workload's commit was rebuilt since)
is dropped after a minute: the pod gets the revision's current digest. Remove the digest
annotation when you pin a revision by hand.

A pod admitted before its image exists stays `SchedulingGated` (not `Pending` on a node, never
`ImagePullBackOff`). While it waits, it gets Events like
`Building myapp@3f2c9e1: step 4/12 RUN npm ci`, and `GitImageNotFound` if the GitImage does not
exist yet.

A pod may only reference GitImage images of its own namespace; the webhook denies anything else.

## Image scanners

The template keeps `placeholder`; only pods get the built image. Scanners that read templates
instead of pods, such as Trivy Operator for Deployments, StatefulSets and DaemonSets, scan the
placeholder. Two chart values fix that:

- `controller.pinTemplateImage: true`: the roller writes the built image into each bound
  container of the template, together with the revision annotation. It checks on every
  reconcile, so workloads created after a build and templates reset by an apply are pinned
  again. Turning it on rolls each workload once.
- `webhook.crossTenantNamespaces: [trivy-system]`: Trivy Operator in filesystem mode runs the
  scanned image in its own namespace, which the cross-tenant guard otherwise denies.

With ArgoCD, the `managedFieldsManagers` ignore rule and `RespectIgnoreDifferences` from
[GitOps with ArgoCD](argocd.md) keep the pinned image on sync. Helm and `kubectl apply` reset it
to the placeholder, which rolls the workload, and the roller then pins it again, which rolls it
once more.
