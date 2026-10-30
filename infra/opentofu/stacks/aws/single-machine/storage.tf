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
