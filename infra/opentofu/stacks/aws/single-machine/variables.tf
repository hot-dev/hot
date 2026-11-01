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
variable "public_certificate_arn" {
  description = "ACM certificate covering every public hostname, DNS-validated by the operator. Required when an endpoint is public."
  type        = string
  default     = ""
  validation {
    condition     = var.public_certificate_arn == "" || can(regex("^arn:aws[a-z-]*:acm:[a-z0-9-]+:[0-9]{12}:certificate/.+$", var.public_certificate_arn))
    error_message = "public_certificate_arn must be an ACM certificate ARN."
  }
}
