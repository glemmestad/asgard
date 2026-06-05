# Load-balanced Fargate service: cluster + task definition + ALB + target group
# + listener, with a task role built from an explicit `grants` object and an
# optional HTTPS listener. This is the keystone primitive for standing a
# web-facing container app behind a stable URL. It closes the predecessor-platform gaps
# documented in the migrate runbook: declared grants are always honored, the
# secret-wrapping KMS key is decryptable, HTTPS is a one-field opt-in, and the
# service emits a real `url`.

terraform {
  required_version = ">= 1.3.0"
  required_providers {
    aws = { source = "hashicorp/aws" }
  }
}

provider "aws" {
  region = var.region
}

data "aws_iam_policy_document" "assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }
  }
}

# The region resources actually land in. `var.region` is null by default so the
# provider resolves it from AWS_REGION/AWS_DEFAULT_REGION; this data source reads
# back that resolved value. The awslogs log driver needs the region as a literal
# (it has no env fallback like the provider does), so feed it from here rather
# than from var.region, which is empty in the common env-driven case.
data "aws_region" "current" {}

locals {
  prefix     = lower("${lookup(var.tags, "project", "asgard")}-${var.name}")
  make_sg    = length(var.security_group_ids) == 0
  task_sg    = local.make_sg ? [aws_security_group.task[0].id] : var.security_group_ids
  exec_role  = var.execution_role_arn != "" ? var.execution_role_arn : aws_iam_role.exec[0].arn
  enable_tls = var.certificate_arn != ""
  region     = coalesce(var.region, data.aws_region.current.region)
  # Secret ARNs the execution role must read to inject `secrets` at task start.
  secret_inject_arns = values(var.secrets)
}

# ---------------------------------------------------------------------------
# Execution role (pull image, write logs, read injected secrets). Created unless
# the operator passes a pre-approved one.
# ---------------------------------------------------------------------------
resource "aws_iam_role" "exec" {
  count              = var.execution_role_arn == "" ? 1 : 0
  name               = "${local.prefix}-exec"
  assume_role_policy = data.aws_iam_policy_document.assume.json
  tags               = var.tags
}

resource "aws_iam_role_policy_attachment" "exec_managed" {
  count      = var.execution_role_arn == "" ? 1 : 0
  role       = aws_iam_role.exec[0].name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

data "aws_iam_policy_document" "exec_secrets" {
  count = var.execution_role_arn == "" && length(local.secret_inject_arns) > 0 ? 1 : 0
  statement {
    actions   = ["secretsmanager:GetSecretValue"]
    resources = local.secret_inject_arns
  }
  dynamic "statement" {
    for_each = length(var.grants.kms_decrypt) > 0 ? [1] : []
    content {
      actions   = ["kms:Decrypt"]
      resources = var.grants.kms_decrypt
    }
  }
}

resource "aws_iam_role_policy" "exec_secrets" {
  count  = var.execution_role_arn == "" && length(local.secret_inject_arns) > 0 ? 1 : 0
  name   = "${local.prefix}-exec-secrets"
  role   = aws_iam_role.exec[0].id
  policy = data.aws_iam_policy_document.exec_secrets[0].json
}

# ---------------------------------------------------------------------------
# Task role — the runtime identity. Built entirely from `grants`.
# ---------------------------------------------------------------------------
resource "aws_iam_role" "task" {
  name               = "${local.prefix}-task"
  assume_role_policy = data.aws_iam_policy_document.assume.json
  tags               = var.tags
}

data "aws_iam_policy_document" "task" {
  dynamic "statement" {
    for_each = length(var.grants.s3_read) > 0 ? [1] : []
    content {
      actions   = ["s3:GetObject", "s3:ListBucket"]
      resources = concat(var.grants.s3_read, [for a in var.grants.s3_read : "${a}/*"])
    }
  }
  dynamic "statement" {
    for_each = length(var.grants.s3_write) > 0 ? [1] : []
    content {
      actions   = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:ListBucket"]
      resources = concat(var.grants.s3_write, [for a in var.grants.s3_write : "${a}/*"])
    }
  }
  dynamic "statement" {
    for_each = length(var.grants.secrets_read) > 0 ? [1] : []
    content {
      actions   = ["secretsmanager:GetSecretValue"]
      resources = var.grants.secrets_read
    }
  }
  dynamic "statement" {
    for_each = length(var.grants.kms_decrypt) > 0 ? [1] : []
    content {
      actions   = ["kms:Decrypt"]
      resources = var.grants.kms_decrypt
    }
  }
}

# Only attach an inline policy when at least one grant exists (an empty policy
# document is invalid).
locals {
  has_grants = length(var.grants.s3_read) + length(var.grants.s3_write) + length(var.grants.secrets_read) + length(var.grants.kms_decrypt) > 0
}

resource "aws_iam_role_policy" "task" {
  count  = local.has_grants ? 1 : 0
  name   = "${local.prefix}-task"
  role   = aws_iam_role.task.id
  policy = data.aws_iam_policy_document.task.json
}

# ---------------------------------------------------------------------------
# Networking: ALB + service security groups (created only when none supplied).
# ---------------------------------------------------------------------------
resource "aws_security_group" "alb" {
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

resource "aws_security_group" "task" {
  count  = local.make_sg ? 1 : 0
  name   = "${local.prefix}-task"
  vpc_id = var.vpc_id
  tags   = var.tags

  ingress {
    from_port       = var.container_port
    to_port         = var.container_port
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_lb" "this" {
  name               = substr("${local.prefix}", 0, 32)
  internal           = var.internal
  load_balancer_type = "application"
  subnets            = var.subnet_ids
  security_groups    = [aws_security_group.alb.id]
  idle_timeout       = var.idle_timeout
  tags               = var.tags
}

resource "aws_lb_target_group" "this" {
  name        = substr("${local.prefix}", 0, 32)
  port        = var.container_port
  protocol    = "HTTP"
  vpc_id      = var.vpc_id
  target_type = "ip"
  tags        = var.tags

  health_check {
    path                = var.health_path
    healthy_threshold   = 2
    unhealthy_threshold = 5
    timeout             = 5
    interval            = 30
    matcher             = "200-399"
  }
}

resource "aws_lb_listener" "http" {
  load_balancer_arn = aws_lb.this.arn
  port              = 80
  protocol          = "HTTP"

  # With a cert, 80 redirects to 443; without, 80 serves the app directly.
  dynamic "default_action" {
    for_each = local.enable_tls ? [1] : []
    content {
      type = "redirect"
      redirect {
        port        = "443"
        protocol    = "HTTPS"
        status_code = "HTTP_301"
      }
    }
  }
  dynamic "default_action" {
    for_each = local.enable_tls ? [] : [1]
    content {
      type             = "forward"
      target_group_arn = aws_lb_target_group.this.arn
    }
  }
}

resource "aws_lb_listener" "https" {
  count             = local.enable_tls ? 1 : 0
  load_balancer_arn = aws_lb.this.arn
  port              = 443
  protocol          = "HTTPS"
  ssl_policy        = "ELBSecurityPolicy-TLS13-1-2-2021-06"
  certificate_arn   = var.certificate_arn

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.this.arn
  }
}

# ---------------------------------------------------------------------------
# Compute: cluster, logs, task definition, service.
# ---------------------------------------------------------------------------
resource "aws_ecs_cluster" "this" {
  name = local.prefix
  tags = var.tags
}

resource "aws_cloudwatch_log_group" "this" {
  name              = "/ecs/${local.prefix}"
  retention_in_days = 14
  tags              = var.tags
}

resource "aws_ecs_task_definition" "this" {
  family                   = local.prefix
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.cpu
  memory                   = var.memory
  execution_role_arn       = local.exec_role
  task_role_arn            = aws_iam_role.task.arn
  tags                     = var.tags

  container_definitions = jsonencode([{
    name      = "app"
    image     = var.image
    essential = true
    portMappings = [{
      containerPort = var.container_port
      protocol      = "tcp"
    }]
    environment = [for k, v in var.env : { name = k, value = v }]
    secrets     = [for k, v in var.secrets : { name = k, valueFrom = v }]
    logConfiguration = {
      logDriver = "awslogs"
      options = {
        "awslogs-group"         = aws_cloudwatch_log_group.this.name
        "awslogs-region"        = local.region
        "awslogs-stream-prefix" = "app"
      }
    }
  }])
}

resource "aws_ecs_service" "this" {
  name            = local.prefix
  cluster         = aws_ecs_cluster.this.id
  task_definition = aws_ecs_task_definition.this.arn
  desired_count   = var.desired_count
  launch_type     = "FARGATE"

  # Keep this fraction of tasks serving through a rolling deploy. With
  # desired_count > 1 (safe on Postgres) capacity stays up while tasks are
  # replaced; the default 100 matches ECS's own default for desired_count = 1.
  deployment_minimum_healthy_percent = var.min_healthy_percent

  network_configuration {
    subnets          = var.subnet_ids
    security_groups  = local.task_sg
    assign_public_ip = var.internal ? false : true
  }

  # With desired_count=1 a bad image would otherwise sit failing health checks
  # forever; the circuit breaker fails the deployment and rolls back to the last
  # healthy task definition.
  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.this.arn
    container_name   = "app"
    container_port   = var.container_port
  }

  depends_on = [aws_lb_listener.http]
  tags       = var.tags
}
