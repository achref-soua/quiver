# Kubernetes & Helm

Quiver ships a Helm chart (`infra/helm/quiver`) and raw manifests
(`infra/k8s/quiver.yaml`) for self-hosting on a cluster.

## Helm

```bash
# A 256-bit master key, generated once and kept safe (rotating it re-encrypts).
KEY=$(openssl rand -hex 32)

helm install quiver ./infra/helm/quiver \
  --set image.repository=ghcr.io/achref-soua/quiver \
  --set image.tag=0.20.1 \
  --set encryption.masterKey="$KEY" \
  --set apiKeys="$(openssl rand -hex 24)"
```

Quiver encrypts at rest by default, so the install **fails fast** with a clear
message unless you provide a key strategy:

| Value | Meaning |
|-------|---------|
| `encryption.masterKey` | 64 hex chars; the chart stores it in a Secret. |
| `encryption.existingSecret` (+ `existingSecretKey`) | reference a Secret you manage. |
| `encryption.insecure=true` | **dev only** — disables at-rest encryption. |

The chart deploys a single-node server (REST `6333`, gRPC `6334`) with a PVC at
`/data`, runs as the distroless non-root user (uid 65532) with a read-only root
filesystem, and exposes a `ClusterIP` Service. Set `ingress.enabled=true` for
external REST access. See [`infra/helm/quiver/values.yaml`](https://github.com/achref-soua/quiver/blob/main/infra/helm/quiver/values.yaml)
for every value.

> **Image.** No container image is published by the project yet — build and push
> one from `infra/docker/Dockerfile` (or use the release binaries) and point
> `image.repository`/`image.tag` at your registry.

## Raw manifests

No Helm? Edit the Secret and image in
[`infra/k8s/quiver.yaml`](https://github.com/achref-soua/quiver/blob/main/infra/k8s/quiver.yaml)
and apply it:

```bash
kubectl apply -f infra/k8s/quiver.yaml
```

## Reaching the server

```bash
kubectl port-forward svc/quiver 6333:6333
curl http://127.0.0.1:6333/metrics    # the metrics endpoint is open (scrape it from Prometheus)
```
