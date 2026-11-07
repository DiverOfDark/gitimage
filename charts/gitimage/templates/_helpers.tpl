{{- define "gitimage.image" -}}
{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}
{{- end -}}

{{- define "gitimage.labels" -}}
app.kubernetes.io/name: gitimage
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end -}}

{{- define "gitimage.selector" -}}
app.kubernetes.io/name: gitimage
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "gitimage.pushHost" -}}
gitimage-push.{{ .Release.Namespace }}.svc
{{- end -}}

{{- define "gitimage.pullHost" -}}
{{- if eq .Values.storage.type "s3" -}}
localhost:{{ .Values.node.port }}
{{- else -}}
{{ trimSuffix "/" .Values.storage.registry.url }}
{{- end -}}
{{- end -}}

{{- define "gitimage.logArgs" -}}
- --log-format={{ .Values.logFormat }}
- --log-level={{ .Values.logLevel }}
{{- end -}}

{{- define "gitimage.podEnv" -}}
- name: POD_NAMESPACE
  valueFrom: { fieldRef: { fieldPath: metadata.namespace } }
- name: POD_NAME
  valueFrom: { fieldRef: { fieldPath: metadata.name } }
- name: POD_IP
  valueFrom: { fieldRef: { fieldPath: status.podIP } }
{{- end -}}

{{- define "gitimage.securityContext" -}}
allowPrivilegeEscalation: false
readOnlyRootFilesystem: true
runAsNonRoot: true
runAsUser: 65532
runAsGroup: 65532
capabilities: { drop: [ALL] }
seccompProfile: { type: RuntimeDefault }
{{- end -}}
