# Secrets Manager Secret

Random cryptographic material stored in **AWS Secrets Manager** — the AWS-backed
counterpart to `random-secret` (which keeps material in the platform's own store).

Reach for this when the value must live in Secrets Manager so a task role can
read it: an `ecs-service` injecting it via `secrets:` (env var ← secret ARN), or
reading it at runtime with `grants.secrets_read`. Use `random-secret` instead for
app-layer secrets that never need to be an AWS resource.

## Spec fields

| field | required | default | notes |
|-------|----------|---------|-------|
| `name` | yes | | namespaced by project |
| `byte_length` | no | `64` | random bytes, hex-encoded (64 → 128 hex chars, an HMAC-256 key) |

## Outputs

- `secret_arn` — non-secret ARN, recorded on the resource for sibling services.
- `value` — the secret material; routed to the secret store (`secret_outputs`),
  never recorded in the resource record or audit log.
