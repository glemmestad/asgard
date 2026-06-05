# An Auth0 application (client) in an existing tenant. The auth0/auth0 provider
# reads AUTH0_DOMAIN / AUTH0_CLIENT_ID / AUTH0_CLIENT_SECRET from the inherited
# process env (Asgard loads them from .env). The project `tags` map is stamped
# onto the client's metadata so `project=<id>` attribution lands downstream.

terraform {
  required_providers {
    auth0 = { source = "auth0/auth0" }
  }
}

provider "auth0" {}

locals {
  # Qualify the user-supplied name with the project id so apps from different
  # projects don't collide (every AWS module does the same inline). Auth0 names
  # are display strings with no charset/length limit, so no lowercasing.
  qualified_name = "${lookup(var.tags, "project", "asgard")}-${var.name}"

  audience = var.resource_server_template == "" ? "" : replace(
    var.resource_server_template, "{project}", lookup(var.tags, "project", "")
  )
}

resource "auth0_client" "app" {
  name     = local.qualified_name
  app_type = var.app_type

  # Auth0 client_metadata values must be strings; the tags map already is.
  client_metadata = var.tags
}

# The auth0_client resource no longer exports the secret (provider v1+); the data
# source reads it back (needs Management API read:client_keys at runtime).
data "auth0_client" "app" {
  client_id = auth0_client.app.client_id
}

# A project-dedicated API whose identifier becomes the application's audience.
resource "auth0_resource_server" "api" {
  count      = local.audience == "" ? 0 : 1
  name       = local.qualified_name
  identifier = local.audience
}

# Enable existing tenant connections on this client. The singular association
# resource adds only this client to each connection — it does not own the
# connection's full client list, so it never clobbers other apps' access.
data "auth0_connection" "enabled" {
  for_each = toset(var.enabled_connections)
  name     = each.value
}

resource "auth0_connection_client" "enabled" {
  for_each      = data.auth0_connection.enabled
  connection_id = each.value.id
  client_id     = auth0_client.app.client_id
}
