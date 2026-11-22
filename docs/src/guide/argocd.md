# GitOps with ArgoCD

GitImage writes two things ArgoCD would report as drift:

- the `gitimage.dev/revision.<container>` and `gitimage.dev/digest.<container>` pod-template
  annotations, and with
  `controller.pinTemplateImage` the bound containers' `image` (field manager
  `gitimage-controller`);
- the `caBundle` of the `gitimage` MutatingWebhookConfiguration.

```yaml
spec:
  syncPolicy:
    syncOptions: [ServerSideApply=true, RespectIgnoreDifferences=true]
  ignoreDifferences:
    - { group: apps, kind: Deployment, managedFieldsManagers: [gitimage-controller] }
    - { group: apps, kind: StatefulSet, managedFieldsManagers: [gitimage-controller] }
    - { group: apps, kind: DaemonSet, managedFieldsManagers: [gitimage-controller] }
    # for the Application that installs the chart:
    - group: admissionregistration.k8s.io
      kind: MutatingWebhookConfiguration
      name: gitimage
      jqPathExpressions: ['.webhooks[].clientConfig.caBundle']
```

Sync order does not matter: pods created before their GitImage wait gated.
