variable "name" {
  type = string
}

variable "region" {
  type    = string
  # null => the AWS provider reads AWS_REGION/AWS_DEFAULT_REGION (operator-set, AWS-wide).
  default = null
}

variable "tags" {
  type    = map(string)
  default = {}
}

# Existing network the service lands in. Frontkeep never creates a VPC. Leave blank to
# fall back to a fleet default (FRONTKEEP_DEFAULT_VPC_ID/SUBNET_IDS via the manifest) or,
# failing that, the account's default VPC and its subnets — so an agent with no console
# access can still deploy. See the vpc_id/subnet_ids locals in main.tf.
variable "vpc_id" {
  type    = string
  default = ""
}

variable "subnet_ids" {
  type    = list(string)
  default = []
}

# Extra security groups attached to the service tasks. Empty ⇒ the module creates
# one that only accepts traffic from the ALB on container_port.
variable "security_group_ids" {
  type    = list(string)
  default = []
}

variable "image" {
  type = string
}

variable "cpu" {
  type    = string
  default = "512"
}

variable "memory" {
  type    = string
  default = "1024"
}

variable "desired_count" {
  type    = number
  default = 1
}

variable "min_healthy_percent" {
  type    = number
  default = 100
}

variable "container_port" {
  type    = number
  default = 8080
}

# ALB target-group health check path. Defaults to "/" so a generic web app works
# out of the box, but "/" usually only proves the app serves *something* — set this
# to a real readiness endpoint that fails when the backend is unhealthy (Frontkeep:
# "/readyz", which checks the DB) so the ALB pulls broken tasks instead of routing
# to them.
variable "health_path" {
  type    = string
  default = "/"
}

# Plain (non-secret) environment, injected as container `environment`.
variable "env" {
  type    = map(string)
  default = {}
}

# Secret environment: env var name -> Secrets Manager ARN (optionally suffixed
# `:json-key::` to pull one field). Injected as container `secrets`; the
# execution role is granted GetSecretValue on these automatically — the gap
# the predecessor platform hit, where a declared secret ref produced no working grant.
variable "secrets" {
  type    = map(string)
  default = {}
}

# Runtime task-role grants. Each list is built into the task-role inline policy,
# so a declared grant is always an effective grant (the predecessor silently dropped
# these). kms_decrypt closes the companion gap: a secret grant is inert without
# decrypt on the wrapping key.
variable "grants" {
  type = object({
    s3_read      = optional(list(string), [])
    s3_write     = optional(list(string), [])
    secrets_read = optional(list(string), [])
    kms_decrypt  = optional(list(string), [])
  })
  default = {}
}

# When set, an HTTPS listener is added on 443 with this ACM cert and HTTP 80
# redirects to it. Auth0 SPAs (auth0-spa-js) refuse any non-localhost HTTP
# origin, so HTTPS is required for a working login — the predecessor shipped HTTP-only.
variable "certificate_arn" {
  type    = string
  default = ""
}

# Optionally reuse a pre-approved execution role instead of creating one. Useful
# where an SCP forbids iam:PassRole on freshly created roles.
variable "execution_role_arn" {
  type    = string
  default = ""
}

# Whether the ALB is internet-facing. Internal by default.
variable "internal" {
  type    = bool
  default = true
}

# ALB idle timeout (seconds). AWS defaults to 60, which severs long-lived
# Streamable-HTTP / SSE connections mid-stream — an agent on Frontkeep's `/mcp` would
# see a tool call cut off. Default to 300 so streaming works out of the box; raise
# toward 900 for very long tool calls.
variable "idle_timeout" {
  type    = number
  default = 300
}
