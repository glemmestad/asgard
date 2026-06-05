# A random secret stored in AWS Secrets Manager, so a sibling ecs-service can
# inject it by ARN (`secrets:`) or read it at runtime (`grants.secrets_read`).
# This is the AWS-backed counterpart to the `random-secret` stub, which keeps
# material in Asgard's own store: use that for app-layer secrets, this when the
# value must live in Secrets Manager for a task role to reach it.

terraform {
  required_providers {
    aws    = { source = "hashicorp/aws" }
    random = { source = "hashicorp/random" }
  }
}

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

# Random material length in bytes; hex-encoded into the secret (a 64-byte HMAC
# key becomes 128 hex chars).
variable "byte_length" {
  type    = number
  default = 64
}

provider "aws" {
  region = var.region
}

resource "random_id" "material" {
  byte_length = var.byte_length
}

resource "aws_secretsmanager_secret" "this" {
  name = lower("${lookup(var.tags, "project", "asgard")}-${var.name}")
  tags = var.tags
}

resource "aws_secretsmanager_secret_version" "this" {
  secret_id     = aws_secretsmanager_secret.this.id
  secret_string = random_id.material.hex
}

# Non-secret reference recorded on the resource for sibling services to consume.
output "secret_arn" {
  value = aws_secretsmanager_secret.this.arn
}

output "value" {
  value     = random_id.material.hex
  sensitive = true
}
