# State for Hot installations on Google Cloud, and optionally the GitHub
# identities that plan, apply, and release them. Apply this root once,
# separately from any installation, so destroying an installation can never
# remove the only copy of its state.
terraform {
  required_version = ">= 1.13.1, < 1.14.0"
  required_providers {
    google = { source = "hashicorp/google", version = "8.7.0" }
    random = { source = "hashicorp/random", version = "3.9.1" }
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
  description = "Cloud KMS key for state objects, as projects/P/locations/L/keyRings/R/cryptoKeys/K in the bucket's location. Empty uses Google-managed encryption. The bootstrap lets the project's Cloud Storage service agent use the key, which needs permission to set the key's IAM policy."
  type        = string
  default     = ""
  validation {
    condition     = var.kms_key == "" || can(regex("^projects/[^/]+/locations/[^/]+/keyRings/[^/]+/cryptoKeys/[^/]+$", var.kms_key))
    error_message = "kms_key must be a key name: projects/PROJECT/locations/LOCATION/keyRings/RING/cryptoKeys/KEY."
  }
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
variable "github_repository_id" {
  description = "The repository's numeric ID, from `gh api repos/OWNER/REPO --jq .id`. Required with github_repository: names can be registered again after a rename or deletion, IDs cannot."
  type        = string
  default     = ""
  validation {
    condition     = can(regex("^[0-9]*$", var.github_repository_id))
    error_message = "github_repository_id must be the repository's numeric ID."
  }
}
variable "github_repository_owner_id" {
  description = "The repository owner's numeric ID, from `gh api repos/OWNER/REPO --jq .owner.id`. Required with github_repository."
  type        = string
  default     = ""
  validation {
    condition     = can(regex("^[0-9]*$", var.github_repository_owner_id))
    error_message = "github_repository_owner_id must be the owner's numeric ID."
  }
}
variable "github_environment" {
  description = "GitHub Environment whose jobs may apply and release, from main only."
  type        = string
  default     = "production"
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
  # As on AWS, a version goes only once it is 90 days old and 20 newer ones
  # exist: one long apply can write dozens.
  lifecycle_rule {
    condition {
      num_newer_versions         = 20
      days_since_noncurrent_time = 90
      with_state                 = "ARCHIVED"
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
  depends_on = [google_kms_crypto_key_iam_member.state]
}
# Cloud Storage encrypts with the key as the project's service agent, so the
# bucket cannot be created until the agent may use it.
data "google_storage_project_service_account" "state" {
  count = var.kms_key == "" ? 0 : 1
}
resource "google_kms_crypto_key_iam_member" "state" {
  count         = var.kms_key == "" ? 0 : 1
  crypto_key_id = var.kms_key
  role          = "roles/cloudkms.cryptoKeyEncrypterDecrypter"
  member        = "serviceAccount:${data.google_storage_project_service_account.state[0].email_address}"
}

# Workload Identity Federation, impersonation, and project IAM use these
# APIs, which a new project has off, and a custom role cannot list a permission
# of a service whose API is off: the plan identity's role lists Pub/Sub's, and
# the stacks enable Pub/Sub only at their own first apply. They stay on if the
# bootstrap is destroyed.
resource "google_project_service" "identity" {
  for_each = length(local.identities) > 0 ? toset(concat(
    ["iam.googleapis.com", "iamcredentials.googleapis.com", "sts.googleapis.com", "cloudresourcemanager.googleapis.com"],
    contains(local.identities, "plan") ? ["pubsub.googleapis.com"] : [],
  )) : toset([])
  service            = each.value
  disable_on_destroy = false
}

resource "google_iam_workload_identity_pool" "github" {
  count                     = local.github ? 1 : 0
  workload_identity_pool_id = "${var.name_prefix}-github"
  display_name              = "GitHub Actions"
  depends_on                = [google_project_service.identity]
  lifecycle {
    precondition {
      condition     = var.github_repository_id != "" && var.github_repository_owner_id != ""
      error_message = "Set github_repository_id and github_repository_owner_id: gh api repos/OWNER/REPO --jq '.id, .owner.id'."
    }
  }
}
resource "google_iam_workload_identity_pool_provider" "github" {
  count                              = local.github ? 1 : 0
  workload_identity_pool_id          = google_iam_workload_identity_pool.github[0].workload_identity_pool_id
  workload_identity_pool_provider_id = "github"
  # Every claim mapped here is in every GitHub token; environment is not,
  # so access is granted on the subject, which carries it.
  attribute_mapping = {
    "google.subject"                = "assertion.sub"
    "attribute.repository"          = "assertion.repository"
    "attribute.repository_id"       = "assertion.repository_id"
    "attribute.repository_owner_id" = "assertion.repository_owner_id"
    "attribute.ref"                 = "assertion.ref"
    "attribute.event_name"          = "assertion.event_name"
    "attribute.workflow_ref"        = "assertion.job_workflow_ref"
  }
  # Only this repository's tokens are accepted at all, by its immutable IDs.
  # Jobs in an environment (apply and release) are accepted only from main,
  # so an unmerged branch's workflow never reaches them, even with the
  # environment's approval.
  attribute_condition = join(" && ", [
    "assertion.repository_id == '${var.github_repository_id}'",
    "assertion.repository_owner_id == '${var.github_repository_owner_id}'",
    "(!assertion.sub.contains(':environment:') || assertion.ref == 'refs/heads/main')",
  ])
  oidc {
    issuer_uri = "https://token.actions.githubusercontent.com"
  }
}

resource "google_service_account" "github" {
  for_each   = local.identities
  account_id = lookup(var.names, each.key, "${var.name_prefix}-github-${each.key}")
  depends_on = [google_project_service.identity]
  lifecycle {
    precondition {
      condition     = each.key != "apply" || length(var.apply_roles) > 0
      error_message = "Choose apply_roles for the apply service account."
    }
  }
}
locals {
  pool = local.github ? google_iam_workload_identity_pool.github[0].name : ""
  # The same subjects the AWS roles trust. The subject names the repository
  # as OWNER/REPO or, for repositories created or opted in since July 2026,
  # as OWNER@ID/REPO@ID.
  subject_repositories = local.github ? [
    "repo:${var.github_repository}",
    "repo:${split("/", var.github_repository)[0]}@${var.github_repository_owner_id}/${split("/", var.github_repository)[1]}@${var.github_repository_id}",
  ] : []
  contexts = {
    # Plans run on pull requests and on main; apply and release only in the
    # approved environment.
    plan    = ["pull_request", "ref:refs/heads/main"]
    apply   = ["environment:${var.github_environment}"]
    release = ["environment:${var.github_environment}"]
  }
  workload_principals = flatten([for identity in local.identities : [
    for pair in setproduct(local.subject_repositories, local.contexts[identity]) : { identity = identity, subject = join(":", pair) }
  ]])
}
resource "google_service_account_iam_member" "github" {
  for_each           = { for p in local.workload_principals : "${p.identity} ${p.subject}" => p }
  service_account_id = google_service_account.github[each.value.identity].name
  role               = "roles/iam.workloadIdentityUser"
  member             = "principal://iam.googleapis.com/${local.pool}/subject/${each.value.subject}"
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
  count      = contains(local.identities, "plan") ? 1 : 0
  project    = var.project
  role       = "roles/viewer"
  member     = "serviceAccount:${google_service_account.github["plan"].email}"
  depends_on = [google_project_service.identity]
}
# roles/viewer reads resources but not the IAM policies on Pub/Sub topics and
# subscriptions or on buckets, and a plan refreshes every IAM member in state.
# It reads a bucket's settings only through the project-viewer binding on the
# bucket itself, which an authoritative bucket policy drops. None of this reads
# objects or messages. A bucket in another project needs the same read there.
# Custom role IDs are never reused, as random_id.roles in the GCP single-machine
# stack explains, so this one carries a suffix too.
resource "random_id" "plan_role" {
  count       = contains(local.identities, "plan") ? 1 : 0
  byte_length = 3
}
resource "google_project_iam_custom_role" "plan_read" {
  count   = contains(local.identities, "plan") ? 1 : 0
  project = var.project
  role_id = "${replace(var.name_prefix, "-", "_")}_github_plan_read_${random_id.plan_role[0].hex}"
  title   = "Hot plan: IAM policies and bucket settings"
  permissions = [
    "pubsub.subscriptions.getIamPolicy",
    "pubsub.topics.getIamPolicy",
    "storage.buckets.get",
    "storage.buckets.getIamPolicy",
  ]
  depends_on = [google_project_service.identity]
}
resource "google_project_iam_member" "plan_policies" {
  count   = contains(local.identities, "plan") ? 1 : 0
  project = var.project
  role    = google_project_iam_custom_role.plan_read[0].id
  member  = "serviceAccount:${google_service_account.github["plan"].email}"
}
resource "google_project_iam_member" "apply" {
  for_each   = contains(local.identities, "apply") ? toset(var.apply_roles) : toset([])
  project    = var.project
  role       = each.value
  member     = "serviceAccount:${google_service_account.github["apply"].email}"
  depends_on = [google_project_service.identity]
}
# Plan and apply read state and take its lock, a PREFIX/WORKSPACE.tflock
# object beside each state object. Only apply changes an existing state. The
# release identity gets its permissions from each installation stack, scoped
# to that installation.
resource "google_storage_bucket_iam_member" "state" {
  for_each = setintersection(local.identities, ["plan", "apply"])
  bucket   = google_storage_bucket.state.name
  role     = each.key == "apply" ? "roles/storage.objectUser" : "roles/storage.objectViewer"
  member   = "serviceAccount:${google_service_account.github[each.key].email}"
}
resource "google_storage_bucket_iam_member" "plan_lock" {
  count  = contains(local.identities, "plan") ? 1 : 0
  bucket = google_storage_bucket.state.name
  role   = "roles/storage.objectUser"
  member = "serviceAccount:${google_service_account.github["plan"].email}"
  condition {
    title       = "state-locks"
    description = "Create and remove OpenTofu state locks only."
    expression  = "resource.name.endsWith('.tflock')"
  }
}
# The gcs backend writes an empty state the first time it opens a missing one,
# so an installation's first plan creates its state, and apply only runs after
# a plan. objectCreator cannot overwrite, delete, or read, so plan can add a
# missing state but cannot change one that exists.
resource "google_storage_bucket_iam_member" "plan_new_state" {
  count  = contains(local.identities, "plan") ? 1 : 0
  bucket = google_storage_bucket.state.name
  role   = "roles/storage.objectCreator"
  member = "serviceAccount:${google_service_account.github["plan"].email}"
  condition {
    title       = "new-states"
    description = "Create a missing OpenTofu state only."
    expression  = "resource.name.endsWith('.tfstate')"
  }
}

output "state_bucket" { value = google_storage_bucket.state.name }
output "backend_override" {
  description = "An installation's backend_override.tf, with NAME to replace. The gcs backend always locks state."
  value       = <<-EOT
    terraform {
      backend "gcs" {
        bucket = "${google_storage_bucket.state.name}"
        prefix = "NAME"
      }
    }
  EOT
}
output "workload_identity_provider" { value = local.github ? google_iam_workload_identity_pool_provider.github[0].name : "" }
output "github_service_accounts" { value = { for k, s in google_service_account.github : k => s.email } }
output "service_accounts" { value = { for k, s in google_service_account.github : k => s.email } }
