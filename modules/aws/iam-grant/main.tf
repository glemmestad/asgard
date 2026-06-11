# Bind a consumer's IAM role to a target resource: attaches an inline policy to
# the role granting `actions` on the target (and its sub-resources). The Frontkeep
# provision layer resolves principal_role_arn, target_arn, and actions from the
# two resources' records + the target manifest's access_levels; it writes them
# plus `name` and the project `tags` as tfvars. Extra spec keys
# (consumer_resource_id, target_resource_id, level, cloud, …) arrive as undeclared
# vars and are ignored.

terraform {
  required_providers {
    aws = { source = "hashicorp/aws" }
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

variable "principal_role_arn" {
  type = string
}

variable "target_arn" {
  type = string
}

variable "actions" {
  type = list(string)
}

provider "aws" {
  region = var.region
}

locals {
  # PutRolePolicy takes the role name, not the ARN.
  role_parts = split("/", var.principal_role_arn)
  role_name  = element(local.role_parts, length(local.role_parts) - 1)
  # IAM policy names allow [A-Za-z0-9+=,.@_-]; the grant name is already in that set.
  policy_name = var.name
}

data "aws_iam_policy_document" "grant" {
  statement {
    effect    = "Allow"
    actions   = var.actions
    resources = [var.target_arn, "${var.target_arn}/*"]
  }
}

resource "aws_iam_role_policy" "grant" {
  name   = local.policy_name
  role   = local.role_name
  policy = data.aws_iam_policy_document.grant.json
}

output "policy_name" {
  value = aws_iam_role_policy.grant.name
}

output "role_name" {
  value = local.role_name
}
