# Security model

| Asset | Who can read it |
| --- | --- |
| Git credentials | the controller (`get` on the named Secret, in memory) and the clone init container of that GitImage's builds |
| Build secrets | the BuildKit container of that GitImage's builds, as tmpfs for one `RUN` |
| Storage credentials | the controller (and read-only ones in gitimage-node); never build pods |
| Push tokens | one per build Job, HS256, scoped to `<namespace>/<gitimage>`, valid only while that Job runs |

- **Pod Security admission.** Build pods are privileged in Kata (inside the VM) and privileged
  modes, and use unconfined seccomp/AppArmor in rootless modes, so every namespace with a
  GitImage needs `pod-security.kubernetes.io/enforce: privileged`, and so does the release
  namespace (gitimage-node uses hostNetwork). Without it the build fails within seconds with
  `PodCreationFailed` and the admission message.
- **RBAC.** The controller gets `get/create/update/delete` on Secrets, never `list` or `watch`.
- **Admission.** The mutating webhook is called only for pods carrying a
  `gitimage.dev/image.*` annotation (CEL `matchConditions`), so a webhook outage blocks only
  GitImage pods. A second entry denies any pod that references another namespace's GitImage image.
- **Egress.** Build pods get a NetworkPolicy: DNS, the push endpoint, and the internet minus the
  cluster CIDRs.
- **TLS.** The controller runs its own CA (Secret `gitimage-ca`), issues the webhook and push
  certificates, renews them 30 days before expiry and patches the webhook `caBundle`.
- **Supply chain.** Release images are signed with cosign (keyless) and carry an SBOM and
  provenance.
