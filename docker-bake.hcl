# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Cisco Systems, Inc.

variable "IMAGE" {
  default = "ghcr.io/cisco-open/docker-http-passthrough"
}

variable "TAG" {
  default = "local"
}

variable "PLATFORMS" {
  default = ["linux/amd64", "linux/arm64"]
}

group "default" {
  targets = ["image-local"]
}

target "image" {
  context    = "."
  dockerfile = "Dockerfile"
  tags       = ["${IMAGE}:${TAG}"]
  labels = {
    "org.opencontainers.image.title"       = "Docker HTTP Passthrough"
    "org.opencontainers.image.description" = "HTTP server that proxies container registry images as Docker save-compatible tar archives"
    "org.opencontainers.image.vendor"      = "Cisco Systems, Inc."
    "org.opencontainers.image.licenses"    = "Apache-2.0"
    "org.opencontainers.image.source"      = "https://github.com/cisco-open/docker-http-passthrough"
  }
}

target "image-local" {
  inherits = ["image"]
  output   = ["type=docker"]
}

target "image-all" {
  inherits  = ["image"]
  platforms = PLATFORMS
  output    = ["type=image,push=false"]
}

target "image-push" {
  inherits  = ["image"]
  platforms = PLATFORMS
  output    = ["type=image,push=true"]
}
