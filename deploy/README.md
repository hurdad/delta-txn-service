# deploy/

Deployment artifacts for `delta-txn-service`, in two independent forms —
pick whichever matches what you're doing:

## `docker-compose.yaml` — local development

Runs the service alongside a local MinIO instance (S3-compatible storage),
built from this repo's own `Dockerfile`. This is what to reach for when
iterating locally against a real object-store backend rather than the
`file://` filesystem backend the integration test suite (`tests/`) uses.

```bash
docker compose -f deploy/docker-compose.yaml up -d --build
```

- `delta-txn-service` listens on `localhost:50051`.
- MinIO's S3 API is on `localhost:9000`; its web console is on
  `localhost:9001` (`minioadmin` / `minioadmin`, matching the credentials
  baked into the compose file's own `AWS_*` env vars).
- MinIO starts with **no buckets** — create one before pointing a
  `table_uri` at it, e.g.:
  ```bash
  aws --endpoint-url http://localhost:9000 s3 mb s3://your-bucket
  ```

Not meant for production use as-is — credentials are hardcoded, there's no
TLS, and MinIO's single-node mode has no durability guarantees.

## `helm/delta-txn-service/` — Kubernetes

The production deployment path: a `Deployment` + `Service`, with
`values.yaml` driving gRPC bind address, TLS, API-key auth (plain value or
`Secret` reference — see `values.yaml`'s own warning about which one is
safe for anything beyond local testing), storage credentials, and the
`table_uri` allowlist. Pod/container security defaults to non-root,
read-only root filesystem, and all Linux capabilities dropped.
Readiness/liveness probes use Kubernetes' native `grpc` probe type against
the service's own `grpc.health.v1.Health` endpoint, not a bare
TCP-connect check.

```bash
helm install delta-txn-service deploy/helm/delta-txn-service \
  --set image.repository=ghcr.io/hurdad/delta-txn-service \
  --set image.tag=<tag>
```

See `values.yaml` for the full set of configurable values, each with an
inline comment on what it maps to (mostly a 1:1 mapping onto the
environment variables documented in the repo root `README.md`'s
"Configuration" section).
