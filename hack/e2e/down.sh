#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH="$PWD/hack/bin:$PATH"
kind delete cluster --name "${CLUSTER:-gitimage-e2e}" --kubeconfig "$PWD/hack/.kubeconfig" || true
docker rm -f kind-registry e2e-s3 e2e-gitea >/dev/null 2>&1 || true
