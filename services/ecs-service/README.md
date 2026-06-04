# ECS Service (load-balanced)

A Fargate service behind an Application Load Balancer — the keystone primitive
for putting a container app on a stable URL. One request produces: an ECS
cluster, a task definition, a task role scoped from your `grants`, an ALB +
target group + listener, CloudWatch logs, and a `url` output you can hit.

## Why this exists

It is the primitive a real app (a collaborative editor, an API, the control plane itself)
needs to run web-facing. It is deliberately built to close the gaps the
predecessor platform hit (see `docs/docs/migrate-app.md`):

- **Grants are honored.** Whatever you put in `grants` becomes the task-role
  inline policy. No silent drop.
- **KMS decrypt is first-class.** A secret read is inert without decrypt on the
  wrapping key, so `grants.kms_decrypt` is a top-level field.
- **HTTPS is one field.** Set `certificate_arn` and you get a 443 listener with
  80→443 redirect — required for any Auth0 SPA, which refuses non-secure origins.
- **There is a `url`.** No reconstructing ALB DNS by hand.

## Networking

The platform never creates a VPC. Supply an existing one:

- `vpc_id` (required)
- `subnet_ids` (required) — where the ALB and tasks land
- `security_group_ids` (optional) — omit and the module creates a task SG that
  only accepts ALB traffic on `container_port`

## Spec fields

| field | required | default | notes |
|-------|----------|---------|-------|
| `name` | yes | | resource name (namespaced by project) |
| `image` | yes | | full image ref, e.g. `…dkr.ecr….amazonaws.com/app:sha-abc123` |
| `vpc_id` | yes | | existing VPC |
| `subnet_ids` | yes | | list of subnet ids |
| `security_group_ids` | no | `[]` | extra task SGs; empty ⇒ module creates one |
| `cpu` / `memory` | no | `512` / `1024` | Fargate sizes (strings) |
| `desired_count` | no | `1` | task count (safe to raise above 1 on Postgres) |
| `min_healthy_percent` | no | `100` | min % of tasks kept serving during a rolling deploy |
| `container_port` | no | `8080` | app listen port |
| `health_path` | no | `/` | ALB health check path |
| `env` | no | `{}` | plain env map |
| `secrets` | no | `{}` | env var → Secrets Manager ARN; execution role auto-granted read |
| `grants` | no | `{}` | `s3_read`, `s3_write`, `secrets_read`, `kms_decrypt` (lists of ARNs) |
| `certificate_arn` | no | `""` | ACM cert ⇒ HTTPS listener |
| `execution_role_arn` | no | `""` | reuse a pre-approved role (e.g. where an SCP forbids `iam:PassRole`) |
| `internal` | no | `true` | internet-facing when false |

## Outputs

`url`, `alb_dns`, `service_arn`, `task_role_arn`, `execution_role_arn`,
`log_group`, `cluster`. No secret outputs.

## Image tagging

Tag images by content (`:sha-<gitsha>`), not `:latest`. The predecessor's ECR
repos were fully immutable and `:latest` re-pushes failed; provision
`ecr-repository` and push immutable tags, then set `image` to the exact tag.
