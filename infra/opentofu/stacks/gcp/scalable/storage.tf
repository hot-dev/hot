# Provisioned GCS buckets (storage.<role>.provision): uniform access, public
# access prevented, versioned so an overwritten or deleted object can be
# recovered for noncurrent_days, with abandoned multipart uploads removed after
# a week. Google encrypts objects at rest. Existing buckets are never managed.
# The host's access is granted in main.tf, limited to each role's prefix.
locals {
  # Both roles may share one bucket; the contract makes their settings agree.
  provisioned_buckets = { for name, roles in {
    for s in [local.config.storage.build, local.config.storage.file] : s.bucket => s... if s.backend == "gcs" && s.provision
  } : name => roles[0] }
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
  lifecycle { prevent_destroy = true }
}
# Every role reads and writes objects under each role's prefix only.
resource "google_storage_bucket_iam_member" "storage" {
  for_each = { for pair in setproduct(keys(local.roles), ["build", "file"]) : "${pair[0]}-${pair[1]}" => {
    account = google_service_account.role[pair[0]].email, storage = local.config.storage[pair[1]], role = pair[1]
  } if local.config.storage[pair[1]].backend == "gcs" }
  bucket = each.value.storage.bucket
  role   = "roles/storage.objectUser"
  member = "serviceAccount:${each.value.account}"
  condition {
    title      = "hot-${local.config.name}-${each.key}"
    expression = "resource.type == \"storage.googleapis.com/Bucket\" || resource.name.startsWith(\"projects/_/buckets/${each.value.storage.bucket}/objects/${trim(each.value.storage.prefix, "/")}/\")"
  }
  depends_on = [google_storage_bucket.storage]
}
