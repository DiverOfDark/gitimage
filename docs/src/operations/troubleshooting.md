# Troubleshooting

| Symptom | Look at |
| --- | --- |
| Controller in CrashLoopBackOff | `kubectl -n gitimage-system logs deploy/gitimage-controller --previous`: `IsolationUnavailable` names the failing mode; `kubectl -n gitimage-system logs job/gitimage-isolation-check-<hash>` |
| Pod stays `SchedulingGated` | `kubectl get gi <name>`: no image yet; `kubectl describe pod`: `GitImageNotFound` or build progress Events |
| `Failed` / `ResolveFailed` | wrong URL, ref, credentials or host key (`host key ... not in known_hosts`) |
| `Failed` / `PodCreationFailed` | the namespace's Pod Security level: label it `pod-security.kubernetes.io/enforce=privileged` (`baseline` with `rootfulInSandbox`) |
| `Failed` / `BuildFailed` | `kubectl logs job/<name>-<sha7> -c buildkit` |
| `Failed` / `PushFailed` | controller logs (storage errors), then the build log |
| Annotated pod is created without a gate or digest | `kubectl get mutatingwebhookconfiguration gitimage -o yaml`: `caBundle` set? webhook pods ready? |
| Pull fails in s3 mode | `kubectl -n gitimage-system get ds gitimage-node`: NotReady means S3 is unreachable or `port 25050 in use on 127.0.0.1` |

Logs are JSON by default; `--set logFormat=text` for humans, `logLevel=info,gitimage=debug`
for more.
