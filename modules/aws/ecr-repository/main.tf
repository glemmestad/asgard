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

provider "aws" {
  region = var.region
}

resource "aws_ecr_repository" "this" {
  name = "${lookup(var.tags, "project", "asgard")}-${var.name}"

  image_scanning_configuration {
    scan_on_push = true
  }

  tags = var.tags
}

output "repository" {
  value = aws_ecr_repository.this.name
}

output "uri" {
  value = aws_ecr_repository.this.repository_url
}
