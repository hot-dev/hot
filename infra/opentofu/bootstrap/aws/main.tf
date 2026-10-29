# State for Hot installations on AWS, and optionally the GitHub identities
# that plan, apply, and release them. Apply this root once, separately from
# any installation, with its own local or remote state: destroying an
# installation then can never remove the only copy of its state.
terraform {
  required_version = ">= 1.13.1, < 1.14.0"
  required_providers {
    aws = { source = "hashicorp/aws", version = "6.69.1" }
  }
}
provider "aws" {
  region = var.region
  default_tags {
    tags = { ManagedBy = "OpenTofu", HotStateBootstrap = var.state_bucket }
  }
}

variable "region" {
  type = string
}
variable "state_bucket" {
  description = "Name for the new state bucket."
  type        = string
  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$", var.state_bucket))
    error_message = "state_bucket must be a valid S3 bucket name."
  }
}
variable "kms_key_arn" {
  description = "Customer-managed KMS key for state objects. Empty uses S3-managed keys."
  type        = string
  default     = ""
}
variable "github_repository" {
  description = "OWNER/REPO allowed to plan, apply, and release with OIDC. Empty skips the GitHub identities."
  type        = string
  default     = ""
  validation {
    condition     = var.github_repository == "" || can(regex("^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$", var.github_repository))
    error_message = "github_repository must be OWNER/REPO."
  }
}
variable "github_environment" {
  description = "GitHub Environment whose jobs may apply and release."
  type        = string
  default     = "production"
}
variable "create_github_oidc_provider" {
  description = "Create the account's GitHub OIDC provider; set false if it already exists."
  type        = bool
  default     = true
}
variable "apply_policy_arns" {
  description = "Policies for the apply role. Choose them for your account's guardrails; there is no default."
  type        = list(string)
  default     = []
}
variable "name_prefix" {
  type    = string
  default = "hot"
}
variable "operators" {
  description = "IAM roles, users, or accounts that may assume a deploy role to run OpenTofu or the operator helper themselves, by role: plan, apply, release. For example { apply = [\"arn:aws:iam::123456789012:role/platform-admins\"] }."
  type = object({
    plan    = optional(list(string), [])
    apply   = optional(list(string), [])
    release = optional(list(string), [])
  })
  default = {}
  validation {
    condition = alltrue([for principal in flatten([var.operators.plan, var.operators.apply, var.operators.release]) :
    can(regex("^arn:aws[a-z-]*:iam::[0-9]{12}:(root|role/.+|user/.+)$", principal))])
    error_message = "operators entries must be IAM role, user, or account root ARNs."
  }
}
variable "names" {
  description = "Role names for the deploy roles, such as { apply = \"infra-provisioner\" }. The default is NAME_PREFIX-github-ROLE."
  type        = map(string)
  default     = {}
  validation {
    condition = alltrue([for role, name in var.names :
    contains(["plan", "apply", "release"], role) && can(regex("^[A-Za-z0-9+=,.@_-]{1,64}$", name))])
    error_message = "names maps plan, apply, or release to an IAM role name."
  }
}

locals {
  github = var.github_repository != ""
  oidc   = "token.actions.githubusercontent.com"
  # A deploy role exists for GitHub, for the operators who assume it, or both.
  roles = toset([for role in ["plan", "apply", "release"] : role if local.github || length(var.operators[role]) > 0])
}

resource "aws_s3_bucket" "state" {
  bucket = var.state_bucket
  lifecycle { prevent_destroy = true }
}
resource "aws_s3_bucket_versioning" "state" {
  bucket = aws_s3_bucket.state.id
  versioning_configuration { status = "Enabled" }
}
resource "aws_s3_bucket_server_side_encryption_configuration" "state" {
  bucket = aws_s3_bucket.state.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm     = var.kms_key_arn == "" ? "AES256" : "aws:kms"
      kms_master_key_id = var.kms_key_arn == "" ? null : var.kms_key_arn
    }
    bucket_key_enabled = var.kms_key_arn != ""
  }
}
resource "aws_s3_bucket_public_access_block" "state" {
  bucket                  = aws_s3_bucket.state.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}
resource "aws_s3_bucket_ownership_controls" "state" {
  bucket = aws_s3_bucket.state.id
  rule { object_ownership = "BucketOwnerEnforced" }
}
resource "aws_s3_bucket_policy" "state" {
  bucket = aws_s3_bucket.state.id
  policy = jsonencode({ Version = "2012-10-17", Statement = [{
    Sid       = "DenyInsecureTransport"
    Effect    = "Deny"
    Principal = "*"
    Action    = "s3:*"
    Resource  = [aws_s3_bucket.state.arn, "${aws_s3_bucket.state.arn}/*"]
    Condition = { Bool = { "aws:SecureTransport" = "false" } }
  }] })
  depends_on = [aws_s3_bucket_public_access_block.state]
}
# Old state versions are the recovery path after a bad apply; keep them.
resource "aws_s3_bucket_lifecycle_configuration" "state" {
  bucket = aws_s3_bucket.state.id
  rule {
    id     = "keep-recent-state-versions"
    status = "Enabled"
    filter {}
    noncurrent_version_expiration {
      noncurrent_days           = 90
      newer_noncurrent_versions = 20
    }
  }
}

# GitHub OIDC: each workflow gets its own role, limited by the token's subject.
resource "aws_iam_openid_connect_provider" "github" {
  count          = local.github && var.create_github_oidc_provider ? 1 : 0
  url            = "https://${local.oidc}"
  client_id_list = ["sts.amazonaws.com"]
}
data "aws_caller_identity" "current" {}
locals {
  oidc_arn = "arn:aws:iam::${data.aws_caller_identity.current.account_id}:oidc-provider/${local.oidc}"
  subjects = {
    # Plans run on pull requests and on the merge before apply.
    plan    = ["repo:${var.github_repository}:pull_request", "repo:${var.github_repository}:ref:refs/heads/main"]
    apply   = ["repo:${var.github_repository}:environment:${var.github_environment}"]
    release = ["repo:${var.github_repository}:environment:${var.github_environment}"]
  }
}
# Operators assume a role with their own credentials, for example through an
# AWS CLI profile with role_arn; see infra/README.md.
resource "aws_iam_role" "github" {
  for_each = local.roles
  name     = lookup(var.names, each.key, "${var.name_prefix}-github-${each.key}")
  assume_role_policy = jsonencode({ Version = "2012-10-17", Statement = concat(local.github ? [{
    Effect    = "Allow"
    Principal = { Federated = local.oidc_arn }
    Action    = "sts:AssumeRoleWithWebIdentity"
    Condition = {
      StringEquals = { "${local.oidc}:aud" = "sts.amazonaws.com" }
      StringLike   = { "${local.oidc}:sub" = local.subjects[each.key] }
    }
    }] : [], length(var.operators[each.key]) > 0 ? [{
    Effect    = "Allow"
    Principal = { AWS = var.operators[each.key] }
    Action    = ["sts:AssumeRole", "sts:SetSourceIdentity"]
  }] : []) })
  lifecycle {
    precondition {
      condition     = each.key != "apply" || length(var.apply_policy_arns) > 0
      error_message = "Choose apply_policy_arns for the apply role."
    }
  }
  depends_on = [aws_iam_openid_connect_provider.github]
}
resource "aws_iam_role_policy_attachment" "plan_read" {
  count      = contains(local.roles, "plan") ? 1 : 0
  role       = aws_iam_role.github["plan"].name
  policy_arn = "arn:aws:iam::aws:policy/ReadOnlyAccess"
}
resource "aws_iam_role_policy_attachment" "apply" {
  for_each   = contains(local.roles, "apply") ? toset(var.apply_policy_arns) : toset([])
  role       = aws_iam_role.github["apply"].name
  policy_arn = each.value
}
# Plan and apply read and lock state. The release role gets its permissions
# from each installation stack (release_principal), scoped to that installation.
resource "aws_iam_role_policy" "state" {
  for_each = setintersection(local.roles, ["plan", "apply"])
  role     = aws_iam_role.github[each.key].id
  policy = jsonencode({ Version = "2012-10-17", Statement = concat([
    { Effect = "Allow", Action = ["s3:ListBucket"], Resource = aws_s3_bucket.state.arn },
    { Effect = "Allow", Action = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"], Resource = "${aws_s3_bucket.state.arn}/*" },
    ], var.kms_key_arn == "" ? [] : [
    { Effect = "Allow", Action = ["kms:Decrypt", "kms:GenerateDataKey"], Resource = var.kms_key_arn },
  ]) })
}

output "state_bucket" { value = aws_s3_bucket.state.id }
output "github_roles" { value = { for k, r in aws_iam_role.github : k => r.arn } }
output "roles" { value = { for k, r in aws_iam_role.github : k => r.arn } }
