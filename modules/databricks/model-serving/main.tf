# A Databricks Model Serving endpoint serving a registered model. Provider creds
# come from the inherited env. Tags are attached to the endpoint and, per the
# provider, automatically propagated to billing logs. Once created, add this
# endpoint's name to the `databricks` inference module so Frontkeep's gateway fronts it.

terraform {
  required_providers {
    databricks = { source = "databricks/databricks" }
  }
}

provider "databricks" {}

variable "name" {
  type = string
}

# The model to serve (UC model or workspace registered-model name).
variable "entity_name" {
  type = string
}

variable "entity_version" {
  type    = string
  default = "1"
}

# Small | Medium | Large.
variable "workload_size" {
  type    = string
  default = "Small"
}

variable "scale_to_zero_enabled" {
  type    = bool
  default = true
}

variable "tags" {
  type    = map(string)
  default = {}
}

resource "databricks_model_serving" "this" {
  name = var.name

  config {
    served_entities {
      name                  = "primary"
      entity_name           = var.entity_name
      entity_version        = var.entity_version
      workload_size         = var.workload_size
      scale_to_zero_enabled = var.scale_to_zero_enabled
    }
  }

  dynamic "tags" {
    for_each = var.tags
    content {
      key   = tags.key
      value = tags.value
    }
  }
}

output "id" {
  value = databricks_model_serving.this.id
}

output "serving_endpoint_id" {
  value = databricks_model_serving.this.serving_endpoint_id
}
