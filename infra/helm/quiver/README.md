# Quiver Helm chart

Deploys the Quiver server (REST + gRPC) on Kubernetes.

```bash
# Generate a 256-bit master key once and keep it safe (rotating it is a re-encrypt).
KEY=$(openssl rand -hex 32)

helm install quiver ./infra/helm/quiver \
  --set image.repository=ghcr.io/achref-soua/quiver \
  --set encryption.masterKey="$KEY" \
  --set apiKeys="$(openssl rand -hex 24)"
```

Quiver encrypts at rest by default, so the install **fails fast** unless you set
one of:

| Value | Meaning |
|-------|---------|
| `encryption.masterKey` | 64 hex chars; the chart stores it in a Secret. |
| `encryption.existingSecret` (+ `existingSecretKey`) | reference a Secret you manage. |
| `encryption.insecure=true` | **DEV ONLY** — disables at-rest encryption. |

No container image is published by the project yet — build and push your own from
`infra/docker/Dockerfile`, or point `image.repository`/`image.tag` at your registry.

Key values (see [`values.yaml`](values.yaml) for all of them):

| Value | Default | Notes |
|-------|---------|-------|
| `replicaCount` | `1` | Quiver is single-node; replicas share no state. |
| `service.restPort` / `service.grpcPort` | `6333` / `6334` | |
| `persistence.enabled` / `persistence.size` | `true` / `10Gi` | PVC mounted at `/data`. |
| `ingress.enabled` | `false` | REST-only ingress. |
| `extraEnv` | `{}` | e.g. `QUIVER_MAX_K`, `QUIVER_RATE_LIMIT_REQUESTS_PER_SECOND`. |

The pod runs as the distroless non-root user (uid 65532) with a read-only root
filesystem; only `/data` and `/tmp` are writable.

A Helm-free path (raw manifests) lives in [`../k8s`](../k8s).
