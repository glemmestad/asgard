# Standalone Application Load Balancer for shared / advanced composition, where
# several services register target groups against one ALB rather than each
# `ecs-service` owning its own. Same existing-VPC inputs as `ecs-service`.

terraform {
  required_version = ">= 1.3.0"
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

variable "vpc_id" {
  type = string
}

variable "subnet_ids" {
  type = list(string)
}

variable "security_group_ids" {
  type    = list(string)
  default = []
}

variable "internal" {
  type    = bool
  default = true
}

variable "certificate_arn" {
  type    = string
  default = ""
}

variable "container_port" {
  type    = number
  default = 8080
}

provider "aws" {
  region = var.region
}

locals {
  prefix     = substr(lower("${lookup(var.tags, "project", "asgard")}-${var.name}"), 0, 32)
  enable_tls = var.certificate_arn != ""
  make_sg    = length(var.security_group_ids) == 0
}

resource "aws_security_group" "alb" {
  count  = local.make_sg ? 1 : 0
  name   = "${local.prefix}-alb"
  vpc_id = var.vpc_id
  tags   = var.tags

  ingress {
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }
  dynamic "ingress" {
    for_each = local.enable_tls ? [1] : []
    content {
      from_port   = 443
      to_port     = 443
      protocol    = "tcp"
      cidr_blocks = ["0.0.0.0/0"]
    }
  }
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_lb" "this" {
  name               = local.prefix
  internal           = var.internal
  load_balancer_type = "application"
  subnets            = var.subnet_ids
  security_groups    = local.make_sg ? [aws_security_group.alb[0].id] : var.security_group_ids
  tags               = var.tags
}

resource "aws_lb_target_group" "this" {
  name        = local.prefix
  port        = var.container_port
  protocol    = "HTTP"
  vpc_id      = var.vpc_id
  target_type = "ip"
  tags        = var.tags
}

resource "aws_lb_listener" "main" {
  load_balancer_arn = aws_lb.this.arn
  port              = local.enable_tls ? 443 : 80
  protocol          = local.enable_tls ? "HTTPS" : "HTTP"
  ssl_policy        = local.enable_tls ? "ELBSecurityPolicy-TLS13-1-2-2021-06" : null
  certificate_arn   = local.enable_tls ? var.certificate_arn : null

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.this.arn
  }
}

output "alb_arn" {
  value = aws_lb.this.arn
}

output "dns_name" {
  value = aws_lb.this.dns_name
}

output "listener_arn" {
  value = aws_lb_listener.main.arn
}

output "target_group_arn" {
  value = aws_lb_target_group.this.arn
}
