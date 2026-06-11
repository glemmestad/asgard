# A Unity Catalog schema + managed volume under an existing catalog — a governed
# namespace + storage for a project. Provider creds come from the inherited env.
# The project tag is recorded in the schema properties for attribution.

terraform {
  required_providers {
    databricks = { source = "databricks/databricks" }
  }
}

provider "databricks" {}

variable "name" {
  type = string
}

# An existing Unity Catalog catalog the project may create under.
variable "catalog_name" {
  type = string
}

# Schema to hold the volume; defaults to the project id.
variable "schema_name" {
  type    = string
  default = ""
}

variable "tags" {
  type    = map(string)
  default = {}
}

locals {
  schema = var.schema_name != "" ? var.schema_name : lookup(var.tags, "project", "frontkeep")
}

resource "databricks_schema" "this" {
  catalog_name = var.catalog_name
  name         = local.schema
  comment      = "Frontkeep project ${lookup(var.tags, "project", "")}"
  properties   = var.tags
}

resource "databricks_volume" "this" {
  name         = var.name
  catalog_name = var.catalog_name
  schema_name  = databricks_schema.this.name
  volume_type  = "MANAGED"
}

output "schema_full_name" {
  value = "${var.catalog_name}.${databricks_schema.this.name}"
}

output "volume_path" {
  value = "/Volumes/${var.catalog_name}/${databricks_schema.this.name}/${databricks_volume.this.name}"
}
