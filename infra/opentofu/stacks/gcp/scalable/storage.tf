# Provisioned GCS buckets (storage.<role>.provision): uniform access, public
# access prevented, versioned so an overwritten or deleted object can be
# recovered for noncurrent_days, with abandoned multipart uploads removed after
# a week. Google encrypts objects at rest. Existing buckets are never managed.
locals {
  # Both roles may share one bucket; the contract makes their settings agree.
  provisioned_buckets = { for name, roles in {
    for s in [local.config.storage.build, local.config.storage.file] : s.bucket => s... if s.backend == "gcs" && s.provision
  } : name => merge(roles[0], { staging = [for s in roles : "${trim(s.prefix, "/")}/.uploads/"] }) }
}
resource "google_storage_bucket" "storage" {
  for_each                    = local.provisioned_buckets
  name                        = each.key
  project                     = local.config.project
  location                    = upper(local.config.region)
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  force_destroy               = false
  labels                      = { hot-installation = local.config.name }
  versioning {
    enabled = true
  }
  lifecycle_rule {
    condition {
      days_since_noncurrent_time = each.value.noncurrent_days
    }
    action {
      type = "Delete"
    }
  }
  lifecycle_rule {
    condition {
      age = 7
    }
    action {
      type = "AbortIncompleteMultipartUpload"
    }
  }
  # Hot stages multipart uploads as objects under <prefix>/.uploads/, which
  # holds nothing committed; a week-old part belongs to an abandoned upload.
  lifecycle_rule {
    condition {
      age            = 7
      matches_prefix = each.value.staging
    }
    action {
      type = "Delete"
    }
  }
  lifecycle { prevent_destroy = true }
}
# Existing buckets: every role reads and writes objects under each role's prefix
# only, and the rest of the bucket's policy stays the operator's.
resource "google_storage_bucket_iam_member" "existing" {
  for_each = { for pair in setproduct(keys(local.roles), ["build", "file"]) : "${pair[0]}-${pair[1]}" => {
    account = google_service_account.role[pair[0]].email, storage = local.config.storage[pair[1]], role = pair[1]
  } if local.config.storage[pair[1]].backend == "gcs" && !local.config.storage[pair[1]].provision }
  bucket = each.value.storage.bucket
  role   = "roles/storage.objectUser"
  member = "serviceAccount:${each.value.account}"
  condition {
    title      = "hot-${local.config.name}-${each.key}"
    expression = "resource.type == \"storage.googleapis.com/Bucket\" || resource.name.startsWith(\"projects/_/buckets/${each.value.storage.bucket}/objects/${trim(each.value.storage.prefix, "/")}/\")"
  }
}
# The bindings earlier versions made, on both kinds of bucket, are only
# forgotten, so the hosts never lose access: destroying one would remove it from
# a provisioned bucket's new policy, or for a moment from an existing bucket,
# where the binding above adopts the one already there.
removed {
  from = google_storage_bucket_iam_member.storage
  lifecycle { destroy = false }
}
# A provisioned bucket's whole IAM policy, as on the single machine: no
# project-viewer bindings, which would let a pull request's plan read every
# object. Any other grant on the bucket, such as another installation's, is
# removed by the next apply.
data "google_iam_policy" "storage" {
  for_each = local.provisioned_buckets
  dynamic "binding" {
    for_each = ["roles/storage.legacyBucketOwner", "roles/storage.legacyObjectOwner"]
    content {
      role    = binding.value
      members = ["projectOwner:${local.config.project}", "projectEditor:${local.config.project}"]
    }
  }
  dynamic "binding" {
    for_each = { for role, s in local.config.storage : role => s if s.backend == "gcs" && s.bucket == each.key }
    content {
      role    = "roles/storage.objectUser"
      members = [for account in google_service_account.role : "serviceAccount:${account.email}"]
      condition {
        title      = "hot-${local.config.name}-${binding.key}"
        expression = "resource.type == \"storage.googleapis.com/Bucket\" || resource.name.startsWith(\"projects/_/buckets/${binding.value.bucket}/objects/${trim(binding.value.prefix, "/")}/\")"
      }
    }
  }
}
resource "google_storage_bucket_iam_policy" "storage" {
  for_each    = local.provisioned_buckets
  bucket      = google_storage_bucket.storage[each.key].name
  policy_data = data.google_iam_policy.storage[each.key].policy_data
  # Destroying this resource empties the bucket's policy, even of its owners.
  lifecycle { prevent_destroy = true }
  # An existing bucket that becomes provisioned loses its binding first, which
  # afterwards would take the access out of this policy.
  depends_on = [google_storage_bucket_iam_member.existing]
}
# A scalable installation does not move storage, and no host can refuse a change
# of backend: its new instance template replaces every host with one that has no
# earlier release to compare with, so each would report deployed on the new
# backend, with the records still on the old one, where Hot refuses to read them.
# So the plan refuses it, against the backends kept here from the first apply.
# An installation that has stored nothing can start again with
# -replace=terraform_data.storage_backends.
resource "terraform_data" "storage_backends" {
  input = { for role, s in local.config.storage : role => s.backend }
  lifecycle {
    ignore_changes = [input]
    postcondition {
      condition     = alltrue([for role, backend in self.input : local.config.storage[role].backend == backend])
      error_message = "A scalable installation does not move storage, and Hot would start on the new backend with its records still on the old one. Put storage back on the backend it started on: ${join(", ", [for role, backend in self.input : "storage.${role} was ${backend} and is now ${local.config.storage[role].backend}" if local.config.storage[role].backend != backend])}. An installation that has stored nothing, such as one whose first apply failed or named the wrong existing bucket, can start again with tofu apply -replace=terraform_data.storage_backends. Moving off a bucket this stack already created still stops on its prevent_destroy."
    }
  }
}
# Another bucket or prefix of the same backend strands the records the same way,
# and no host can refuse that either, so the plan refuses it too. It has a record
# of its own because the one above never takes a new shape: its input is kept
# from the first apply. An installation from before this record has nothing to
# compare with, so its next plan is not refused and its next apply records where
# storage is then. Replacing the backends replaces this as well, which keeps one
# way past the refusal. Each refusal says so, because a location here may never
# have worked: neither record waits for the buckets, so a first apply that fails
# keeps them, and preflight does not look at an existing S3 bucket at all. A new
# name for a bucket this stack created still stops on prevent_destroy.
locals {
  # A bucket name has no slash, and Hot reads a prefix the same with or without
  # a trailing one, so each location has one spelling.
  storage_locations = { for role, s in local.config.storage : role => "${s.bucket}/${trim(s.prefix, "/")}" }
}
resource "terraform_data" "storage_locations" {
  input            = local.storage_locations
  triggers_replace = terraform_data.storage_backends.id
  lifecycle {
    ignore_changes = [input]
    postcondition {
      condition     = alltrue([for role, location in self.input : local.storage_locations[role] == location])
      error_message = "A scalable installation does not move storage, and Hot would start on the new location with its records still on the old one. Put storage back on the bucket and prefix it started on: ${join(", ", [for role, location in self.input : "storage.${role} was ${location} and is now ${local.storage_locations[role]}" if local.storage_locations[role] != location])}. An installation that has stored nothing, such as one whose first apply failed or named the wrong existing bucket, can start again with tofu apply -replace=terraform_data.storage_backends. Renaming a bucket this stack already created still stops on its prevent_destroy."
    }
  }
}
