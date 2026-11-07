#!/usr/bin/env bash
# Builds gitimage:e2e and loads it into the kind cluster.
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH="$PWD/hack/bin:$PATH"
CLUSTER=${CLUSTER:-gitimage-e2e}
DOCKER_BUILDKIT=1 docker build -f hack/e2e/Dockerfile.dev -t gitimage:e2e .
kind load docker-image gitimage:e2e --name "$CLUSTER"
