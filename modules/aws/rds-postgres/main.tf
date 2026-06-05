# Managed PostgreSQL (RDS). The master password is generated and surfaced as a
# sensitive output, which the Asgard connector routes to the secret store —
# matching the manifest's `secret_outputs: [master_password, connection_url]`.
# Live-apply is deferred (needs operator subnet group / security groups); the
# module is validated, not yet live-proven.

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

variable "instance_class" {
  type    = string
  default = "db.t3.micro"
}

variable "allocated_storage" {
  type    = number
  default = 20
}

variable "engine_version" {
  type    = string
  default = "16"
}

variable "db_name" {
  type    = string
  default = "appdb"
}

variable "username" {
  type    = string
  default = "asgard"
}

variable "subnet_group_name" {
  type    = string
  default = ""
}

variable "vpc_security_group_ids" {
  type    = list(string)
  default = []
}

provider "aws" {
  region = var.region
}

resource "random_password" "master" {
  length  = 24
  special = false
}

resource "aws_db_instance" "this" {
  identifier             = lower("${lookup(var.tags, "project", "asgard")}-${var.name}")
  engine                 = "postgres"
  engine_version         = var.engine_version
  instance_class         = var.instance_class
  allocated_storage      = var.allocated_storage
  db_name                = var.db_name
  username               = var.username
  password               = random_password.master.result
  db_subnet_group_name   = var.subnet_group_name != "" ? var.subnet_group_name : null
  vpc_security_group_ids = length(var.vpc_security_group_ids) > 0 ? var.vpc_security_group_ids : null
  skip_final_snapshot    = true
  tags                   = var.tags
}

locals {
  connection_url = "postgres://${var.username}:${random_password.master.result}@${aws_db_instance.this.endpoint}/${var.db_name}"
}

# A real Secrets Manager secret holding the connection details, so a consumer
# (e.g. ecs-service `secrets:`) can inject it by ARN at task start. The
# predecessor surfaced this under an inconsistent key name and consumers' ref
# lookups missed it; here the ARN is just `secret_arn`, full stop.
resource "aws_secretsmanager_secret" "connection" {
  name = lower("${lookup(var.tags, "project", "asgard")}-${var.name}-connection")
  tags = var.tags
}

resource "aws_secretsmanager_secret_version" "connection" {
  secret_id = aws_secretsmanager_secret.connection.id
  secret_string = jsonencode({
    host     = aws_db_instance.this.address
    port     = aws_db_instance.this.port
    dbname   = var.db_name
    username = var.username
    password = random_password.master.result
    url      = local.connection_url
  })
}

output "endpoint" {
  value = aws_db_instance.this.address
}

# Non-secret reference to the Secrets Manager secret — recorded on the resource
# so a sibling ecs-service can wire it into `secrets:` / `grants.secrets_read`.
output "secret_arn" {
  value = aws_secretsmanager_secret.connection.arn
}

output "master_password" {
  value     = random_password.master.result
  sensitive = true
}

output "connection_url" {
  value     = local.connection_url
  sensitive = true
}
