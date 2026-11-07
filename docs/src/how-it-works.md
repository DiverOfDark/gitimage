# How it works

1. **Resolve.** The controller lists the remote's refs (git protocol v2 `ls-refs`, nothing
   fetched) every `interval`, or at once when a push hook arrives. A full sha is used as is,
   else a tag, else a branch; only a branch can move.
2. **Decide.** If the commit did not change the build context's git tree (monorepos), the last
   image is reused and nothing is built.
3. **Build.** A Job in the tenant namespace: an init container clones exactly that commit at
   depth 1 (git credentials are mounted only here), then BuildKit builds it under the chosen
   isolation mode. Progress (`step 4/12 RUN npm ci`) is copied into `status` and into Events on
   waiting pods.
4. **Push.** BuildKit pushes the image and its layer cache to the controller's push endpoint
   with a token valid only for `<namespace>/<gitimage>` and only while that Job runs. The
   endpoint writes to the registry or S3.
5. **Admit.** The webhook sees every pod annotated with `gitimage.dev/image.<container>`. If
   the image for the pinned (or current) commit exists, it writes the digest into the
   container right away; otherwise it adds the scheduling gate `gitimage.dev/build`.
6. **Release.** When the build lands, the controller patches the digest into each gated pod
   and removes the gate. It also pins the new commit into every Deployment, StatefulSet and
   DaemonSet that uses the GitImage (`gitimage.dev/revision.<container>`), which rolls them.

```text
 git push ──> controller ──> build Job (tenant ns) ──> push endpoint ──> registry | S3
                  │                                                         │
                  └── status.image ──> webhook / gate releaser ──> pods <───┘ (pull)
```

## Workloads in `gitimage-system`

| Workload | Replicas | Does |
| --- | --- | --- |
| `gitimage-controller` | 2, one leader (Lease) | reconcilers, gate release, rollouts, GC on the leader; push endpoint and hooks on every replica |
| `gitimage-webhook` | 2 | admission (gates, digests, cross-tenant guard); reads a GitImage cache, never the API on the request path |
| `gitimage-node` | DaemonSet, s3 mode only | read-only OCI server on `127.0.0.1:25050` for containerd |
