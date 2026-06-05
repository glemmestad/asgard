output "client_id" {
  value = auth0_client.app.client_id
}

output "client_secret" {
  value     = data.auth0_client.app.client_secret
  sensitive = true
}

# The resource server identifier, for the app's AUTH0_AUDIENCE. Empty when no
# resource_server_template was supplied.
output "audience" {
  value = local.audience
}
