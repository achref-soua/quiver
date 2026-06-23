{{- define "quiver.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "quiver.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name (include "quiver.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "quiver.labels" -}}
app.kubernetes.io/name: {{ include "quiver.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "quiver.selectorLabels" -}}
app.kubernetes.io/name: {{ include "quiver.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "quiver.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "quiver.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* Name of the Secret holding the master key (chart-managed or external). */}}
{{- define "quiver.masterKeySecret" -}}
{{- if .Values.encryption.existingSecret -}}
{{- .Values.encryption.existingSecret -}}
{{- else -}}
{{- printf "%s-secrets" (include "quiver.fullname" .) -}}
{{- end -}}
{{- end -}}
