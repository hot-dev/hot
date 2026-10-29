variable "installation" {
  description = "See the shared single-machine-contract module. No plaintext secrets."
  type        = any
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
  description = "IAM role ARN that releases this installation, such as the state bootstrap's release role. Empty grants nothing."
  type        = string
  default     = ""
  validation {
    condition     = var.release_principal == "" || can(regex("^arn:aws[a-z-]*:iam::[0-9]{12}:role/.+$", var.release_principal))
    error_message = "release_principal must be an IAM role ARN."
  }
}
