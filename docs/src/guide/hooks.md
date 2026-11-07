# Push hooks

Polling every `interval` always works. For builds within seconds of a push:

1. Expose the hooks Service: `hooks.baseURL: https://gitimage.example.com` and
   `hooks.ingress.enabled: true` (or your own Ingress/Gateway to Service `gitimage-hooks:8080`,
   path prefix `/hooks/`).
2. Copy the URL: `kubectl get gi myapp -o jsonpath='{.status.webhookURL}'`.
3. Add it as a push webhook in GitHub, GitLab, Gitea or Forgejo (any content type, no secret).

The URL carries a random token (Secret `<name>-gitimage-hook`; delete it to rotate). A hook only
triggers a re-check of the ref: its payload is never trusted. Hooks for one repository wake every
GitImage in that namespace with the same repository URL.
