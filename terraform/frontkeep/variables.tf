variable "name" {
  type    = string
  default = "frontkeep"
}

variable "image_repository" {
  type    = string
  default = "ghcr.io/glemmestad/frontkeep"
}

variable "image_tag" {
  type    = string
  default = "latest"
}

variable "database_url" {
  type        = string
  description = "sqlite:///data/frontkeep.db by default; set a postgres:// DSN to scale out."
  default     = "sqlite:///data/frontkeep.db"
}

variable "git_token" {
  type      = string
  default   = ""
  sensitive = true
}

variable "host_port" {
  type    = number
  default = 8080
}
