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
  description = "IAM roles or users that may assume a deploy role to run OpenTofu or the operator helper themselves, by role: plan, apply, release. Users must sign in with MFA. For example { apply = [\"arn:aws:iam::123456789012:role/platform-admins\"] }."
  type = object({
    plan    = optional(list(string), [])
    apply   = optional(list(string), [])
    release = optional(list(string), [])
  })
  default = {}
  validation {
    # An account root would delegate the role to every principal in that
    # account allowed sts:AssumeRole, such as PowerUserAccess holders.
    condition = alltrue([for principal in flatten([var.operators.plan, var.operators.apply, var.operators.release]) :
    can(regex("^arn:aws[a-z-]*:iam::[0-9]{12}:(role|user)/.+$", principal))])
    error_message = "operators entries must be IAM role or user ARNs, not account roots."
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

# GitHub OIDC: each workflow gets its own role, limited by the token's
# subject and pinned to the repository's immutable IDs.
#
# The account has one GitHub OIDC provider, which other repositories' roles
# may share, so this root never destroys it. To stop managing it, run
# `tofu state rm 'aws_iam_openid_connect_provider.github[0]'` first.
resource "aws_iam_openid_connect_provider" "github" {
  count          = local.github && var.create_github_oidc_provider ? 1 : 0
  url            = "https://${local.oidc}"
  client_id_list = ["sts.amazonaws.com"]
  lifecycle { prevent_destroy = true }
}
data "aws_caller_identity" "current" {}
locals {
  oidc_arn = "arn:aws:iam::${data.aws_caller_identity.current.account_id}:oidc-provider/${local.oidc}"
  # The subject names the repository as OWNER/REPO or, for repositories
  # created or opted in since July 2026, as OWNER@ID/REPO@ID.
  subject_repositories = local.github ? [
    "repo:${var.github_repository}",
    "repo:${split("/", var.github_repository)[0]}@${var.github_repository_owner_id}/${split("/", var.github_repository)[1]}@${var.github_repository_id}",
  ] : []
  contexts = {
    # Plans run on pull requests and on the merge before apply.
    plan    = ["pull_request", "ref:refs/heads/main"]
    apply   = ["environment:${var.github_environment}"]
    release = ["environment:${var.github_environment}"]
  }
  subjects       = { for role, contexts in local.contexts : role => [for pair in setproduct(local.subject_repositories, contexts) : join(":", pair)] }
  operator_users = { for role in ["plan", "apply", "release"] : role => [for p in var.operators[role] : p if strcontains(p, ":user/")] }
  operator_roles = { for role in ["plan", "apply", "release"] : role => [for p in var.operators[role] : p if !strcontains(p, ":user/")] }
}
# Operators assume a role with their own credentials, for example through an
# AWS CLI profile with role_arn; see infra/README.md.
resource "aws_iam_role" "github" {
  for_each = local.roles
  name     = lookup(var.names, each.key, "${var.name_prefix}-github-${each.key}")
  # An apply, release, or command can outlive the default hour, and a workflow
  # can ask for no longer than this. Plans are short and run for every pull
  # request, so they keep the hour.
  max_session_duration = each.key == "plan" ? 3600 : 14400
  assume_role_policy = jsonencode({ Version = "2012-10-17", Statement = concat(local.github ? [{
    Effect    = "Allow"
    Principal = { Federated = local.oidc_arn }
    Action    = "sts:AssumeRoleWithWebIdentity"
    Condition = {
      StringEquals = merge({
        "${local.oidc}:aud"                 = "sts.amazonaws.com"
        "${local.oidc}:sub"                 = local.subjects[each.key]
        "${local.oidc}:repository_id"       = var.github_repository_id
        "${local.oidc}:repository_owner_id" = var.github_repository_owner_id
        }, each.key == "plan" ? {} : {
        # Apply and release run only from main: an unmerged branch's
        # workflow never reaches them, even with the environment's approval.
        "${local.oidc}:ref" = "refs/heads/main"
      })
    }
    }] : [], length(local.operator_roles[each.key]) > 0 ? [{
    Effect    = "Allow"
    Principal = { AWS = local.operator_roles[each.key] }
    Action    = ["sts:AssumeRole", "sts:SetSourceIdentity"]
    }] : [], length(local.operator_users[each.key]) > 0 ? [{
    Effect    = "Allow"
    Principal = { AWS = local.operator_users[each.key] }
    Action    = ["sts:AssumeRole", "sts:SetSourceIdentity"]
    Condition = { Bool = { "aws:MultiFactorAuthPresent" = "true" } }
  }] : []) })
  lifecycle {
    precondition {
      condition     = each.key != "apply" || length(var.apply_policy_arns) > 0
      error_message = "Choose apply_policy_arns for the apply role."
    }
    precondition {
      condition     = !local.github || (var.github_repository_id != "" && var.github_repository_owner_id != "")
      error_message = "Set github_repository_id and github_repository_owner_id: gh api repos/OWNER/REPO --jq '.id, .owner.id'."
    }
  }
  depends_on = [aws_iam_openid_connect_provider.github]
}
resource "aws_iam_role_policy_attachment" "plan_read" {
  count      = contains(local.roles, "plan") ? 1 : 0
  role       = aws_iam_role.github["plan"].name
  policy_arn = "arn:aws:iam::aws:policy/ReadOnlyAccess"
}
# ReadOnlyAccess also reads data: objects, logs, and command output. Plans
# run for every pull request and need none of it, only resource settings and
# the state objects.
resource "aws_iam_role_policy" "plan_no_data" {
  count = contains(local.roles, "plan") ? 1 : 0
  name  = "no-data-reads"
  role  = aws_iam_role.github["plan"].id
  policy = jsonencode({ Version = "2012-10-17", Statement = [
    { Effect = "Deny", Action = ["s3:GetObject", "s3:GetObjectVersion"], NotResource = "${aws_s3_bucket.state.arn}/*" },
    { Effect = "Deny", Resource = "*", Action = [
      "logs:GetLogEvents", "logs:GetLogRecord", "logs:FilterLogEvents", "logs:StartQuery", "logs:GetQueryResults",
      "logs:StartLiveTail", "logs:Unmask", "ssm:GetCommandInvocation", "ssm:ListCommandInvocations",
      "rds:DownloadDBLogFilePortion", "rds:DownloadCompleteDBLogFile", "ec2:GetConsoleOutput", "ec2:GetConsoleScreenshot",
    ] },
  ] })
}
resource "aws_iam_role_policy_attachment" "apply" {
  for_each   = contains(local.roles, "apply") ? toset(var.apply_policy_arns) : toset([])
  role       = aws_iam_role.github["apply"].name
  policy_arn = each.value
}
# Plan and apply read state and take its lock, a KEY.tflock object beside
# each state object (the backend's use_lockfile; see backend_override). Only
# apply writes state. The release role gets its permissions from each
# installation stack (release_principal), scoped to that installation.
resource "aws_iam_role_policy" "state" {
  for_each = setintersection(local.roles, ["plan", "apply"])
  role     = aws_iam_role.github[each.key].id
  policy = jsonencode({ Version = "2012-10-17", Statement = concat([
    { Effect = "Allow", Action = ["s3:ListBucket"], Resource = aws_s3_bucket.state.arn },
    { Effect = "Allow", Action = ["s3:GetObject"], Resource = "${aws_s3_bucket.state.arn}/*" },
    { Effect = "Allow", Action = ["s3:PutObject", "s3:DeleteObject"], Resource = each.key == "apply" ? "${aws_s3_bucket.state.arn}/*" : "${aws_s3_bucket.state.arn}/*.tflock" },
    ], var.kms_key_arn == "" ? [] : [
    { Effect = "Allow", Action = ["kms:Decrypt", "kms:GenerateDataKey"], Resource = var.kms_key_arn },
  ]) })
}

output "state_bucket" { value = aws_s3_bucket.state.id }
output "backend_override" {
  description = "An installation's backend_override.tf, with NAME to replace. Without use_lockfile the S3 backend takes no lock, and concurrent applies overwrite each other's state."
  value       = <<-EOT
    terraform {
      backend "s3" {
        bucket       = "${aws_s3_bucket.state.id}"
        key          = "NAME/terraform.tfstate"
        region       = "${var.region}"
        encrypt      = true%{if var.kms_key_arn != ""}
        kms_key_id   = "${var.kms_key_arn}"%{endif}
        use_lockfile = true
      }
    }
  EOT
}
output "github_roles" { value = { for k, r in aws_iam_role.github : k => r.arn } }
output "roles" { value = { for k, r in aws_iam_role.github : k => r.arn } }
