# Provisioned GCS buckets (storage.<role>.provision): uniform access, public
# access prevented, versioned so an overwritten or deleted object can be
# recovered for noncurrent_days, with abandoned multipart uploads removed after
# a week. Google encrypts objects at rest. Existing buckets are never managed;
# main.tf grants the host's access to those.
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
# A provisioned bucket's whole IAM policy, in place of the one Google gives a
# new bucket, whose project-viewer bindings read every object: a pull request's
# plan runs as a viewer, and needs only bucket settings and this policy, which
# the bootstrap's plan role grants at the project. Project owners and editors
# keep their bindings, since an apply identity may manage the bucket through
# them. The host uses objects under each role's prefix only. Any other grant on
# the bucket, such as another installation's, is removed by the next apply.
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
      members = ["serviceAccount:${google_service_account.host.email}"]
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
