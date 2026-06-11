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

# Leave blank to fall back to a fleet default (FRONTKEEP_DEFAULT_VPC_ID/SUBNET_IDS via the
# manifest) or, failing that, the account's default VPC and its subnets.
variable "vpc_id" {
  type    = string
  default = ""
}

variable "subnet_ids" {
  type    = list(string)
  default = []
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

# Network fallback (see ecs-service): looked up only when ids weren't supplied.
# `aws_vpcs` returns a list and never errors on "none found".
data "aws_vpcs" "default" {
  count = var.vpc_id == "" ? 1 : 0
  filter {
    name   = "isDefault"
    values = ["true"]
  }
}

data "aws_subnets" "default" {
  count = length(var.subnet_ids) == 0 ? 1 : 0
  filter {
    name   = "vpc-id"
    values = [local.vpc_id]
  }
}

locals {
  prefix     = substr(lower("${lookup(var.tags, "project", "frontkeep")}-${var.name}"), 0, 32)
  enable_tls = var.certificate_arn != ""
  make_sg    = length(var.security_group_ids) == 0
  vpc_id     = var.vpc_id != "" ? var.vpc_id : try(one(data.aws_vpcs.default[0].ids), "")
  subnet_ids = length(var.subnet_ids) > 0 ? var.subnet_ids : try(data.aws_subnets.default[0].ids, [])
}

resource "aws_security_group" "alb" {
  count  = local.make_sg ? 1 : 0
  name   = "${local.prefix}-alb"
  vpc_id = local.vpc_id
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
  subnets            = local.subnet_ids
  security_groups    = local.make_sg ? [aws_security_group.alb[0].id] : var.security_group_ids
  tags               = var.tags

  lifecycle {
    precondition {
      condition     = local.vpc_id != "" && length(local.subnet_ids) > 0
      error_message = "No VPC/subnets resolved for alb: pass vpc_id + subnet_ids, set the fleet defaults (FRONTKEEP_DEFAULT_VPC_ID / FRONTKEEP_DEFAULT_SUBNET_IDS), or ensure the target account has a default VPC."
    }
  }
}

resource "aws_lb_target_group" "this" {
  name        = local.prefix
  port        = var.container_port
  protocol    = "HTTP"
  vpc_id      = local.vpc_id
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
