# Application Load Balancer (standalone)

A standalone ALB for shared or advanced composition — when several services
should register target groups against one load balancer instead of each
`ecs-service` owning its own. Most apps want `ecs-service` directly (it bundles
its own ALB); reach for this only when you need a shared entry point.

## Networking

Takes an existing VPC (the platform never creates one):

- `vpc_id` (required)
- `subnet_ids` (required)
- `security_group_ids` (optional) — empty ⇒ the module creates one

## Spec fields

| field | required | default | notes |
|-------|----------|---------|-------|
| `name` | yes | | namespaced by project |
| `vpc_id` | yes | | existing VPC |
| `subnet_ids` | yes | | list of subnet ids |
| `security_group_ids` | no | `[]` | empty ⇒ module creates one |
| `internal` | no | `true` | internet-facing when false |
| `certificate_arn` | no | `""` | ACM cert ⇒ HTTPS listener on 443 |
| `container_port` | no | `8080` | target group port |

## Outputs

`alb_arn`, `dns_name`, `listener_arn`, `target_group_arn`. No secret outputs.
