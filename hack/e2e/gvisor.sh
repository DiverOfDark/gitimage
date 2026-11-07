#!/usr/bin/env bash
# Installs gVisor (runsc + containerd shim) into every kind node and creates RuntimeClass gvisor.
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH="$PWD/hack/bin:$PATH"
export KUBECONFIG="$PWD/hack/.kubeconfig"
CLUSTER=${CLUSTER:-gitimage-e2e}
URL=https://storage.googleapis.com/gvisor/releases/release/latest/x86_64
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$URL/gvisor.tar.bz2" -o "$tmp/gvisor.tar.bz2"
curl -fsSL "$URL/gvisor.tar.bz2.sha512" -o "$tmp/gvisor.tar.bz2.sha512"
(cd "$tmp" && sha512sum -c gvisor.tar.bz2.sha512 >/dev/null)
mkdir "$tmp/x"
if command -v bzip2 >/dev/null; then
  tar -xjf "$tmp/gvisor.tar.bz2" -C "$tmp/x"
else
  # No bzip2 on the runner image: decompress in a container, stream a plain tar back
  # (stdin/stdout, so it also works with a remote or sidecar docker daemon).
  docker run --rm -i "${E2E_DOCKERHUB_PREFIX:-docker.io/}library/busybox:1.37" sh -c 'mkdir /x && tar -xjf - -C /x && tar -cf - -C /x .' \
    < "$tmp/gvisor.tar.bz2" | tar -xf - -C "$tmp/x"
fi
[ -x "$tmp/x/runsc" ] && [ -x "$tmp/x/containerd-shim-runsc-v1" ] || { echo "unexpected gvisor.tar.bz2 layout"; exit 1; }
for node in $(kind get nodes --name "$CLUSTER"); do
  # runsc finds its helpers in gvisor-bin/ next to itself.
  docker cp "$tmp/x/." "$node:/usr/local/bin/"
  if ! docker exec "$node" grep -q 'runtimes.runsc' /etc/containerd/config.toml; then
    docker exec "$node" sh -c 'cat >> /etc/containerd/config.toml <<TOML

[plugins."io.containerd.cri.v1.runtime".containerd.runtimes.runsc]
  runtime_type = "io.containerd.runsc.v1"
TOML'
    docker exec "$node" systemctl restart containerd
  fi
done
cat <<YAML | kubectl apply -f -
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata: { name: gvisor }
handler: runsc
YAML
kubectl wait --for=condition=Ready nodes --all --timeout=120s >/dev/null
echo "gVisor installed; RuntimeClass gvisor ready"
