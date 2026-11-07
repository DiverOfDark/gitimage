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
| Automatic rollouts | nothing: Deployments, StatefulSets and DaemonSets get `gitimage.dev/revision.<container>` pinned on each new commit |
| No automatic rollouts | `gitimage.dev/rollout: manual` on the workload's metadata |
| Jobs and CronJobs | supported; each new pod gets the current revision at creation |
| A specific commit | set `ref` to the sha (or a tag) |

A pod admitted before its image exists stays `SchedulingGated` (not `Pending` on a node, never
`ImagePullBackOff`). While it waits, it gets Events like
`Building myapp@3f2c9e1: step 4/12 RUN npm ci`, and `GitImageNotFound` if the GitImage does not
exist yet.

A pod may only reference GitImage images of its own namespace; the webhook denies anything else.
