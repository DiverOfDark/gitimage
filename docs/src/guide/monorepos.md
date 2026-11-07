# Monorepos

Create one GitImage per image, all with the same `url` and a different `build.context`:

```yaml
spec: { url: git@github.com:you/mono.git, build: { context: services/api } }
---
spec: { url: git@github.com:you/mono.git, build: { context: services/web } }
```

A commit that does not change the git tree of `context` is recorded as `Reused`: the previous
image is pinned to the new commit and nothing is built. The tree id comes from the host's API for
GitHub, GitLab and Gitea/Forgejo over https; otherwise the build Job's clone step compares it and
stops before BuildKit starts.
