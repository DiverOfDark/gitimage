# Storage modes

| `storage.type` | Images live in | Pods pull from |
| --- | --- | --- |
| `registry` | `<url>/<namespace>/<gitimage>:<sha>` in any OCI registry | the registry |
| `s3` | an S3 bucket: `repos/<namespace>/<gitimage>/{blobs,manifests,tags}` | `localhost:25050` on each node |

Builds never see storage credentials: BuildKit pushes to the controller's push endpoint
(Service `gitimage-push:5000`, TLS with the controller's CA), which writes to the backend.

## registry

`pushSecret` (dockerconfigjson, in the release namespace) is used by the controller only. With a
private registry set `pullSecret`: it is copied into every namespace with a GitImage as
`gitimage-pull` and added to gated pods. GC deletes manifests; blob GC is the registry's own.

## s3

Works with Ceph RGW, SeaweedFS, AWS S3, R2 and other S3 APIs (`pathStyle: true` for most
self-hosted ones). `gitimage-node` is a hostNetwork DaemonSet bound to `127.0.0.1:25050` only;
containerd allows plain HTTP to localhost with no node configuration (verified with containerd
2.3). Change `node.port` if 25050 is taken on your nodes.

Add a lifecycle rule that expires objects under `uploads/` after a day: they are abandoned
multipart uploads of crashed pushes. Blobs over 5 GiB are not supported.

## Garbage collection

Hourly on the leader: keep the 5 newest builds per GitImage, every digest a pod runs, the build
cache and in-flight builds; delete unreferenced manifests, and in s3 mode blobs older than 1 h.
