# Private, versioned, encrypted S3 bucket. The Asgard terraform connector writes
# `name` + the immutable project `tags` map as tfvars; any extra spec keys it
# passes (cloud, account, …) arrive as undeclared vars and are ignored.

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

# Bucket names are globally unique + lowercase; namespace by project and account.
locals {
  bucket = lower("${lookup(var.tags, "project", "asgard")}-${var.name}-${lookup(var.tags, "account", "acct")}")
}

resource "aws_s3_bucket" "this" {
  bucket = local.bucket
  tags   = var.tags
}

resource "aws_s3_bucket_versioning" "this" {
  bucket = aws_s3_bucket.this.id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_public_access_block" "this" {
  bucket                  = aws_s3_bucket.this.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

output "bucket" {
  value = aws_s3_bucket.this.id
}

output "arn" {
  value = aws_s3_bucket.this.arn
}
