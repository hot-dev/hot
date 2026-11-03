# Provisioned S3 buckets (storage.<role>.provision): encrypted, private,
# TLS-only, versioned so an overwritten or deleted object can be recovered for
# noncurrent_days, with abandoned multipart uploads removed after a week.
# Existing buckets are never managed. The host's access is granted in main.tf,
# limited to each role's prefix.
locals {
  # Both roles may share one bucket; the contract makes their settings agree.
  provisioned_buckets = { for name, roles in {
    for s in [local.config.storage.build, local.config.storage.file] : s.bucket => s... if s.backend == "s3" && s.provision
  } : name => roles[0] }
  # A bucket name has no slash, and Hot and the hosts' grant read a prefix the
  # same with or without a trailing one, so each location has one spelling.
  storage_locations = { for role, s in local.config.storage : role => "${s.backend}://${s.bucket}/${trim(s.prefix, "/")}" }
}
resource "aws_s3_bucket" "storage" {
  for_each      = local.provisioned_buckets
  bucket        = each.key
  force_destroy = false
  tags          = { HotInstallation = local.config.name }
  lifecycle { prevent_destroy = true }
}
resource "aws_s3_bucket_ownership_controls" "storage" {
  for_each = aws_s3_bucket.storage
  bucket   = each.value.id
  rule {
    object_ownership = "BucketOwnerEnforced"
  }
}
resource "aws_s3_bucket_public_access_block" "storage" {
  for_each                = aws_s3_bucket.storage
  bucket                  = each.value.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}
resource "aws_s3_bucket_server_side_encryption_configuration" "storage" {
  for_each = aws_s3_bucket.storage
  bucket   = each.value.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}
resource "aws_s3_bucket_versioning" "storage" {
  for_each = aws_s3_bucket.storage
  bucket   = each.value.id
  versioning_configuration {
    status = "Enabled"
  }
}
resource "aws_s3_bucket_lifecycle_configuration" "storage" {
  for_each = aws_s3_bucket.storage
  bucket   = each.value.id
  rule {
    id     = "abandoned-uploads"
    status = "Enabled"
    filter {}
    abort_incomplete_multipart_upload {
      days_after_initiation = 7
    }
  }
  rule {
    id     = "old-versions"
    status = "Enabled"
    filter {}
    noncurrent_version_expiration {
      noncurrent_days = local.provisioned_buckets[each.key].noncurrent_days
    }
    # A deleted object's marker, once its old versions are gone.
    expiration {
      expired_object_delete_marker = true
    }
  }
  depends_on = [aws_s3_bucket_versioning.storage]
}
resource "aws_s3_bucket_policy" "storage" {
  for_each = aws_s3_bucket.storage
  bucket   = each.value.id
  policy = jsonencode({ Version = "2012-10-17", Statement = [{
    Sid       = "DenyInsecureTransport", Effect = "Deny", Principal = "*", Action = "s3:*"
    Resource  = [each.value.arn, "${each.value.arn}/*"]
    Condition = { Bool = { "aws:SecureTransport" = "false" } }
  }] })
  depends_on = [aws_s3_bucket_public_access_block.storage]
}
# A scalable installation does not move storage, and no host can refuse a move:
# its new launch template replaces every host with one that has no earlier release
# to compare with, so each would report deployed on the new location, with the
# records still on the old one and the hosts' grant there gone. So the plan refuses
# a change of backend, bucket or prefix, against those kept here from the first
# apply. An installation from before this record has nothing to compare with, so
# its next plan is not refused and its next apply records where storage is then.
# One that has stored nothing can start again with
# -replace=terraform_data.storage_locations, and the refusal says so, because a
# location here may never have worked: the record does not wait for the buckets, so
# a first apply that fails keeps it, and preflight only warns on a mistyped existing
# bucket. A new name for a bucket this stack created still stops on prevent_destroy.
resource "terraform_data" "storage_locations" {
  input = local.storage_locations
  lifecycle {
    ignore_changes = [input]
    postcondition {
      condition     = alltrue([for role, location in self.input : local.storage_locations[role] == location])
      error_message = "A scalable installation does not move storage, and Hot would start on the new location with its records still on the old one. Put storage back on the bucket and prefix it started on: ${join(", ", [for role, location in self.input : "storage.${role} was ${location} and is now ${local.storage_locations[role]}" if local.storage_locations[role] != location])}. An installation that has stored nothing, such as one whose first apply failed or named the wrong existing bucket, can start again with tofu apply -replace=terraform_data.storage_locations. Renaming a bucket this stack already created still stops on its prevent_destroy."
    }
  }
}
