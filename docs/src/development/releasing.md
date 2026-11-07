# Releasing

Every push to `main` runs one pipeline: checks (fmt, clippy, tests, chart, cargo-deny), OCI
conformance, one build per architecture, the e2e matrix, and only if all of them pass, a
release:

- version `<major>.<minor>.<run number>` (major.minor from `Cargo.toml`);
- multi-arch image `ghcr.io/diverofdark/gitimage:<version>` and `:latest`, cosign-signed;
- Helm chart `oci://ghcr.io/diverofdark/charts/gitimage --version <version>`;
- a GitHub release with binaries and notes;
- this documentation, published to GitHub Pages.

The e2e jobs run on self-hosted runners when the repository variable `E2E_RUNS_ON` names
their label (pull requests from forks always use GitHub-hosted runners); `E2E_ISOLATIONS`
(JSON list) adds `kata-qemu` where runners have `/dev/kvm`.
