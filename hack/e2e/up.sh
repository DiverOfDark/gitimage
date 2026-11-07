#!/usr/bin/env bash
# Creates the e2e environment: a kind cluster with its OWN kubeconfig (hack/.kubeconfig),
# plus a registry, MinIO and Gitea as containers on the `kind` docker network, reachable by
# name from pods (via CoreDNS -> docker DNS) and from nodes.
# Never touches ~/.kube/config.
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH="$PWD/hack/bin:$PATH"
export KUBECONFIG="$PWD/hack/.kubeconfig"
CLUSTER=${CLUSTER:-gitimage-e2e}
# E2E_DOCKERHUB_PREFIX (e.g. harbor.example.com/docker-hub/): pull every Docker Hub image
# through a path-style proxy cache instead of Docker Hub (rate limits on self-hosted runners).
DH=${E2E_DOCKERHUB_PREFIX:-docker.io/}
GITEA_IMAGE=${GITEA_IMAGE:-${DH}gitea/gitea:1.27-rootless}
S3_IMAGE=${S3_IMAGE:-${DH}chrislusf/seaweedfs:4.50}
REGISTRY_IMAGE=${REGISTRY_IMAGE:-${DH}library/registry:3}
KIND_NODE_IMAGE=${KIND_NODE_IMAGE:-${DH}kindest/node:v1.37.0}

if ! kind get clusters | grep -qx "$CLUSTER"; then
  kind create cluster --name "$CLUSTER" --config hack/e2e/kind.yaml --kubeconfig "$KUBECONFIG" \
    --image "$KIND_NODE_IMAGE" --wait 120s
fi

run_once() { # name, args...
  local name=$1; shift
  if [ -z "$(docker ps -q -f name="^${name}$")" ]; then
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker run -d --restart=unless-stopped --name "$name" --network kind "$@" >/dev/null
  fi
}
run_once kind-registry -p 127.0.0.1:5001:5000 "$REGISTRY_IMAGE"
# Docker Hub pull-through cache for kind's containerd and for BuildKit in build pods: each image
# is pulled from upstream once per environment instead of once per build (rate limits, speed).
mirror_auth=()
[ -n "${E2E_DOCKERHUB_USERNAME:-}" ] && mirror_auth=(-e "REGISTRY_PROXY_USERNAME=$E2E_DOCKERHUB_USERNAME" -e "REGISTRY_PROXY_PASSWORD=$E2E_DOCKERHUB_PASSWORD")
[ -n "${E2E_DOCKERHUB_PREFIX:-}" ] || run_once e2e-mirror -e "REGISTRY_PROXY_REMOTEURL=${E2E_DOCKERHUB_UPSTREAM:-https://registry-1.docker.io}" \
  "${mirror_auth[@]}" "$REGISTRY_IMAGE"
# S3: SeaweedFS (MinIO no longer publishes images).
run_once e2e-s3 -p 127.0.0.1:8333:8333 -v "$PWD/hack/e2e/s3.json:/etc/seaweedfs/s3.json:ro" \
  "$S3_IMAGE" server -dir=/data -master.volumeSizeLimitMB=1024 -volume.max=64 -s3 -s3.port=8333 -s3.config=/etc/seaweedfs/s3.json
run_once e2e-gitea -p 127.0.0.1:3300:3000 -p 127.0.0.1:2222:2222 \
  -e GITEA__security__INSTALL_LOCK=true -e GITEA__server__ROOT_URL=http://e2e-gitea:3000/ \
  -e GITEA__server__SSH_DOMAIN=e2e-gitea -e GITEA__server__SSH_PORT=2222 -e GITEA__server__START_SSH_SERVER=true \
  -e GITEA__webhook__ALLOWED_HOST_LIST='*' -e GITEA__repository__DEFAULT_BRANCH=main \
  -e GITEA__database__DB_TYPE=sqlite3 "$GITEA_IMAGE"

# Registry over plain HTTP, and docker.io through the mirror, for containerd on every node.
for node in $(kind get nodes --name "$CLUSTER"); do
  docker exec "$node" mkdir -p /etc/containerd/certs.d/kind-registry:5000 /etc/containerd/certs.d/docker.io
  printf '[host."http://kind-registry:5000"]\n  capabilities = ["pull", "resolve"]\n' |
    docker exec -i "$node" tee /etc/containerd/certs.d/kind-registry:5000/hosts.toml >/dev/null
  if [ -n "${E2E_DOCKERHUB_PREFIX:-}" ]; then
    proxy=${E2E_DOCKERHUB_PREFIX%/}   # harbor.example.com/docker-hub
    printf 'server = "https://registry-1.docker.io"\n\n[host."https://%s/v2/%s"]\n  capabilities = ["pull", "resolve"]\n  override_path = true\n' \
      "${proxy%%/*}" "${proxy#*/}"
  else
    printf 'server = "https://registry-1.docker.io"\n\n[host."http://e2e-mirror:5000"]\n  capabilities = ["pull", "resolve"]\n'
  fi | docker exec -i "$node" tee /etc/containerd/certs.d/docker.io/hosts.toml >/dev/null
done

# Gitea admin + S3 bucket.
for i in $(seq 1 60); do
  curl -sf http://127.0.0.1:3300/api/v1/version >/dev/null && break; sleep 1
done
docker exec e2e-gitea gitea admin user create --admin --username e2e --password e2e-password \
  --email e2e@example.invalid --must-change-password=false >/dev/null 2>&1 || true
for i in $(seq 1 60); do
  echo "s3.bucket.create -name gitimage" | docker exec -i e2e-s3 weed shell -master=localhost:9333 >/dev/null 2>&1 && break
  sleep 1
done
echo "s3.bucket.list" | docker exec -i e2e-s3 weed shell -master=localhost:9333 2>/dev/null | grep -q gitimage

kubectl get nodes -o wide
echo "e2e environment ready. KUBECONFIG=$KUBECONFIG"
