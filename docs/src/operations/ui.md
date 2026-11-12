# Status UI

A read-only web page served by the controller: every GitImage with its state, the live build
log grouped by BuildKit step, the workloads that use an image and the pods waiting for one.

```yaml
ui:
  enabled: true
```

```sh
kubectl -n gitimage-system port-forward svc/gitimage-ui 8082
# open http://localhost:8082
```

The UI changes nothing in the cluster. To retry a build, it shows the `kubectl annotate`
command to run.

## Sign-in

| `ui.auth.mode` | Who sees what | Use it for |
| --- | --- | --- |
| `none` (default) | Every visitor sees what the controller's ServiceAccount sees: all namespaces, all build logs. | `kubectl port-forward`, or behind a proxy that authenticates. Do not expose it otherwise. |
| `oidc` | Each user sees what cluster RBAC allows them. | A shared URL. |

### OIDC

The UI signs the user in (authorization code flow with PKCE) and sends their ID token to the
Kubernetes API server as the bearer token of every request it makes for them. The API server
authenticates the token and applies RBAC, the same model as Headlamp. The controller's own
permissions are never used for a signed-in user.

Requirements:

1. The API server trusts the issuer (`--oidc-*` flags or a structured
   `AuthenticationConfiguration`), and the UI's client id is one of its accepted audiences.
2. The client has the redirect URI `<ui.baseURL>/auth/callback`.
3. Users have RBAC. To see a GitImage in a namespace:

   | To see | Needs |
   | --- | --- |
   | It in the list | `list` on `gitimages.gitimage.dev` in the namespace (or cluster-wide) |
   | Its page | `get` on that GitImage |
   | Workloads | `list` on `deployments`, `statefulsets`, `daemonsets` |
   | Waiting pods | `list` on `pods` |
   | Build logs | `list` and `get` on `pods`, `get` on `pods/log` |

   What a user may not list is shown as a notice on the page, not as an error.

```yaml
ui:
  enabled: true
  baseURL: https://gitimage.example.com
  auth:
    mode: oidc
    oidc:
      issuer: https://auth.example.com     # exactly as the API server has it
      clientID: "312345678901234567"       # quote numeric ids; with helm use --set-string
      existingSecret: gitimage-ui-oidc     # keys: clientSecret, optionally clientID
  ingress:
    enabled: true
    host: gitimage.example.com
```

A session lasts as long as the ID token. When it expires the browser is sent through the
issuer again, which is invisible while the issuer's own session is alive. The session is an
encrypted cookie, so both controller replicas accept it and nothing is stored server-side.

If the API server rejects the token, the UI says so. The usual cause is an audience mismatch:
the API server accepts only the client ids listed in its configuration.

## Limits

- **Logs live one hour.** They are read from the build pod, and build Jobs are removed one
  hour after they finish. After that the page shows the failure message kept in
  `status.builds`.
- **Build output is visible to everyone who may read pod logs in the namespace.** A
  Dockerfile that prints a build argument prints it there too. Use `build.secrets` for
  credentials; secret mounts never reach the log.
- The page of a GitImage shows its push-hook URL to anyone who may `get` the GitImage, the
  same as `kubectl get gi -o yaml`.

## Developing the UI

The web app lives in `ui/` (Svelte 5, Vite, Tailwind) and is embedded into the binary at build
time. Without a built `ui/dist`, the binary serves a placeholder page.

```sh
cd ui && npm ci
npm run dev                                        # against mock data, no cluster needed
GITIMAGE_UI_PROXY=http://localhost:8082 npm run dev   # against a port-forwarded controller
npm run check && npm run build                     # then cargo build embeds ui/dist
```
