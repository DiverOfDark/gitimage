#!/usr/bin/env bash
# Renders the chart with several value sets and checks each config.yaml with the strict parser.
set -euo pipefail
cd "$(dirname "$0")/.."
[ -n "${GITIMAGE_BIN:-}" ] || cargo build -q -p gitimage
bin=${GITIMAGE_BIN:-target/debug/gitimage}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
check() {
  # The literal block under `config.yaml: |`, de-indented (no yq/PyYAML needed).
  helm template t charts/gitimage -n gitimage-system --show-only templates/configmap.yaml "$@" \
    | awk '/^  config.yaml: \|/ {on=1; next} on && /^    / {print substr($0, 5); next} on && !/^    / && NF {on=0}' \
    > "$tmp/config.yaml"
  "$bin" check-config --config "$tmp/config.yaml"
}
check --set buildPolicy.isolation.mode=userns --set storage.registry.url=registry.example.com/builds --set storage.registry.pushSecret=push --set storage.registry.pullSecret=pull
check --set buildPolicy.isolation.mode=runtimeClass --set buildPolicy.isolation.runtimeClassName=kata \
  --set storage.type=s3 --set storage.s3.bucket=gi --set storage.s3.endpoint=http://rgw --set storage.s3.secret=s3 \
  --set hooks.baseURL=https://gitimage.example.com --set 'network.clusterCIDRs={10.244.0.0/16,10.96.0.0/12}' \
  --set build.defaultResources.requests.cpu=1
check --set buildPolicy.isolation.mode=privileged --set buildPolicy.isolation.allowPrivileged=true \
  --set storage.registry.url=r/b --set storage.registry.pushSecret=p --set build.cache=off
if helm template t charts/gitimage --set storage.registry.url=r --set storage.registry.pushSecret=p >/dev/null 2>&1; then
  echo "FAIL: chart rendered without buildPolicy.isolation.mode"; exit 1
fi
echo "chart config: all value sets parse"
