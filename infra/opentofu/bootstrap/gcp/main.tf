# State for Hot installations on Google Cloud, and optionally the GitHub
# identities that plan, apply, and release them. Apply this root once,
# separately from any installation, so destroying an installation can never
# remove the only copy of its state.
terraform {
  required_version = ">= 1.13.1, < 1.14.0"
  required_providers {
    google = { source = "hashicorp/google", version = "8.7.0" }
  }
}
provider "google" {
  project = var.project
}

variable "project" {
  type = string
}
variable "location" {
  description = "Bucket location, such as US or us-central1."
  type        = string
  default     = "US"
}
variable "state_bucket" {
  type = string
  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9._-]{1,61}[a-z0-9]$", var.state_bucket))
    error_message = "state_bucket must be a valid bucket name."
  }
}
variable "kms_key" {
  description = "Cloud KMS key for state objects. Empty uses Google-managed encryption."
  type        = string
  default     = ""
}
variable "github_repository" {
  description = "OWNER/REPO allowed to plan, apply, and release. Empty skips the GitHub identities."
  type        = string
  default     = ""
  validation {
    condition     = var.github_repository == "" || can(regex("^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$", var.github_repository))
    error_message = "github_repository must be OWNER/REPO."
  }
}
variable "github_environment" {
  type    = string
  default = "production"
}
variable "apply_roles" {
  description = "Project roles for the apply service account. Choose them for your guardrails; there is no default."
  type        = list(string)
  default     = []
}
variable "name_prefix" {
  type    = string
  default = "hot"
}
variable "operators" {
  description = "People, groups, or service accounts who may impersonate a deploy identity to run OpenTofu or the operator helper themselves, by identity: plan, apply, release. For example { apply = [\"user:ada@example.com\"] }."
  type = object({
    plan    = optional(list(string), [])
    apply   = optional(list(string), [])
    release = optional(list(string), [])
  })
  default = {}
  validation {
    condition = alltrue([for member in flatten([var.operators.plan, var.operators.apply, var.operators.release]) :
    can(regex("^(user|group|serviceAccount|domain|principal|principalSet):.+$", member))])
    error_message = "operators entries must be IAM members such as user:ada@example.com or group:ops@example.com."
  }
}
variable "names" {
  description = "Service account IDs for the deploy identities, such as { apply = \"infra-provisioner\" }. The default is NAME_PREFIX-github-IDENTITY."
  type        = map(string)
  default     = {}
  validation {
    condition = alltrue([for identity, id in var.names :
    contains(["plan", "apply", "release"], identity) && can(regex("^[a-z][a-z0-9-]{4,28}[a-z0-9]$", id))])
    error_message = "names maps plan, apply, or release to a service account ID of 6-30 lowercase letters, digits, and hyphens."
  }
}

locals {
  github = var.github_repository != ""
  # A deploy identity exists for GitHub, for the operators who impersonate
  # it, or both.
  identities = toset([for identity in ["plan", "apply", "release"] : identity if local.github || length(var.operators[identity]) > 0])
}

resource "google_storage_bucket" "state" {
  name                        = var.state_bucket
  location                    = var.location
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  versioning {
    enabled = true
  }
  # Old state versions are the recovery path after a bad apply; keep them.
  lifecycle_rule {
    condition {
      num_newer_versions = 20
      with_state         = "ARCHIVED"
    }
    action {
      type = "Delete"
    }
  }
  dynamic "encryption" {
    for_each = var.kms_key == "" ? [] : [var.kms_key]
    content {
      default_kms_key_name = encryption.value
    }
  }
  lifecycle { prevent_destroy = true }
}

resource "google_iam_workload_identity_pool" "github" {
  count                     = local.github ? 1 : 0
  workload_identity_pool_id = "${var.name_prefix}-github"
  display_name              = "GitHub Actions"
}
resource "google_iam_workload_identity_pool_provider" "github" {
  count                              = local.github ? 1 : 0
  workload_identity_pool_id          = google_iam_workload_identity_pool.github[0].workload_identity_pool_id
  workload_identity_pool_provider_id = "github"
  attribute_mapping = {
    "google.subject"         = "assertion.sub"
    "attribute.repository"   = "assertion.repository"
    "attribute.environment"  = "assertion.environment"
    "attribute.event_name"   = "assertion.event_name"
    "attribute.workflow_ref" = "assertion.job_workflow_ref"
  }
  # Only this repository's tokens are accepted at all.
  attribute_condition = "assertion.repository == '${var.github_repository}'"
  oidc {
    issuer_uri = "https://token.actions.githubusercontent.com"
  }
}

resource "google_service_account" "github" {
  for_each   = local.identities
  account_id = lookup(var.names, each.key, "${var.name_prefix}-github-${each.key}")
  lifecycle {
    precondition {
      condition     = each.key != "apply" || length(var.apply_roles) > 0
      error_message = "Choose apply_roles for the apply service account."
    }
  }
}
locals {
  pool = local.github ? google_iam_workload_identity_pool.github[0].name : ""
  # Plans may run anywhere in the repository; apply and release only in the
  # approved environment.
  principals = {
    plan    = "principalSet://iam.googleapis.com/${local.pool}/attribute.repository/${var.github_repository}"
    apply   = "principalSet://iam.googleapis.com/${local.pool}/attribute.environment/${var.github_environment}"
    release = "principalSet://iam.googleapis.com/${local.pool}/attribute.environment/${var.github_environment}"
  }
}
resource "google_service_account_iam_member" "github" {
  for_each           = local.github ? google_service_account.github : {}
  service_account_id = each.value.name
  role               = "roles/iam.workloadIdentityUser"
  member             = local.principals[each.key]
}
# Operators impersonate a deploy identity with short-lived tokens, for
# example with GOOGLE_IMPERSONATE_SERVICE_ACCOUNT; see infra/README.md.
resource "google_service_account_iam_member" "operators" {
  for_each = { for pair in flatten([for identity in local.identities : [
    for member in var.operators[identity] : { key = "${identity} ${member}", identity = identity, member = member }
  ]]) : pair.key => pair }
  service_account_id = google_service_account.github[each.value.identity].name
  role               = "roles/iam.serviceAccountTokenCreator"
  member             = each.value.member
}
resource "google_project_iam_member" "plan_read" {
  count   = contains(local.identities, "plan") ? 1 : 0
  project = var.project
  role    = "roles/viewer"
  member  = "serviceAccount:${google_service_account.github["plan"].email}"
}
resource "google_project_iam_member" "apply" {
  for_each = contains(local.identities, "apply") ? toset(var.apply_roles) : toset([])
  project  = var.project
  role     = each.value
  member   = "serviceAccount:${google_service_account.github["apply"].email}"
}
# Plan and apply read and lock state. The release identity gets its
# permissions from each installation stack, scoped to that installation.
resource "google_storage_bucket_iam_member" "state" {
  for_each = setintersection(local.identities, ["plan", "apply"])
  bucket   = google_storage_bucket.state.name
  role     = "roles/storage.objectUser"
  member   = "serviceAccount:${google_service_account.github[each.key].email}"
}

output "state_bucket" { value = google_storage_bucket.state.name }
output "workload_identity_provider" { value = local.github ? google_iam_workload_identity_pool_provider.github[0].name : "" }
output "github_service_accounts" { value = { for k, s in google_service_account.github : k => s.email } }
output "service_accounts" { value = { for k, s in google_service_account.github : k => s.email } }
