# Hot self-host VM images for AWS and Google Cloud, built from the same
# script (../host/setup-host.sh). Build with ../build-host.sh, which prepares
# the boot scripts below.
#
# No SSH: both builders use `communicator = "none"`. The build VM runs the
# setup from its own boot data. On AWS it powers itself off when the bake
# succeeds and Packer waits for that; on Google Cloud Packer waits for the
# startup script to report completion. A failed bake never completes, so the
# build times out instead of publishing an image.

packer {
  required_plugins {
    amazon = {
      version = "= 1.8.3"
      source  = "github.com/hashicorp/amazon"
    }
    googlecompute = {
      version = "= 1.2.8"
      source  = "github.com/hashicorp/googlecompute"
    }
  }
}

variable "hot_release" {
  type        = string
  description = "Hot release whose runtime image supplies hotbox, such as 3.0.1."

  validation {
    condition     = can(regex("^[0-9]+\\.[0-9]+\\.[0-9]+(-[0-9A-Za-z.-]+)?$", var.hot_release))
    error_message = "The hot_release value must be a version such as 3.0.1."
  }
}

variable "aws_user_data_file" {
  type        = string
  default     = ""
  description = "Gzipped AWS boot script from build-host.sh."
}

variable "gcp_startup_script_file" {
  type        = string
  default     = ""
  description = "Compute Engine startup script from build-host.sh."
}

variable "aws_region" {
  type    = string
  default = "us-east-2"
}

variable "aws_subnet_id" {
  type        = string
  default     = ""
  description = "Subnet for the build VM; it needs outbound internet access. Empty uses the default VPC."
}

variable "aws_kms_key_id" {
  type        = string
  default     = ""
  description = "Customer-managed KMS key for the image snapshot. Empty uses the account's default EBS key."
}

variable "gcp_project" {
  type    = string
  default = ""
}

variable "gcp_zone" {
  type    = string
  default = "us-central1-a"
}

variable "gcp_subnetwork" {
  type        = string
  default     = ""
  description = "Subnetwork for the build VM; it needs outbound internet access, such as Cloud NAT."
}

variable "gcp_kms_key" {
  type        = string
  default     = ""
  description = "Cloud KMS key for the image. Empty uses Google-managed encryption."
}

locals {
  timestamp = formatdate("YYYYMMDDhhmmss", timestamp())
  name      = "hot-host-${replace(var.hot_release, ".", "-")}-${local.timestamp}"
  labels = {
    managed-by  = "packer"
    hot-release = replace(var.hot_release, ".", "-")
  }
}

source "amazon-ebs" "hot-host" {
  region        = var.aws_region
  instance_type = "m8i.large"
  subnet_id     = var.aws_subnet_id != "" ? var.aws_subnet_id : null

  source_ami_filter {
    filters = {
      name                = "ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*"
      root-device-type    = "ebs"
      virtualization-type = "hvm"
    }
    owners      = ["099720109477"] # Canonical
    most_recent = true
  }

  communicator          = "none"
  user_data_file        = var.aws_user_data_file
  disable_stop_instance = true

  metadata_options {
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  launch_block_device_mappings {
    device_name           = "/dev/sda1"
    volume_size           = 30
    volume_type           = "gp3"
    encrypted             = true
    kms_key_id            = var.aws_kms_key_id != "" ? var.aws_kms_key_id : null
    delete_on_termination = true
  }
  encrypt_boot = true
  kms_key_id   = var.aws_kms_key_id != "" ? var.aws_kms_key_id : null

  # The bake downloads and verifies Kata and builds the guest image.
  aws_polling {
    delay_seconds = 15
    max_attempts  = 160
  }

  ami_name        = local.name
  ami_description = "Hot self-host host image for release ${var.hot_release}"
  tags            = merge(local.labels, { Name = local.name })
  run_tags        = { Name = "packer-${local.name}" }
}

source "googlecompute" "hot-host" {
  project_id              = var.gcp_project
  zone                    = var.gcp_zone
  subnetwork              = var.gcp_subnetwork != "" ? var.gcp_subnetwork : null
  omit_external_ip        = var.gcp_subnetwork != ""
  source_image_family     = "ubuntu-2404-lts-amd64"
  source_image_project_id = ["ubuntu-os-cloud"]
  machine_type            = "n2-standard-2"
  disk_size               = 30
  disk_type               = "pd-balanced"

  communicator        = "none"
  startup_script_file = var.gcp_startup_script_file
  wrap_startup_script = true
  metadata            = { block-project-ssh-keys = "TRUE" }

  dynamic "image_encryption_key" {
    for_each = var.gcp_kms_key != "" ? [var.gcp_kms_key] : []
    content {
      kmsKeyName = image_encryption_key.value
    }
  }

  image_name        = local.name
  image_family      = "hot-host"
  image_description = "Hot self-host host image for release ${var.hot_release}"
  image_labels      = local.labels
}

build {
  sources = [
    "source.amazon-ebs.hot-host",
    "source.googlecompute.hot-host",
  ]
}
