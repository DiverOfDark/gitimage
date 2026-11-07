#!/usr/bin/env bash
# Builds gitimage:e2e and loads it into the kind cluster.
# GITIMAGE_BINARY=<path>: package a prebuilt binary (CI; seconds). Otherwise a cached cargo
# build inside docker (local development).
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH="$PWD/hack/bin:$PATH"
CLUSTER=${CLUSTER:-gitimage-e2e}
if [ -n "${GITIMAGE_BINARY:-}" ]; then
  ctx=$(mktemp -d); trap 'rm -rf "$ctx"' EXIT
  cp "$GITIMAGE_BINARY" "$ctx/gitimage-amd64"   # mode as downloaded: Dockerfile.release must fix it
  docker build -q -f Dockerfile.release --build-arg TARGETARCH=amd64 -t gitimage:e2e "$ctx"
else
  DOCKER_BUILDKIT=1 docker build -f hack/e2e/Dockerfile.dev -t gitimage:e2e .
fi
kind load docker-image gitimage:e2e --name "$CLUSTER"
