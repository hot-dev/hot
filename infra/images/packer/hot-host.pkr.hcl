# Hot self-host VM images for AWS and Google Cloud, built from the same
# script (../host/setup-host.sh). Build with ../build-host.sh, which prepares
# the boot scripts below.
#
# No SSH: both builders use `communicator = "none"`. The build VM runs the
# setup from its own boot data. On AWS it powers itself off when the bake
# succeeds and Packer waits for that; a failed bake never powers off, so Packer
# gives up when aws_polling runs out. On Google Cloud Packer's startup-script
# wrapper reports the script's exit status, and a failed bake ends the build.
# Neither publishes an image. Packer waits for that status without a limit, so
# build-host.sh bounds the whole build in time.

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
  description = "Hot release whose runtime image supplies the deploy agent, its renderer, and the fallback hotbox, such as 3.0.1."

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
  description = "Customer-managed KMS key for the image snapshot. Empty uses the account's default EBS key. Instances launch from the image only if the key policy lets the identity that applies the stack use the key, and for the scalable preset also the AWSServiceRoleForAutoScaling service-linked role; otherwise every launch fails."
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

variable "gcp_service_account_email" {
  type        = string
  default     = ""
  description = "Service account for the build VM. Packer's startup-script wrapper reports the bake's status with it, so it needs compute.instances.get and compute.instances.setMetadata on the build VM, and iam.serviceAccounts.actAs on itself (such as roles/iam.serviceAccountUser on itself), which Compute Engine requires to set metadata on a VM that runs as a service account. Without them the status never arrives and the build waits for its time limit. Packer's own identity also needs iam.serviceAccounts.actAs on it. Empty uses the project's default compute service account, which needs the same permissions."
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
  use_internal_ip         = var.gcp_subnetwork != "" # required with omit_external_ip; unused without a communicator
  source_image_family     = "ubuntu-2404-lts-amd64"
  source_image_project_id = ["ubuntu-os-cloud"]
  machine_type            = "n2-standard-2"
  disk_size               = 30
  disk_type               = "pd-balanced"

  # The wrapper only calls Compute Engine, to set its status. Packer's default
  # scopes also grant full Cloud Storage access.
  service_account_email = var.gcp_service_account_email != "" ? var.gcp_service_account_email : null
  scopes = [
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/compute",
  ]

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
