# Example: Talos homelab

Kata on nodes with `/dev/kvm`, Ceph RGW (Rook) for storage, no registry, ArgoCD.

```yaml
buildPolicy:
  isolation: { mode: runtimeClass, runtimeClassName: kata, privilegedInSandbox: true }
storage:
  type: s3
  s3:
    bucket: gitimage
    endpoint: http://rook-ceph-rgw-ceph-objectstore.rook-ceph.svc
    secret: gitimage        # created by the ObjectBucketClaim below
    pathStyle: true
network:
  clusterCIDRs: [10.244.0.0/16, 10.96.0.0/12]
extraObjects:
  - apiVersion: objectbucket.io/v1alpha1
    kind: ObjectBucketClaim
    metadata: { name: gitimage, namespace: "{{ .Release.Namespace }}" }
    spec: { bucketName: gitimage, storageClassName: ceph-bucket }
```

Before enabling `privilegedInSandbox`, confirm the Kata handler's containerd config has
`privileged_without_host_devices = true`.
