#!/usr/bin/env bash
# Installs Kata Containers into the kind nodes with kata-deploy (needs /dev/kvm on the host;
# kind's privileged node containers see it). Creates RuntimeClasses kata-qemu, kata-clh, ...
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH="$PWD/hack/bin:$PATH"
export KUBECONFIG="$PWD/hack/.kubeconfig"
VERSION=${KATA_VERSION:-4.3.1}
CLUSTER=${CLUSTER:-gitimage-e2e}
# Kata backs guest RAM with a file in /dev/shm; docker gives kind nodes only 64 MiB, which
# fails with "kvm run failed Bad address".
for node in $(kind get nodes --name "$CLUSTER"); do
  docker exec "$node" mount -o remount,size=8G /dev/shm
done
helm upgrade --install kata-deploy oci://ghcr.io/kata-containers/kata-deploy-charts/kata-deploy \
  --version "$VERSION" -n kube-system --wait --timeout 10m
for i in $(seq 1 120); do
  kubectl get runtimeclass kata-qemu >/dev/null 2>&1 && \
  [ "$(kubectl get nodes -l katacontainers.io/kata-runtime=true --no-headers | wc -l)" -ge 1 ] && break
  sleep 5
done
kubectl get runtimeclass
echo "Kata installed"
