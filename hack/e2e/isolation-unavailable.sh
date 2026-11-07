#!/usr/bin/env bash
# PRD N1 / M4 "Done when": a missing RuntimeClass makes the controller exit with
# IsolationUnavailable (code 3) and stay in CrashLoopBackOff; nothing falls back.
# Restores the previous install afterwards: ./isolation-unavailable.sh <storage> <isolation>
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH="$PWD/hack/bin:$PATH"
export KUBECONFIG="$PWD/hack/.kubeconfig"
STORAGE=${1:-registry}
RESTORE=${2:-privileged}
NS=gitimage-system
helm upgrade gitimage charts/gitimage -n "$NS" --reuse-values \
  --set buildPolicy.isolation.mode=runtimeClass --set buildPolicy.isolation.runtimeClassName=does-not-exist >/dev/null
ok=""
for i in $(seq 1 90); do
  for p in $(kubectl -n "$NS" get pods -l app.kubernetes.io/component=controller -o name); do
    code=$(kubectl -n "$NS" get "$p" -o jsonpath='{.status.containerStatuses[0].lastState.terminated.exitCode}' 2>/dev/null || true)
    if [ "$code" = "3" ] && kubectl -n "$NS" logs "$p" --previous 2>/dev/null | grep -q "IsolationUnavailable: RuntimeClass does-not-exist not found"; then
      ok=$p; break 2
    fi
  done
  sleep 2
done
./hack/e2e/install.sh "$STORAGE" "$RESTORE" >/dev/null
if [ -z "$ok" ]; then echo "FAIL: no controller exited 3 with IsolationUnavailable"; exit 1; fi
echo "ok: $ok exited 3 with IsolationUnavailable; reinstalled $STORAGE/$RESTORE"
