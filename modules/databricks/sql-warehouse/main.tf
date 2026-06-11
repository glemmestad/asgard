# A Databricks SQL warehouse. The databricks provider reads DATABRICKS_HOST /
# DATABRICKS_TOKEN from the inherited process env (Frontkeep loads them from .env).
# The Frontkeep terraform connector writes `name` + every spec field + the immutable
# project `tags` map as tfvars; the project tag is stamped as a warehouse
# custom_tag so system.billing.usage attributes spend per project.

terraform {
  required_providers {
    databricks = { source = "databricks/databricks" }
  }
}

provider "databricks" {}

variable "name" {
  type = string
}

variable "cluster_size" {
  type    = string
  default = "Small"
}

variable "auto_stop_mins" {
  type    = number
  default = 10
}

variable "max_num_clusters" {
  type    = number
  default = 1
}

variable "enable_serverless_compute" {
  type    = bool
  default = true
}

# PRO or CLASSIC.
variable "warehouse_type" {
  type    = string
  default = "PRO"
}

variable "tags" {
  type    = map(string)
  default = {}
}

resource "databricks_sql_endpoint" "this" {
  name                      = var.name
  cluster_size              = var.cluster_size
  auto_stop_mins            = var.auto_stop_mins
  max_num_clusters          = var.max_num_clusters
  enable_serverless_compute = var.enable_serverless_compute
  warehouse_type            = var.warehouse_type

  tags {
    dynamic "custom_tags" {
      for_each = var.tags
      content {
        key   = custom_tags.key
        value = custom_tags.value
      }
    }
  }
}

output "id" {
  value = databricks_sql_endpoint.this.id
}

output "jdbc_url" {
  value = databricks_sql_endpoint.this.jdbc_url
}

output "http_path" {
  value = databricks_sql_endpoint.this.odbc_params[0].path
}
