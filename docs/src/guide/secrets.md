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
