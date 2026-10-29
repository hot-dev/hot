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
    settings = optional(map(string), {})
    # local runs PostgreSQL or Valkey on the VM; managed provisions the cloud
    # service (Valkey on AWS only so far); existing uses the operator's, with
    # connection settings in the operator secret.
    data = optional(object({
      postgres = optional(object({
        mode         = optional(string, "local")
        runtime_role = optional(string, "hot_runtime")
        # Managed only. Empty tier and zero storage_gb follow the size.
        version               = optional(string, "18")
        tier                  = optional(string, "")
        storage_gb            = optional(number, 0)
        high_availability     = optional(bool, false)
        backup_retention_days = optional(number, 7)
        deletion_protection   = optional(bool, true)
        # Raise to set a new administrator password (GCP); see infra/README.md.
        admin_password_version = optional(number, 1)
      }), {})
      valkey = optional(object({
        mode = optional(string, "local")
        # Managed only. An empty node_type follows the size.
        node_type         = optional(string, "")
        high_availability = optional(bool, false)
        # Raise to set a new AUTH token; see infra/README.md.
        auth_token_version = optional(number, 1)
      }), {})
    }), {})
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
      # Existing AWS networks with a public endpoint: two or more public
      # subnets in different zones for the load balancer.
      public_subnet_ids = optional(list(string), [])
      # Existing AWS networks with managed PostgreSQL or Valkey: private
      # subnets in two or more zones for them.
      data_subnet_ids = optional(list(string), [])
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
    condition = alltrue([for s in [var.installation.storage.build, var.installation.storage.file] :
      contains(["local", "s3", "gcs"], s.backend)
      && (s.backend != "s3" || (s.bucket != "" && s.region != "" && trim(s.prefix, "/") != ""))
      && (s.backend != "gcs" || (s.bucket != "" && trim(s.prefix, "/") != "" && var.installation.cloud == "gcp"))
    ])
    error_message = "Each storage role selects local, existing S3 (bucket and region), or existing GCS (bucket; GCP compute only); cloud storage needs a nonempty relative prefix."
  }
  validation {
    condition     = alltrue([for s in [var.installation.storage.build, var.installation.storage.file] : !startswith(s.prefix, "/") && !strcontains(s.prefix, "..")])
    error_message = "Storage prefixes must be relative and must not contain '..'."
  }
  validation {
    condition     = alltrue([for e in [var.installation.app_exposure, var.installation.api_exposure] : contains(["private", "public"], e)])
    error_message = "app_exposure and api_exposure are each private or public."
  }
  validation {
    condition     = (var.installation.app_exposure == "private" && var.installation.api_exposure == "private") || var.installation.cloud != "aws" || var.installation.network.mode == "new" || length(var.installation.network.public_subnet_ids) >= 2
    error_message = "A public endpoint on an existing AWS network needs network.public_subnet_ids in at least two zones."
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
    condition     = contains(["local", "managed", "existing"], var.installation.data.postgres.mode) && contains(["local", "managed", "existing"], var.installation.data.valkey.mode) && can(regex("^[a-z_][a-z0-9_]{0,62}$", var.installation.data.postgres.runtime_role)) && !contains(["hot_owner", "hot_admin", "postgres"], var.installation.data.postgres.runtime_role)
    error_message = "data.postgres.mode and data.valkey.mode must be local, managed, or existing; runtime_role a PostgreSQL role name other than hot_owner, hot_admin, or postgres."
  }
  validation {
    condition     = var.installation.data.valkey.mode != "managed" || (var.installation.cloud == "aws" && var.installation.data.valkey.auth_token_version >= 1)
    error_message = "Managed Valkey is available on AWS only: Memorystore for Valkey accepts only IAM tokens, which Hot's Valkey client does not request yet. Use local or existing Valkey on GCP."
  }
  validation {
    condition = var.installation.data.postgres.mode != "managed" || (
      contains(["18"], var.installation.data.postgres.version)
      && var.installation.data.postgres.backup_retention_days >= 1 && var.installation.data.postgres.backup_retention_days <= 35
      && (var.installation.data.postgres.storage_gb == 0 || var.installation.data.postgres.storage_gb >= 20)
      && var.installation.data.postgres.admin_password_version >= 1
    )
    error_message = "Managed PostgreSQL supports version 18, 1-35 days of backups, and 20 GB or more of storage."
  }
  validation {
    condition     = (var.installation.data.postgres.mode != "managed" && var.installation.data.valkey.mode != "managed") || var.installation.cloud != "aws" || var.installation.network.mode == "new" || length(var.installation.network.data_subnet_ids) >= 2
    error_message = "Managed PostgreSQL or Valkey on an existing AWS network needs network.data_subnet_ids in at least two zones."
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
  # Managed PostgreSQL and Valkey starting points per size.
  database = {
    small  = { aws = "db.m7g.large", gcp = "db-custom-2-8192", storage_gb = 50, valkey = "cache.m7g.large" }
    medium = { aws = "db.m7g.xlarge", gcp = "db-custom-4-16384", storage_gb = 100, valkey = "cache.m7g.xlarge" }
    large  = { aws = "db.m7g.2xlarge", gcp = "db-custom-8-32768", storage_gb = 200, valkey = "cache.m7g.2xlarge" }
  }
  postgres = var.installation.data.postgres
  capacity = local.sizes[var.installation.size]
}

output "configuration" {
  value = var.installation
}
output "capacity" {
  description = "Conservative starting allocations; not benchmarked throughput guarantees."
  value       = local.capacity
}
output "managed_valkey" {
  description = "The managed Valkey node type, with the size default applied; null unless data.valkey.mode is managed."
  value = var.installation.data.valkey.mode != "managed" ? null : {
    node_type = var.installation.data.valkey.node_type != "" ? var.installation.data.valkey.node_type : local.database[var.installation.size].valkey
  }
}
output "managed_postgres" {
  description = "The managed PostgreSQL tier and storage, with size defaults applied; null unless data.postgres.mode is managed."
  value = local.postgres.mode != "managed" ? null : {
    tier       = local.postgres.tier != "" ? local.postgres.tier : local.database[var.installation.size][var.installation.cloud]
    storage_gb = local.postgres.storage_gb > 0 ? local.postgres.storage_gb : local.database[var.installation.size].storage_gb
  }
}
