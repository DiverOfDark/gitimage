# Architecture

One Rust binary (`gitimage`) with subcommands `controller`, `webhook`, `node`, `clone`,
`crdgen`, `check-config`, built on kube-rs, axum, gix and russh. No `git` or `ssh` binary, no
OpenSSL.

| Crate | Role |
| --- | --- |
| `gitimage-api` | CRD types, annotation keys, the strict `Config` |
| `gitimage-git` | ref resolution, gix fetch and checkout, russh ssh transport with known_hosts checks, host tree APIs |
| `gitimage-oci` | OCI distribution routes (push endpoint and node server), S3 / registry / memory storage, push tokens |
| `gitimage-controller` | reconciler state machine, Job builder, gate releaser, roller, hooks, GC, isolation check, TLS bootstrap |
| `gitimage-webhook` | pure `mutate(pod) -> patch`, guard, axum handler |
| `gitimage-node` | localhost-only read-only OCI server |
| `gitimage-tls` | CA and certificates, hot-reloading TLS listeners |

The design documents (PRD and implementation design) record the decisions and trade-offs.
