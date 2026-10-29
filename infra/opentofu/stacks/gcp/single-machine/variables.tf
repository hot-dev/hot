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
