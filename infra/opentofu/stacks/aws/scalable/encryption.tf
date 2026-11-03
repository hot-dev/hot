variable "state_encryption_passphrase" {
  description = "Independent state/plan key. Supply through TF_VAR_state_encryption_passphrase; retain it outside this stack for recovery."
  type        = string
  sensitive   = true
  ephemeral   = true
  validation {
    condition     = length(var.state_encryption_passphrase) >= 32
    error_message = "State encryption requires a passphrase of at least 32 characters."
  }
}
terraform {
  encryption {
    key_provider "pbkdf2" "state" {
      passphrase = var.state_encryption_passphrase
    }
    method "aes_gcm" "state" {
      keys = key_provider.pbkdf2.state
    }
    state {
      method   = method.aes_gcm.state
      enforced = true
    }
    plan {
      method   = method.aes_gcm.state
      enforced = true
    }
  }
}
