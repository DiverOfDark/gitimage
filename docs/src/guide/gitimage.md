# The GitImage resource

`gitimage.dev/v1alpha1`, kind `GitImage`, namespaced, short name `gi`.

```yaml
apiVersion: gitimage.dev/v1alpha1
kind: GitImage
metadata: { name: myapp, namespace: team-a }
spec:
  url: git@github.com:you/myapp.git   # ssh (git@host:path, ssh://...) or https
  ref: main                           # branch | tag | full sha | refs/heads/... | refs/tags/...
  secretRef: { name: myapp-git }      # optional, same namespace
  interval: 5m                        # poll interval; hooks trigger earlier
  build:
    context: .                        # repo-relative build directory
    dockerfile: Dockerfile            # relative to context
    args: {}                          # see Credentials, build secrets and args
    secrets: []
    resources: { requests: { cpu: "1", memory: 2Gi }, limits: { memory: 4Gi } }
    onNewCommit: wait                 # wait | cancel
```

## Status

| Field | Meaning |
| --- | --- |
| `latestRevision` | newest commit seen on the ref |
| `revision` | commit whose image is current |
| `image` | `<pull host>/<namespace>/<name>@sha256:...` |
| `builds[]` | newest first, up to 10: `revision`, `outcome` (`Built`, `Reused`, `Superseded`, `Failed`), `digest`, `contextTree`, `sourceRevision`, `reason`, `attempt` (the build Job's), `finishedAt` |
| `webhookURL` | push-hook URL to paste into the git host (needs `hooks.baseURL`) |
| `handledRebuild` | the last `gitimage.dev/rebuild` value whose rebuild finished; a different annotation value is a rebuild still to do |
| `conditions` | `Ready`, `Building` (message = current BuildKit step), `Failed` (reason) |

Failure reasons: `ResolveFailed`, `SecretMissing`, `CloneFailed`, `BuildFailed`, `PushFailed`,
`DeadlineExceeded`, `PodCreationFailed`, `ReconcileError`.

`ReconcileError` means the controller itself failed, for example because the API server
rejected the build Job or a quota forbids it; the message is the API server's. The controller
retries every 30 seconds, and the next successful reconcile clears the condition. If the
GitImage has no image yet, `Ready` is `False` with the same reason, so `kubectl get gi` shows it.

## Behaviour

- **One build at a time** per GitImage. With `onNewCommit: wait` (default) the running build
  finishes, then only the newest commit is built; with `cancel` the running build is deleted.
- **A failed build is not retried automatically.** Push a new commit or bump the annotation:
  `kubectl annotate gi myapp gitimage.dev/rebuild=$(date +%s) --overwrite`.
- **Any change of `gitimage.dev/rebuild` rebuilds the current commit**, whatever its last
  outcome (`Built`, `Reused`, `Failed`, `Superseded`); see [Rebuilding a commit](#rebuilding-a-commit).
- **Pod-level failures** (eviction, node loss) are retried twice.
- **Deleting the GitImage** deletes its Jobs and hook Secret; stored images are left to GC.

## Rebuilding a commit

Set `gitimage.dev/rebuild` on the GitImage to a new value to build the current commit again
without a new commit:

```sh
kubectl annotate gi myapp gitimage.dev/rebuild=$(date +%s) --overwrite
```

- The rebuild always runs a build Job: it never ends as `Reused`, even if the context did not
  change, and it runs even when the commit's earlier Jobs are already gone.
- The value reaches the Dockerfile as the build arg `GITIMAGE_REBUILD` (empty without the
  annotation). BuildKit otherwise reuses every cached layer; a Dockerfile that declares
  `ARG GITIMAGE_REBUILD` misses the cache from that line on, so the steps after it run again.
- While it runs, `status.image` and `status.revision` stay on the previous image and workloads
  keep running it. When it succeeds, the commit's record gets the new digest and workloads
  roll to it (the roller pins `gitimage.dev/digest.<container>` next to the revision). If the
  digest is unchanged, nothing rolls.
- If it fails, the failure is recorded (`Failed` condition, `BuildFailed` event) and the
  previous image stays current: the `Failed` record keeps its digest.
- A request made while a build runs (of this commit, or of an older one with
  `onNewCommit: wait`) waits for that build, then rebuilds. `status.handledRebuild` takes the
  value once the rebuild's result is recorded.
- A rebuild that has started always finishes and is recorded: removing the annotation, or
  setting it back to `status.handledRebuild`, does not cancel it, and a newer value rebuilds
  again after it.
- Only the value matters, not what it is; a request is never repeated by itself. Do not keep
  the annotation in a GitOps-managed manifest, or a sync that resets it starts a rebuild.

### Daily rebuilds

A site that filters content by date (posts published in the future) has to be rebuilt even when
no commit changed. Declare the arg before the build step:

```dockerfile
FROM node:24 AS build
WORKDIR /src
COPY package*.json ./
RUN npm ci
COPY . .
# Changes on every rebuild: the build below runs again, npm ci stays cached.
ARG GITIMAGE_REBUILD
RUN npm run build
```

And bump the annotation from a CronJob in the GitImage's namespace (`kubectl annotate` reads
the object first, so the Role needs `get` and `patch`):

```yaml
apiVersion: v1
kind: ServiceAccount
metadata: { name: site-rebuild, namespace: team-a }
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata: { name: site-rebuild, namespace: team-a }
rules:
  - apiGroups: [gitimage.dev]
    resources: [gitimages]
    resourceNames: [site]
    verbs: [get, patch]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata: { name: site-rebuild, namespace: team-a }
roleRef: { apiGroup: rbac.authorization.k8s.io, kind: Role, name: site-rebuild }
subjects: [{ kind: ServiceAccount, name: site-rebuild, namespace: team-a }]
---
apiVersion: batch/v1
kind: CronJob
metadata: { name: site-rebuild, namespace: team-a }
spec:
  schedule: "5 0 * * *"            # daily, 00:05
  timeZone: Etc/UTC
  concurrencyPolicy: Forbid
  jobTemplate:
    spec:
      backoffLimit: 2
      template:
        spec:
          serviceAccountName: site-rebuild
          restartPolicy: Never
          containers:
            - name: annotate
              image: docker.io/alpine/kubectl:1.37.2
              command: [sh, -c, 'kubectl annotate gi site gitimage.dev/rebuild=$(date +%s) --overwrite']
```
