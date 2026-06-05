# EC2 instance. Networking (subnet / security group) arrives as spec or TF_VAR_*
# vars; left empty the instance lands in the account default VPC. The AMI defaults
# to the latest Amazon Linux 2023 via SSM. Live-apply is deferred (needs operator
# networking); the module is validated, not yet live-proven.

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

variable "instance_type" {
  type    = string
  default = "t3.micro"
}

variable "ami" {
  type    = string
  default = ""
}

variable "subnet_id" {
  type    = string
  default = ""
}

variable "security_group_id" {
  type    = string
  default = ""
}

# Suspend lever: "stopped" halts compute charges (EBS still bills); "running"
# resumes. Asgard's kill/un-kill re-applies with this overridden / at its default.
variable "instance_state" {
  type    = string
  default = "running"
}

provider "aws" {
  region = var.region
}

data "aws_ssm_parameter" "al2023" {
  name = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64"
}

locals {
  ami = var.ami != "" ? var.ami : data.aws_ssm_parameter.al2023.value
}

resource "aws_instance" "this" {
  ami                    = local.ami
  instance_type          = var.instance_type
  subnet_id              = var.subnet_id != "" ? var.subnet_id : null
  vpc_security_group_ids = var.security_group_id != "" ? [var.security_group_id] : null
  tags                   = var.tags
}

resource "aws_ec2_instance_state" "this" {
  instance_id = aws_instance.this.id
  state       = var.instance_state
}

output "instance_id" {
  value = aws_instance.this.id
}
