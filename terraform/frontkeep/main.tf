# Optional Terraform module to run Frontkeep as a single container on a cloud VM or
# any Docker host. Kubernetes is supported but never required; this mirrors the
# headline `docker run` path for infra-as-code shops.

terraform {
  required_version = ">= 1.3"
  required_providers {
    docker = {
      source  = "kreuzwerker/docker"
      version = "~> 3.0"
    }
  }
}

resource "docker_image" "frontkeep" {
  name = "${var.image_repository}:${var.image_tag}"
}

resource "docker_volume" "data" {
  name = "${var.name}-data"
}

resource "docker_container" "frontkeep" {
  name  = var.name
  image = docker_image.frontkeep.image_id

  env = concat(
    [
      "FRONTKEEP_DATABASE_URL=${var.database_url}",
      "FRONTKEEP_BIND=0.0.0.0:8080",
    ],
    var.git_token == "" ? [] : ["FRONTKEEP_GIT_TOKEN=${var.git_token}"],
  )

  ports {
    internal = 8080
    external = var.host_port
  }

  volumes {
    volume_name    = docker_volume.data.name
    container_path = "/data"
  }

  restart = "unless-stopped"
}

# Resource addresses were renamed asgard→frontkeep. These keep an existing state
# file pointing at the same live image/container instead of destroying and
# recreating it on the next apply. (A deployment that set `name` explicitly is
# unaffected; one relying on the old default name should pass `-var name=asgard`
# to avoid renaming the container and its data volume.)
moved {
  from = docker_image.asgard
  to   = docker_image.frontkeep
}

moved {
  from = docker_container.asgard
  to   = docker_container.frontkeep
}
