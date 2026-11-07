#!/usr/bin/env bash
# One-shot e2e: ./hack/e2e/run.sh <registry|s3> <privileged|gvisor|userns|<runtimeClass>>
# Creates the environment if needed, builds and loads the image, installs, runs the suite and
# the IsolationUnavailable check. Set KEEP=1 to keep the cluster afterwards.
set -euo pipefail
cd "$(dirname "$0")/../.."
STORAGE=${1:-registry}
ISOLATION=${2:-privileged}
mkdir -p hack/bin
export PATH="$PWD/hack/bin:$PATH"
if ! command -v kind >/dev/null; then
  curl -fsSL -o hack/bin/kind https://github.com/kubernetes-sigs/kind/releases/download/v0.33.0/kind-linux-amd64
  chmod +x hack/bin/kind
fi
if ! command -v kubectl >/dev/null; then
  curl -fsSL -o hack/bin/kubectl "https://dl.k8s.io/release/$(curl -fsSL https://dl.k8s.io/release/stable.txt)/bin/linux/amd64/kubectl"
  chmod +x hack/bin/kubectl
fi
./hack/e2e/up.sh
case "$ISOLATION" in
  gvisor) ./hack/e2e/gvisor.sh ;;
  kata*) ./hack/e2e/kata.sh ;;
esac
./hack/e2e/image.sh
./hack/e2e/install.sh "$STORAGE" "$ISOLATION"
export E2E_KUBECONFIG="$PWD/hack/.kubeconfig"
if [ -n "${E2E_TEST_BIN:-}" ]; then
  "$E2E_TEST_BIN" --ignored --test-threads 9   # prebuilt by the pipeline's build job
else
  cargo test -p e2e --test flow -- --ignored --test-threads 9
fi
./hack/e2e/isolation-unavailable.sh "$STORAGE" "$ISOLATION"
[ -n "${KEEP:-}" ] || ./hack/e2e/down.sh
