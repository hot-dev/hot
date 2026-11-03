variable "installation" {
  description = "See the shared single-machine-contract module. No plaintext secrets."
  type        = any
  # The workflows choose the root from their own preset input. A scalable
  # installation planned here would reach the VM as an in-place change.
  validation {
    condition     = try(coalesce(var.installation.preset, "single-machine"), "single-machine") == "single-machine"
    error_message = "This root needs installation.preset = single-machine; plan stacks/gcp/scalable for a scalable installation."
  }
  # The contract's typed object drops a key it does not know, so only the raw
  # value here can still show one. Keep these lists in step with the contract
  # and with DATA_KEYS in infra/preflight/preflight.py.
  validation {
    condition = (
      length(setsubtract(try(keys(var.installation.data), []), ["postgres", "valkey"])) == 0
      && length(setsubtract(try(keys(var.installation.data.postgres), []), ["mode", "runtime_role", "version", "tier", "storage_gb", "high_availability", "backup_retention_days", "deletion_protection", "admin_password_version", "endpoint"])) == 0
      && length(setsubtract(try(keys(var.installation.data.valkey), []), ["mode", "node_type", "high_availability", "auth_token_version", "endpoint"])) == 0
      && length(setsubtract(try(keys(var.installation.data.postgres.endpoint), []), ["deny_cidrs"])) == 0
      && length(setsubtract(try(keys(var.installation.data.valkey.endpoint), []), ["deny_cidrs"])) == 0
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
  description = "Service account email that releases this installation, such as the state bootstrap's release account. Empty grants nothing."
  type        = string
  default     = ""
  validation {
    condition     = var.release_principal == "" || can(regex("^[a-z0-9-]+@[a-z0-9-]+\\.iam\\.gserviceaccount\\.com$", var.release_principal))
    error_message = "release_principal must be a service account email."
  }
}
