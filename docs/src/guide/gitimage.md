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
| `builds[]` | newest first, up to 10: `revision`, `outcome` (`Built`, `Reused`, `Superseded`, `Failed`), `digest`, `contextTree`, `sourceRevision`, `reason`, `finishedAt` |
| `webhookURL` | push-hook URL to paste into the git host (needs `hooks.baseURL`) |
| `conditions` | `Ready`, `Building` (message = current BuildKit step), `Failed` (reason) |

Failure reasons: `ResolveFailed`, `SecretMissing`, `CloneFailed`, `BuildFailed`, `PushFailed`,
`DeadlineExceeded`.

## Behaviour

- **One build at a time** per GitImage. With `onNewCommit: wait` (default) the running build
  finishes, then only the newest commit is built; with `cancel` the running build is deleted.
- **A failed build is not retried automatically.** Push a new commit or bump the annotation:
  `kubectl annotate gi myapp gitimage.dev/rebuild=$(date +%s) --overwrite`.
- **Pod-level failures** (eviction, node loss) are retried twice.
- **Deleting the GitImage** deletes its Jobs and hook Secret; stored images are left to GC.
