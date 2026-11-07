#!/usr/bin/env bash
# Runs the OCI distribution-spec conformance suite against gitimage-oci's push router over
# MemoryStorage (examples/conformance_server.rs), with no auth.
#
#   hack/oci-conformance.sh            # build the suite with Go (cached in target/)
#   OCI_CONFORMANCE_IMAGE=1 hack/...   # use the published container image instead (podman/docker)
#
# Reports land in target/oci-conformance/ (report.html, junit.xml).
set -euo pipefail

SPEC_VERSION="${SPEC_VERSION:-v1.1.1}"
PORT="${PORT:-5055}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/target/oci-conformance"
mkdir -p "$OUT"

cargo build --manifest-path "$ROOT/Cargo.toml" -p gitimage-oci --example conformance_server
"$ROOT/target/debug/examples/conformance_server" "127.0.0.1:$PORT" >"$OUT/server.log" 2>&1 &
SERVER=$!
trap 'kill "$SERVER" 2>/dev/null || true' EXIT
for _ in $(seq 50); do
  curl -fsS "http://127.0.0.1:$PORT/v2/" >/dev/null 2>&1 && break
  sleep 0.2
done

export OCI_ROOT_URL="http://127.0.0.1:$PORT"
export OCI_NAMESPACE="myorg/myrepo"
export OCI_CROSSMOUNT_NAMESPACE="myorg/other"
export OCI_TEST_PULL=1 OCI_TEST_PUSH=1 OCI_TEST_CONTENT_DISCOVERY=1 OCI_TEST_CONTENT_MANAGEMENT=1
export OCI_HIDE_SKIPPED_WORKFLOWS=0 OCI_DEBUG="${OCI_DEBUG:-0}" OCI_DELETE_MANIFEST_BEFORE_BLOBS=1
export OCI_REPORT_DIR="$OUT"
ENV_NAMES=(OCI_ROOT_URL OCI_NAMESPACE OCI_CROSSMOUNT_NAMESPACE OCI_TEST_PULL OCI_TEST_PUSH
  OCI_TEST_CONTENT_DISCOVERY OCI_TEST_CONTENT_MANAGEMENT OCI_HIDE_SKIPPED_WORKFLOWS OCI_DEBUG
  OCI_DELETE_MANIFEST_BEFORE_BLOBS)

if [[ -n "${OCI_CONFORMANCE_IMAGE:-}" ]]; then
  ENGINE="$(command -v podman || command -v docker)"
  args=()
  for n in "${ENV_NAMES[@]}"; do args+=(-e "$n"); done
  "$ENGINE" run --rm --network host "${args[@]}" -e OCI_REPORT_DIR=/report -v "$OUT:/report:Z" \
    "ghcr.io/opencontainers/distribution-spec/conformance:$SPEC_VERSION"
else
  SRC="$ROOT/target/distribution-spec-$SPEC_VERSION"
  if [[ ! -x "$SRC/conformance/conformance.test" ]]; then
    rm -rf "$SRC"
    git clone --depth 1 --branch "$SPEC_VERSION" https://github.com/opencontainers/distribution-spec "$SRC"
    (cd "$SRC/conformance" && go test -c -o conformance.test)
  fi
  (cd "$OUT" && "$SRC/conformance/conformance.test")
fi
