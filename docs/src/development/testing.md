# Testing

```sh
cargo test --workspace                 # unit, storage, router, snapshot tests
./hack/oci-conformance.sh              # OCI distribution-spec conformance
./hack/check-chart-config.sh           # chart renders configs the controller accepts
./hack/e2e/run.sh s3 gvisor            # kind e2e: registry|s3 x privileged|gvisor|kata-qemu
KEEP=1 ./hack/e2e/run.sh registry privileged   # keep the cluster for debugging
```

The e2e environment is a kind cluster with its own kubeconfig (`hack/.kubeconfig`) plus a
registry, SeaweedFS (S3) and Gitea as containers on the `kind` network. The suite drives Gitea
through its API and covers: commit to running digest, rollouts, tags, monorepo reuse, hooks,
build secrets, private repos over ssh with host-key checks, the cross-tenant guard, failed builds
and rebuilds, GC, and `IsolationUnavailable`.
