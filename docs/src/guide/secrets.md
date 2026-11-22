# Credentials, build secrets and args

## Git credentials

`spec.secretRef` names a Secret in the GitImage's namespace (Flux format):

| Transport | Keys |
| --- | --- |
| ssh | `identity` (OpenSSH private key: ed25519, ecdsa or rsa; no passphrase), `known_hosts` (required: unknown host keys are refused) |
| https | `username`, `password` (a token works; `username` defaults to `git`) |

```sh
kubectl -n team-a create secret generic myapp-git \
  --from-file=identity=./deploy_key --from-literal=known_hosts="$(ssh-keyscan github.com)"
```

The controller reads it to list refs; the clone init container of each build mounts it
read-only. It is never copied anywhere else. Over plain `http://` no credentials are sent.

## Build secrets

For private package registries, without baking credentials into layers:

```yaml
build:
  secrets: [{ id: npmrc, secretRef: { name: npm-token, key: npmrc } }]   # key optional if the Secret has one key
```

```dockerfile
RUN --mount=type=secret,id=npmrc,target=/root/.npmrc npm ci
```

The secret exists in tmpfs for that `RUN` only; it is never in a layer, the cache or the history.
Repository code in that step can read it, so give secrets only to repositories you trust.

## Build args

```yaml
build:
  args:
    NODE_ENV: { value: production }
    SENTRY_RELEASE: { valueFrom: { secretKeyRef: { name: sentry, key: release } } }
```

Build args end up in the image history; use build secrets for real secrets.

### Commit metadata

Every build also gets three args from GitImage, set after `build.args` so they cannot be
overridden:

| Arg | Value |
| --- | --- |
| `GITIMAGE_REVISION` | full commit sha |
| `GITIMAGE_COMMIT_TIME` | the commit's committer time, unix seconds |
| `GITIMAGE_REBUILD` | the GitImage's `gitimage.dev/rebuild` annotation, empty without one |

The build sees no `.git` (the clone is one commit deep), so these are how a Dockerfile learns
which commit it is building. Declare the ones you use:

```dockerfile
ARG GITIMAGE_REVISION=unknown
ARG GITIMAGE_COMMIT_TIME=0
```

`GITIMAGE_COMMIT_TIME` only grows along a branch that is not rewritten, so it works as a
monotonic version number (an Android `versionCode`, for example: it stays under Android's
2100000000 cap until mid-2036).

Undeclared, they change nothing: BuildKit ignores unused args. Declared, they change on every
commit and so invalidate the cache of every later `RUN` in that stage: declare them in the
stage that needs them, as late as possible.

`GITIMAGE_REBUILD` changes only when the annotation does. Declare it just before the steps that
must run again on a [rebuild](gitimage.md#rebuilding-a-commit) without a new commit (a site
build that filters posts by date, for example); everything above it stays cached:

```dockerfile
ARG GITIMAGE_REBUILD
RUN npm run build
```
