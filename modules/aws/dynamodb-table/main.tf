# On-demand DynamoDB table with PITR and encryption at rest.

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

variable "pk_name" {
  type = string
}

variable "pk_type" {
  type    = string
  default = "S"
}

provider "aws" {
  region = var.region
}

resource "aws_dynamodb_table" "this" {
  name         = "${lookup(var.tags, "project", "asgard")}-${var.name}"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = var.pk_name

  attribute {
    name = var.pk_name
    type = var.pk_type
  }

  point_in_time_recovery {
    enabled = true
  }

  server_side_encryption {
    enabled = true
  }

  tags = var.tags
}

output "table" {
  value = aws_dynamodb_table.this.name
}

output "arn" {
  value = aws_dynamodb_table.this.arn
}
