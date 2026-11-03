variable "installation" {
  description = "See the shared single-machine-contract module. No plaintext secrets."
  type        = any
  # The workflows choose the root from their own preset input. A scalable
  # installation planned here would reach the VM as an in-place change.
  validation {
    condition     = try(coalesce(var.installation.preset, "single-machine"), "single-machine") == "single-machine"
    error_message = "This root needs installation.preset = single-machine; plan stacks/aws/scalable for a scalable installation."
  }
  # The contract's typed object drops a key it does not know, so only here is
  # one still there to refuse. These lists follow that object, as DATA_KEYS in
  # infra/preflight/preflight.py does. keys(null) errors and endpoint = null is
  # legal, so keys() stays inside try().
  validation {
    condition = (
      length(setsubtract(try(keys(var.installation.data), []), ["postgres", "valkey"])) == 0 &&
      length(setsubtract(try(keys(var.installation.data.postgres), []), ["mode", "runtime_role", "version", "tier", "storage_gb", "high_availability", "backup_retention_days", "deletion_protection", "admin_password_version", "endpoint"])) == 0 &&
      length(setsubtract(try(keys(var.installation.data.valkey), []), ["mode", "node_type", "high_availability", "auth_token_version", "endpoint"])) == 0 &&
      length(setsubtract(try(keys(var.installation.data.postgres.endpoint), []), ["deny_cidrs"])) == 0 &&
      length(setsubtract(try(keys(var.installation.data.valkey.endpoint), []), ["deny_cidrs"])) == 0
    )
    error_message = "installation.data has a key the contract does not know, which OpenTofu would drop with no error. A misspelled or misplaced deny list leaves its networks open to user code; it belongs at data.<service>.endpoint.deny_cidrs."
  }
}
variable "acknowledge_unqualified" {
  description = "Development preview: cloud qualification and the baked host agent are not delivered yet."
  type        = bool
  default     = false
  validation {
    condition     = var.acknowledge_unqualified
    error_message = "This is an unqualified development stack. Read infra/README.md before explicitly acknowledging it."
  }
}
variable "release_principal" {
  description = "IAM role ARN that releases this installation, such as the state bootstrap's release role. Empty grants nothing. Each installation adds an inline policy to the role, and IAM's size limit fits about 6 scalable or 9 single-machine installations on one role; use separate release roles beyond that."
  type        = string
  default     = ""
  validation {
    condition     = var.release_principal == "" || can(regex("^arn:aws[a-z-]*:iam::[0-9]{12}:role/.+$", var.release_principal))
    error_message = "release_principal must be an IAM role ARN."
  }
}
variable "public_certificate_arn" {
  description = "ACM certificate covering every public hostname, DNS-validated by the operator. Required when an endpoint is public."
  type        = string
  default     = ""
  validation {
    condition     = var.public_certificate_arn == "" || can(regex("^arn:aws[a-z-]*:acm:[a-z0-9-]+:[0-9]{12}:certificate/.+$", var.public_certificate_arn))
    error_message = "public_certificate_arn must be an ACM certificate ARN."
  }
}
