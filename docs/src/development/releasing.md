# Releasing

Every push to `main` runs one pipeline: checks (fmt, clippy, tests, chart, cargo-deny), OCI
conformance, one build per architecture, the e2e matrix, and only if all of them pass, a
release:

- version `<major>.<minor>.<run number>` (major.minor from `Cargo.toml`);
- multi-arch image `ghcr.io/diverofdark/gitimage:<version>` and `:latest`, cosign-signed;
- Helm chart `oci://ghcr.io/diverofdark/charts/gitimage --version <version>`;
- a GitHub release with binaries and notes;
- this documentation, published to GitHub Pages.

Runners: the repository variable `RUNS_ON` names a self-hosted runner label for every job
(`E2E_RUNS_ON` overrides it for e2e only); unset means GitHub-hosted. Pull requests from forks
always use GitHub-hosted runners, and arm64 builds always use GitHub's arm runner.
`E2E_ISOLATIONS` (JSON list) adds `kata-qemu` where runners have `/dev/kvm`;
`DOCKERHUB_PREFIX` (e.g. `harbor.example.com/docker-hub/`) makes self-hosted jobs pull every
Docker Hub image through a path-style proxy cache: job containers, kind's node image, the e2e
helpers, containerd in kind (`hosts.toml` with `override_path`) and BuildKit (`mirrors`).
`E2E_GITHUB_ISOLATIONS` (JSON list) keeps some isolations on GitHub-hosted runners — gVisor and
rootless BuildKit need user namespaces, which Talos disables by default
(`user.max_user_namespaces=0`).
