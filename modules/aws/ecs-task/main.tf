# Fargate ECS cluster + task definition from a container image. Running the task
# (network config, exec role) is operator-supplied; live-apply is deferred, the
# module is validated, not yet live-proven.

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

variable "image" {
  type = string
}

variable "cpu" {
  type    = string
  default = "256"
}

variable "memory" {
  type    = string
  default = "512"
}

variable "execution_role_arn" {
  type    = string
  default = ""
}

provider "aws" {
  region = var.region
}

locals {
  family = "${lookup(var.tags, "project", "frontkeep")}-${var.name}"
}

resource "aws_ecs_cluster" "this" {
  name = local.family
  tags = var.tags
}

resource "aws_ecs_task_definition" "this" {
  family                   = local.family
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.cpu
  memory                   = var.memory
  execution_role_arn       = var.execution_role_arn != "" ? var.execution_role_arn : null

  container_definitions = jsonencode([{
    name      = "app"
    image     = var.image
    essential = true
  }])

  tags = var.tags
}

output "cluster" {
  value = aws_ecs_cluster.this.name
}

output "task_definition_arn" {
  value = aws_ecs_task_definition.this.arn
}
