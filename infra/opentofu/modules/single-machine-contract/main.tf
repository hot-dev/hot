terraform {
  required_version = ">= 1.13.1, < 1.14.0"
}

variable "installation" {
  description = "Non-secret installation contract. Secrets are cloud secret-store references only."
  type = object({
    schema_version = optional(number, 1)
    # single-machine runs every role on one VM; scalable runs each role on
    # its own group of VMs, on managed data and shared object storage.
    preset = optional(string, "single-machine")
    # Scalable only: instances per role. The scheduler always runs exactly one.
    # With a maximum above the minimum, workers also scale on the jobs
    # waiting per host, and task workers scale out on the share of their
    # slots in use; an idle task host leaves its group after
    # retire_idle_minutes (0 keeps every task host).
    scaling = optional(object({
      app = optional(object({ min = optional(number, 2), max = optional(number, 4) }), {})
      api = optional(object({ min = optional(number, 2), max = optional(number, 4) }), {})
      worker = optional(object({
        min              = optional(number, 2)
        max              = optional(number, 2)
        waiting_per_host = optional(number, 50)
      }), {})
      task_worker = optional(object({
        min                 = optional(number, 1)
        max                 = optional(number, 1)
        slots_target        = optional(number, 70)
        retire_idle_minutes = optional(number, 30)
      }), {})
    }), {})
    name       = string
    cloud      = string
    region     = string
    zone       = string
    project    = optional(string, "")
    size       = optional(string, "small")
    host_image = string
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
        # AWS only: raise to set a new AUTH token; see infra/README.md. GCP
        # authenticates with the host's IAM identity instead.
        auth_token_version = optional(number, 1)
      }), {})
    }), {})
    # Daily snapshots of the retained data disk, at an hour in UTC, kept for
    # retention_days. The host cannot delete them. restore_snapshot creates a
    # new data disk from a snapshot; it is read only when the disk is created.
    backup = optional(object({
      retention_days   = optional(number, 7)
      time_utc         = optional(string, "03:00")
      restore_snapshot = optional(string, "")
    }), {})
    # Where infrastructure alerts go, independent of Hot's own email; the
    # queue backlog, and the seconds the oldest waiting job or task has
    # waited, that alert; and an optional monthly budget (in the billing
    # currency) that emails at 80% spent and 100% forecast. GCP budgets need
    # the project's billing account.
    monitoring = optional(object({
      alert_emails        = optional(list(string), [])
      queue_backlog_alert = optional(number, 1000)
      queue_wait_alert    = optional(number, 900)
      monthly_budget      = optional(number, 0)
      billing_account     = optional(string, "")
    }), {})
    # A registry that mirrors Hot's images (registry host and path, such as
    # 123456789012.dkr.ecr.us-east-1.amazonaws.com/hot-mirror). Images keep
    # their digests. Empty pulls from the release's own registries.
    image_registry = optional(string, "")
    # cloud ships container logs to CloudWatch Logs or Cloud Logging; local
    # keeps them on the host only. retention_days applies to the AWS log group.
    logging = optional(object({
      destination    = optional(string, "cloud")
      retention_days = optional(number, 30)
    }), {})
    app_hostname = string
    api_hostname = string
    app_exposure = optional(string, "private")
    api_exposure = optional(string, "private")
    # Each role uses local disk, or S3 or GCS. With provision, the stack
    # creates the bucket (in the installation's own cloud and region) and
    # keeps noncurrent object versions for noncurrent_days; otherwise the
    # bucket already exists and is never managed.
    storage = object({
      build = object({
        backend         = string
        bucket          = optional(string, "")
        region          = optional(string, "")
        prefix          = optional(string, "builds")
        provision       = optional(bool, false)
        noncurrent_days = optional(number, 30)
      })
      file = object({
        backend         = string
        bucket          = optional(string, "")
        region          = optional(string, "")
        prefix          = optional(string, "files")
        provision       = optional(bool, false)
        noncurrent_days = optional(number, 30)
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
      # Existing AWS networks, scalable preset: private subnets in two or more
      # zones for the hosts.
      private_subnet_ids = optional(list(string), [])
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
    condition     = contains(["single-machine", "scalable"], var.installation.preset)
    error_message = "preset must be single-machine or scalable."
  }
  validation {
    condition = var.installation.monitoring.queue_backlog_alert >= 1 && var.installation.monitoring.queue_wait_alert >= 60 && var.installation.monitoring.monthly_budget >= 0 && (
      var.installation.monitoring.monthly_budget == 0 || var.installation.cloud == "aws"
    || can(regex("^[0-9A-F]{6}-[0-9A-F]{6}-[0-9A-F]{6}$", var.installation.monitoring.billing_account)))
    error_message = "queue_backlog_alert must be at least 1 and queue_wait_alert at least 60 seconds; a monthly_budget on GCP needs billing_account (XXXXXX-XXXXXX-XXXXXX)."
  }
  validation {
    condition = (var.installation.scaling.worker.waiting_per_host >= 1
      && var.installation.scaling.task_worker.slots_target >= 10 && var.installation.scaling.task_worker.slots_target <= 100
      && (var.installation.scaling.task_worker.retire_idle_minutes == 0
    || (var.installation.scaling.task_worker.retire_idle_minutes >= 5 && var.installation.scaling.task_worker.retire_idle_minutes <= 1440)))
    error_message = "scaling.worker.waiting_per_host must be at least 1, scaling.task_worker.slots_target 10-100, and retire_idle_minutes 0 or 5-1440."
  }
  validation {
    condition = var.installation.image_registry == "" || can(regex("^[a-z0-9.-]+(:[0-9]+)?(/[a-z0-9._-]+)*$", var.installation.image_registry)) && (
      !endswith(split("/", var.installation.image_registry)[0], "-docker.pkg.dev") || var.installation.cloud == "gcp" && length(split("/", var.installation.image_registry)) >= 3
    ) && (!endswith(split("/", var.installation.image_registry)[0], ".amazonaws.com") || var.installation.cloud == "aws")
    error_message = "image_registry must be a registry host and optional path, such as registry.example.com/hot-mirror. An ECR mirror needs cloud aws; an Artifact Registry mirror needs cloud gcp and names its project and repository (us-docker.pkg.dev/PROJECT/REPOSITORY)."
  }
  validation {
    condition = var.installation.preset != "scalable" || var.installation.cloud != "aws" || var.installation.network.mode == "new" || (
    length(var.installation.network.private_subnet_ids) >= 2 && length(var.installation.network.data_subnet_ids) >= 2)
    error_message = "A scalable installation on an existing AWS network needs private_subnet_ids and data_subnet_ids in two or more zones."
  }
  validation {
    condition = var.installation.preset != "scalable" || (
      var.installation.data.postgres.mode != "local" && var.installation.data.valkey.mode != "local"
      && var.installation.storage.build.backend != "local" && var.installation.storage.file.backend != "local"
    )
    error_message = "A scalable installation needs managed or existing PostgreSQL and Valkey, and S3 or GCS for builds and files."
  }
  validation {
    condition = alltrue([for role in values(var.installation.scaling) :
    role.min >= 1 && role.max >= role.min && role.max <= 20])
    error_message = "Each scaling role needs 1 <= min <= max <= 20."
  }
  validation {
    # Every instance's pool (20 connections) at the maximums, plus the
    # scheduler and room for administration, within the connections the
    # database tier of the size accepts.
    condition = var.installation.preset != "scalable" || (
      (var.installation.scaling.app.max + var.installation.scaling.api.max + var.installation.scaling.worker.max
      + var.installation.scaling.task_worker.max + 1) * 20 + 40
    <= lookup({ small = 400, medium = 800, large = 1600 }, var.installation.size, 400))
    error_message = "The scaling maximums exceed the database's connection budget for this size: lower them or choose a larger size."
  }
  validation {
    condition = alltrue([for s in [var.installation.storage.build, var.installation.storage.file] :
      contains(["local", "s3", "gcs"], s.backend)
      && (s.backend != "s3" || (s.bucket != "" && s.region != "" && trim(s.prefix, "/") != ""))
      && (s.backend != "gcs" || (s.bucket != "" && trim(s.prefix, "/") != "" && var.installation.cloud == "gcp"))
    ])
    error_message = "Each storage role selects local, S3 (bucket and region), or GCS (bucket; GCP compute only); cloud storage needs a nonempty relative prefix."
  }
  validation {
    condition     = alltrue([for s in [var.installation.storage.build, var.installation.storage.file] : !startswith(s.prefix, "/") && !strcontains(s.prefix, "..")])
    error_message = "Storage prefixes must be relative and must not contain '..'."
  }
  validation {
    condition = alltrue([for s in [var.installation.storage.build, var.installation.storage.file] : !s.provision || (
      (s.backend == "s3" && var.installation.cloud == "aws" && s.region == var.installation.region
      && can(regex("^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$", s.bucket)))
      || (s.backend == "gcs" && var.installation.cloud == "gcp" && can(regex("^[a-z0-9][a-z0-9_.-]{1,61}[a-z0-9]$", s.bucket)))
    ) && s.noncurrent_days >= 1 && s.noncurrent_days <= 3650])
    error_message = "A provisioned bucket is S3 in the installation's AWS region or GCS on GCP, with a valid bucket name and noncurrent_days between 1 and 3650."
  }
  validation {
    condition = (var.installation.storage.build.bucket != var.installation.storage.file.bucket || var.installation.storage.build.bucket == ""
      || (var.installation.storage.build.provision == var.installation.storage.file.provision
    && var.installation.storage.build.noncurrent_days == var.installation.storage.file.noncurrent_days))
    error_message = "Build and file storage sharing a bucket must agree on provision and noncurrent_days."
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
    condition     = var.installation.data.valkey.mode != "managed" || var.installation.data.valkey.auth_token_version >= 1
    error_message = "data.valkey.auth_token_version must be at least 1."
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
    condition     = var.installation.backup.retention_days >= 1 && var.installation.backup.retention_days <= 365 && can(regex("^([01][0-9]|2[0-3]):00$", var.installation.backup.time_utc))
    error_message = "backup.retention_days must be 1-365, and backup.time_utc an hour such as 03:00."
  }
  validation {
    condition = var.installation.backup.restore_snapshot == "" || (
      var.installation.cloud == "aws" ? can(regex("^snap-[0-9a-f]{8,17}$", var.installation.backup.restore_snapshot))
      : can(regex("^(projects/[a-z][-a-z0-9]{4,28}[a-z0-9]/global/snapshots/)?[a-z]([-a-z0-9]{0,61}[a-z0-9])?$", var.installation.backup.restore_snapshot))
    )
    error_message = "backup.restore_snapshot must be an EBS snapshot ID (snap-...) on AWS, or a snapshot name or projects/.../global/snapshots/NAME on GCP."
  }
  validation {
    condition     = contains(["cloud", "local"], var.installation.logging.destination) && contains([1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192, 2557, 2922, 3288, 3653], var.installation.logging.retention_days)
    error_message = "logging.destination must be cloud or local, and logging.retention_days a CloudWatch Logs retention such as 7, 30, 90, or 365."
  }
  validation {
    condition     = alltrue([for email in var.installation.monitoring.alert_emails : can(regex("^[^@[:space:]]+@[^@[:space:]]+\\.[^@[:space:]]+$", email))])
    error_message = "monitoring.alert_emails must be email addresses."
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
    small  = { aws = "db.m7g.large", gcp = "db-custom-2-8192", storage_gb = 50, valkey = "cache.m7g.large", valkey_gcp = "STANDARD_SMALL" }
    medium = { aws = "db.m7g.xlarge", gcp = "db-custom-4-16384", storage_gb = 100, valkey = "cache.m7g.xlarge", valkey_gcp = "HIGHMEM_MEDIUM" }
    large  = { aws = "db.m7g.2xlarge", gcp = "db-custom-8-32768", storage_gb = 200, valkey = "cache.m7g.2xlarge", valkey_gcp = "HIGHMEM_MEDIUM" }
  }
  postgres = var.installation.data.postgres
  capacity = local.sizes[var.installation.size]
  # Scalable roles other than task-workers need no nested virtualization, so
  # they use general-purpose machines; task-workers use the size's family.
  role_machines = {
    small  = { aws = "m7i.large", gcp = "e2-standard-2" }
    medium = { aws = "m7i.xlarge", gcp = "e2-standard-4" }
    large  = { aws = "m7i.2xlarge", gcp = "e2-standard-8" }
  }
  # Each scalable instance's PostgreSQL pool, and the connections the
  # database tier of each size accepts, leaving room for administration.
  db_pool_per_instance = 20
  scaling              = var.installation.scaling
}


output "configuration" {
  value = var.installation
}
output "capacity" {
  description = "Conservative starting allocations; not benchmarked throughput guarantees."
  value       = local.capacity
}
output "roles" {
  description = "Scalable only: each role's machine types and instance bounds, and the PostgreSQL pool each instance may open."
  value = var.installation.preset != "scalable" ? null : {
    db_pool = local.db_pool_per_instance
    groups = {
      app         = merge(local.role_machines[var.installation.size], local.scaling.app)
      api         = merge(local.role_machines[var.installation.size], local.scaling.api)
      worker      = merge(local.role_machines[var.installation.size], local.scaling.worker)
      scheduler   = merge(local.role_machines[var.installation.size], { min = 1, max = 1 })
      task-worker = merge({ aws = local.capacity.aws, gcp = local.capacity.gcp }, local.scaling.task_worker)
    }
  }
}
output "managed_valkey" {
  description = "The managed Valkey node type, with the size default applied; null unless data.valkey.mode is managed."
  value = var.installation.data.valkey.mode != "managed" ? null : {
    node_type = var.installation.data.valkey.node_type != "" ? var.installation.data.valkey.node_type : local.database[var.installation.size][var.installation.cloud == "aws" ? "valkey" : "valkey_gcp"]
  }
}
output "managed_postgres" {
  description = "The managed PostgreSQL tier and storage, with size defaults applied; null unless data.postgres.mode is managed."
  value = local.postgres.mode != "managed" ? null : {
    tier       = local.postgres.tier != "" ? local.postgres.tier : local.database[var.installation.size][var.installation.cloud]
    storage_gb = local.postgres.storage_gb > 0 ? local.postgres.storage_gb : local.database[var.installation.size].storage_gb
  }
}
locals {
  registry_host = split("/", var.installation.image_registry)[0]
  registry_path = trimprefix(trimprefix(var.installation.image_registry, local.registry_host), "/")
  ecr_mirror    = regex("^(?:([0-9]{12})\\.dkr\\.ecr\\.([a-z0-9-]+)\\.amazonaws\\.com)?", local.registry_host)
  gar_mirror    = regex("^(?:([a-z0-9-]+)-docker\\.pkg\\.dev)?", local.registry_host)
}
output "mirror" {
  description = "The image mirror the hosts sign in to with their own identity: kind is ecr or artifact-registry, or empty for other mirrors and none."
  value = {
    kind = local.ecr_mirror[0] != null ? "ecr" : local.gar_mirror[0] != null && length(split("/", local.registry_path)) >= 2 ? "artifact-registry" : ""
    # ECR: repositories under the mirror path, such as hot-mirror/app.
    account      = coalesce(local.ecr_mirror[0], "-")
    region       = coalesce(local.ecr_mirror[1], "-")
    repositories = local.registry_path == "" ? "*" : "${local.registry_path}/*"
    # Artifact Registry: LOCATION-docker.pkg.dev/PROJECT/REPOSITORY.
    location   = coalesce(local.gar_mirror[0], "-")
    project    = try(split("/", local.registry_path)[0], "-")
    repository = try(split("/", local.registry_path)[1], "-")
  }
}
