# Container image repository (scan-on-push).

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

# IMMUTABLE blocks overwriting a tag once pushed — recommended for `:sha` deploys so
# the ECS image cycle is deterministic. Default MUTABLE keeps existing repos and
# `:latest` workflows unchanged.
variable "immutable" {
  type    = bool
  default = false
}

# Lifecycle hygiene: expire untagged images after `untagged_expire_days`, and cap
# tagged images at `keep_last`. Bounds storage cost without touching in-use tags.
variable "untagged_expire_days" {
  type    = number
  default = 14
}

variable "keep_last" {
  type    = number
  default = 10
}

provider "aws" {
  region = var.region
}

resource "aws_ecr_repository" "this" {
  name                 = "${lookup(var.tags, "project", "frontkeep")}-${var.name}"
  image_tag_mutability = var.immutable ? "IMMUTABLE" : "MUTABLE"

  image_scanning_configuration {
    scan_on_push = true
  }

  tags = var.tags
}

resource "aws_ecr_lifecycle_policy" "this" {
  repository = aws_ecr_repository.this.name

  policy = jsonencode({
    rules = [
      {
        rulePriority = 1
        description  = "Expire untagged images"
        selection = {
          tagStatus   = "untagged"
          countType   = "sinceImagePushed"
          countUnit   = "days"
          countNumber = var.untagged_expire_days
        }
        action = { type = "expire" }
      },
      {
        rulePriority = 2
        description  = "Keep only the most recent tagged images"
        selection = {
          tagStatus   = "any"
          countType   = "imageCountMoreThan"
          countNumber = var.keep_last
        }
        action = { type = "expire" }
      }
    ]
  })
}

output "repository" {
  value = aws_ecr_repository.this.name
}

output "uri" {
  value = aws_ecr_repository.this.repository_url
}
