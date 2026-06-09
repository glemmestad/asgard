# A short-lived ECR registry login, brokered by the control plane's own AWS
# identity. Creates nothing — the `aws_ecr_authorization_token` data source mints a
# ~12h token the caller uses for `docker login`/`push`, so the requesting agent never
# holds AWS credentials. The token is account-scoped (whatever this provider's role
# can push to); scope the role to project repos to bound it.

terraform {
  required_providers {
    aws = { source = "hashicorp/aws" }
  }
}

variable "name" {
  type = string
}

variable "region" {
  type = string
  # null => the AWS provider reads AWS_REGION/AWS_DEFAULT_REGION (operator-set, AWS-wide).
  default = null
}

variable "tags" {
  type    = map(string)
  default = {}
}

provider "aws" {
  region = var.region
}

# The provider base64-decodes the token into user_name/password, so the caller does
# not decode — it logs in with username "AWS" and `password` directly.
data "aws_ecr_authorization_token" "this" {}

output "registry" {
  value = data.aws_ecr_authorization_token.this.proxy_endpoint
}

output "username" {
  value = data.aws_ecr_authorization_token.this.user_name
}

output "password" {
  value     = data.aws_ecr_authorization_token.this.password
  sensitive = true
}

output "expires_at" {
  value = data.aws_ecr_authorization_token.this.expires_at
}
