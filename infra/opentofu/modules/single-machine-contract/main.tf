terraform {
  required_version = ">= 1.13.1, < 1.14.0"
}

variable "installation" {
  description = "Non-secret installation contract. Secrets are cloud secret-store references only."
  type = object({
    schema_version = optional(number, 1)
    name           = string
    cloud          = string
    region         = string
    zone           = string
    project        = optional(string, "")
    size           = optional(string, "small")
    host_image     = string
    # The stacks create the release pointer, status, and secret containers and
    # fill these in; values given here are ignored.
    release_pointer   = optional(string, "")
    secret_references = optional(map(string), {})
    # The first administrator, created by the agent with a generated password.
    admin_email = optional(string, "")
    # Where release manifests are published; empty means Hot's GitHub releases.
    manifest_base_url = optional(string, "")
    # Non-secret HOT_* settings for every Hot service, such as HOT_EMAIL_PROVIDER.
    settings     = optional(map(string), {})
    app_hostname = string
    api_hostname = string
    app_exposure = optional(string, "private")
    api_exposure = optional(string, "private")
    storage = object({
      build = object({
        backend = string
        bucket  = optional(string, "")
        region  = optional(string, "")
        prefix  = optional(string, "builds")
      })
      file = object({
        backend = string
        bucket  = optional(string, "")
        region  = optional(string, "")
        prefix  = optional(string, "files")
      })
    })
    network = optional(object({
      mode         = optional(string, "new")
      cidr         = optional(string, "10.68.0.0/16")
      client_cidrs = optional(list(string), [])
      network_id   = optional(string, "")
      subnet_id    = optional(string, "")
    }), {})
  })
  validation {
    condition     = var.installation.schema_version == 1 && contains(["aws", "gcp"], var.installation.cloud)
    error_message = "schema_version must be 1 and cloud must be aws or gcp."
  }
  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{2,23}$", var.installation.name)) && contains(["small", "medium", "large"], var.installation.size)
    error_message = "name must be 3-24 lowercase letters/digits/hyphens, starting with a letter; size must be small, medium, or large."
  }
  validation {
    condition     = alltrue([for s in [var.installation.storage.build, var.installation.storage.file] : contains(["local", "s3"], s.backend) && (s.backend != "s3" || (s.bucket != "" && s.region != "" && trim(s.prefix, "/") != ""))])
    error_message = "Each storage role must select local or existing S3; S3 requires bucket and region. GCS/provisioned buckets are not implemented in this slice."
  }
  validation {
    condition     = alltrue([for s in [var.installation.storage.build, var.installation.storage.file] : !startswith(s.prefix, "/") && !strcontains(s.prefix, "..")])
    error_message = "Storage prefixes must be relative and must not contain '..'."
  }
  validation {
    condition     = var.installation.app_exposure == "private" && var.installation.api_exposure == "private"
    error_message = "This initial infrastructure slice implements private/private ingress only; public ingress is not implemented yet."
  }
  validation {
    condition     = var.installation.app_hostname != var.installation.api_hostname && alltrue([for h in [var.installation.app_hostname, var.installation.api_hostname] : can(regex("^[a-z0-9][a-z0-9.-]+[a-z0-9]$", h))])
    error_message = "App and API need distinct DNS hostnames, without schemes, ports, or paths."
  }
  validation {
    condition     = contains(["new", "existing"], var.installation.network.mode) && (var.installation.network.mode != "existing" || (var.installation.network.network_id != "" && var.installation.network.subnet_id != ""))
    error_message = "network.mode must be new or existing; existing requires network_id and subnet_id."
  }
  validation {
    condition     = can(cidrsubnet(var.installation.network.cidr, 8, 1)) && alltrue([for c in var.installation.network.client_cidrs : can(cidrnetmask(c)) && try(tonumber(split("/", c)[1]) > 0, false)])
    error_message = "The network needs a usable IPv4 CIDR; client ranges must be valid IPv4 CIDRs narrower than /0."
  }
  validation {
    condition     = var.installation.host_image != ""
    error_message = "A baked host image is required."
  }
  validation {
    condition     = var.installation.admin_email == "" || can(regex("^[^@[:space:]]+@[^@[:space:]]+\\.[^@[:space:]]+$", var.installation.admin_email))
    error_message = "admin_email must be an email address."
  }
  validation {
    condition     = var.installation.manifest_base_url == "" || startswith(var.installation.manifest_base_url, "https://")
    error_message = "manifest_base_url must use https."
  }
  validation {
    condition = alltrue([for key in keys(var.installation.settings) :
      can(regex("^HOT_[A-Z0-9_]+$", key)) && length(regexall("SECRET|PASSWORD|TOKEN|_KEY|_URI", key)) == 0
    ])
    error_message = "settings take non-secret HOT_* names; secrets belong in the operator secret bundle."
  }
  validation {
    condition     = var.installation.cloud != "gcp" || var.installation.project != ""
    error_message = "GCP compute requires project. Storage selection is independent."
  }
  validation {
    condition = alltrue([for ref in values(var.installation.secret_references) :
      var.installation.cloud == "aws" ?
      can(regex("^arn:[^:]+:secretsmanager:[^:]+:[0-9]{12}:secret:.+$", ref)) :
      can(regex("^projects/[^/]+/secrets/[^/]+$", ref))
    ])
    error_message = "secret_references must contain secret-store resource IDs for the compute cloud, never plaintext values."
  }
}

locals {
  sizes = {
    small  = { aws = "m8i.large", gcp = "n2-standard-2", memory_mb = 8192, cpus = 2, data_gb = 100 }
    medium = { aws = "m8i.xlarge", gcp = "n2-standard-4", memory_mb = 16384, cpus = 4, data_gb = 200 }
    large  = { aws = "m8i.2xlarge", gcp = "n2-standard-8", memory_mb = 32768, cpus = 8, data_gb = 500 }
  }
  capacity = local.sizes[var.installation.size]
}

output "configuration" {
  value = var.installation
}
output "capacity" {
  description = "Conservative starting allocations; not benchmarked throughput guarantees."
  value       = local.capacity
}
