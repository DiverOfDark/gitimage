#!/usr/bin/env bash
# Installs Sysbox CE (sysbox-runc, sysbox-mgr, sysbox-fs) into the kind worker nodes and creates
# RuntimeClass sysbox-runc. Done by hand instead of with sysbox-deploy-k8s because that
# installer applies host sysctls (kernel.pid_max, vm.max_map_count, keyrings, inotify): kind
# nodes share the host kernel, so it would reconfigure the machine running the tests. It also
# rejects Kubernetes versions newer than it was tested with. containerd >= 2.0.5 runs Sysbox
# pods with hostUsers: false, so no CRI-O is needed.
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH="$PWD/hack/bin:$PATH"
export KUBECONFIG="$PWD/hack/.kubeconfig"
CLUSTER=${CLUSTER:-gitimage-e2e}
VERSION=${SYSBOX_VERSION:-0.7.1}
SHA256=${SYSBOX_SHA256:-9d6d5484f980d0a17f86c492c1262015c2afb66280bdb97215b79fde6a0261c5}
DEB=sysbox-ce_${VERSION}.linux_amd64.deb
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
curl -fsSL "https://github.com/nestybox/sysbox/releases/download/v${VERSION}/${DEB}" -o "$tmp/$DEB"
echo "$SHA256  $tmp/$DEB" | sha256sum -c - >/dev/null
for node in $(kind get nodes --name "$CLUSTER"); do
  [[ $node == *control-plane* ]] && continue   # build pods run on workers only
  docker cp "$tmp/$DEB" "$node:/opt/$DEB"      # not /tmp: a tmpfs in kind nodes
  docker exec -e DEB="$DEB" "$node" bash -euo pipefail -c '
    if ! systemctl is-active --quiet sysbox; then
      command -v rsync >/dev/null && command -v fusermount3 >/dev/null ||
        { export DEBIAN_FRONTEND=noninteractive; apt-get update -qq && apt-get install -y -qq --no-install-recommends rsync fuse3 >/dev/null; }
      # Binaries and systemd units only; the package scripts would apply host sysctls.
      rm -rf /opt/sysbox-pkg && dpkg-deb -x "/opt/$DEB" /opt/sysbox-pkg
      cp /opt/sysbox-pkg/usr/bin/sysbox-* /usr/bin/
      cp /opt/sysbox-pkg/lib/systemd/system/sysbox*.service /lib/systemd/system/
      getent passwd sysbox >/dev/null || useradd -r -s /bin/false sysbox
      mkdir -p /var/lib/sysboxfs
      systemctl daemon-reload
      systemctl enable --now sysbox.service >/dev/null 2>&1
    fi
    systemctl is-active --quiet sysbox-mgr sysbox-fs
    # kind-only. A new sysfs mount in a pod user namespace is allowed only if the node'"'"'s /sys
    # is fully visible and not locked read-only. kind mounts /sys read-only and bind-mounts
    # files over /sys/devices/virtual/dmi/id/product_{name,uuid}; either makes the kernel refuse
    # ("sysfs mount through procfd to sys caused: operation not permitted").
    for f in /sys/devices/virtual/dmi/id/product_uuid /sys/devices/virtual/dmi/id/product_name; do
      while mountpoint -q "$f"; do umount "$f"; done
    done
    mount -o remount,rw /sys
    cfg=/etc/containerd/config.toml
    if ! grep -q "runtimes.sysbox-runc" "$cfg"; then
      if grep -q "io.containerd.cri.v1.runtime" "$cfg"; then
        p="io.containerd.cri.v1.runtime"
      else
        p="io.containerd.grpc.v1.cri"
      fi
      cat >> "$cfg" <<TOML

[plugins."$p".containerd.runtimes.sysbox-runc]
  runtime_type = "io.containerd.runc.v2"
  [plugins."$p".containerd.runtimes.sysbox-runc.options]
    BinaryName = "/usr/bin/sysbox-runc"
    SystemdCgroup = true
TOML
      systemctl restart containerd
    fi'
  kubectl label node "$node" sysbox-runtime=running --overwrite >/dev/null
done
cat <<YAML | kubectl apply -f -
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata: { name: sysbox-runc }
handler: sysbox-runc
scheduling:
  nodeSelector: { sysbox-runtime: running }
YAML
kubectl wait --for=condition=Ready nodes --all --timeout=120s >/dev/null
echo "Sysbox $VERSION installed; RuntimeClass sysbox-runc ready"
