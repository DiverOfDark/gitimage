#!/usr/bin/env bash
# Installs (or upgrades) the chart for one storage mode: ./install.sh registry|s3 [isolation-mode]
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH="$PWD/hack/bin:$PATH"
export KUBECONFIG="$PWD/hack/.kubeconfig"
STORAGE=${1:-registry}
ISOLATION=${2:-userns}
NS=gitimage-system
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl -n "$NS" create secret docker-registry registry-push --docker-server=kind-registry:5000 \
  --docker-username=e2e --docker-password=e2e --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl -n "$NS" create secret generic s3-creds --from-literal=AWS_ACCESS_KEY_ID=gitimage \
  --from-literal=AWS_SECRET_ACCESS_KEY=gitimage-secret --dry-run=client -o yaml | kubectl apply -f - >/dev/null
args=(--set image.repository=gitimage --set image.tag=e2e --set image.pullPolicy=Never
      --set controller.replicas=2 --set webhook.replicas=2 --set logFormat=text
      --set logLevel='info\,gitimage=debug\,kube=warn'
      --set hooks.baseURL=http://gitimage-hooks.$NS.svc:8080
      --set 'network.clusterCIDRs={10.244.0.0/16,10.96.0.0/12}'
      --set build.activeDeadline=15m)
case "$ISOLATION" in
  privileged) args+=(--set buildPolicy.isolation.mode=privileged --set buildPolicy.isolation.allowPrivileged=true) ;;
  userns) args+=(--set buildPolicy.isolation.mode=userns) ;;
  *) args+=(--set buildPolicy.isolation.mode=runtimeClass --set buildPolicy.isolation.runtimeClassName="$ISOLATION") ;;
esac
case "$STORAGE" in
  registry) args+=(--set storage.type=registry --set storage.registry.url=kind-registry:5000/builds
                   --set storage.registry.pushSecret=registry-push --set storage.registry.insecure=true) ;;
  s3) args+=(--set storage.type=s3 --set storage.s3.bucket=gitimage --set storage.s3.endpoint=http://e2e-s3:8333
             --set storage.s3.secret=s3-creds --set storage.s3.pathStyle=true) ;;
esac
helm upgrade --install gitimage charts/gitimage -n "$NS" "${args[@]}" --wait --timeout 5m
kubectl -n "$NS" rollout restart deploy/gitimage-controller deploy/gitimage-webhook >/dev/null
kubectl -n "$NS" rollout status deploy/gitimage-controller --timeout 5m
kubectl -n "$NS" rollout status deploy/gitimage-webhook --timeout 3m
[ "$STORAGE" = s3 ] && kubectl -n "$NS" rollout status ds/gitimage-node --timeout 3m
echo "installed: storage=$STORAGE isolation=$ISOLATION"
