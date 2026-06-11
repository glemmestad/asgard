output "container_name" {
  value = docker_container.frontkeep.name
}

output "endpoint" {
  value = "http://localhost:${var.host_port}"
}
