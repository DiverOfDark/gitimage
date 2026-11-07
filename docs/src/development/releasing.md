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
`E2E_ISOLATIONS` (JSON list) adds `kata-qemu` where runners have `/dev/kvm`.
