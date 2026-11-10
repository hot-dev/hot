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
    # slots in use. On AWS an idle task host leaves its group after
    # retire_idle_minutes (0 keeps every task host). GCP ignores that setting:
    # its groups stay at their peak size until an operator shrinks them.
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
    # service; existing uses the operator's, with connection settings in the
    # operator secret. endpoint.deny_cidrs (managed and existing) lists networks
    # where the service's standbys and replicas can be, which user code is denied
    # like its current address, since that address can move; the stacks add the
    # data subnets they know.
    data = optional(object({
      postgres = optional(object({
        mode = optional(string, "local")
        # Managed and existing only: a local database uses hot_runtime.
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
        endpoint               = optional(object({ deny_cidrs = optional(list(string), []) }), {})
      }), {})
      valkey = optional(object({
        mode = optional(string, "local")
        # Managed only. An empty node_type follows the size.
        node_type         = optional(string, "")
        high_availability = optional(bool, false)
        # AWS only: raise to give the cache a new AUTH token. The cache keeps
        # the token before it until the next raise, so a leaked token stops
        # working only after a second one; see infra/README.md. GCP
        # authenticates with the host's IAM identity instead.
        auth_token_version = optional(number, 1)
        endpoint           = optional(object({ deny_cidrs = optional(list(string), []) }), {})
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
      # An event type a deployed Hot handler answers. Hosts serving the API
      # publish it every 5 minutes with the operator secret's
      # HOT_SYNTHETIC_API_KEY and alert when its run does not succeed.
      synthetic_event = optional(string, "")
      monthly_budget  = optional(number, 0)
      billing_account = optional(string, "")
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
    # Cloud resource names built from it (hot-NAME) may not end in a hyphen.
    condition     = can(regex("^[a-z][a-z0-9-]{1,22}[a-z0-9]$", var.installation.name)) && contains(["small", "medium", "large"], var.installation.size)
    error_message = "name must be 3-24 lowercase letters/digits/hyphens, starting with a letter and ending with a letter or digit; size must be small, medium, or large."
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
    condition = var.installation.monitoring.synthetic_event == "" || (
      can(regex("^[A-Za-z0-9][A-Za-z0-9:._-]{0,127}$", var.installation.monitoring.synthetic_event))
    && !startswith(var.installation.monitoring.synthetic_event, "hot:"))
    error_message = "monitoring.synthetic_event must be an event type your project handles (letters, digits, :._-), not a reserved hot: type."
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
    # database tier of the size accepts. The GCP stacks set Cloud SQL's
    # max_connections from this budget, as its memory-based default is lower;
    # the scalable one adds the pools of a rolling update's extra hosts.
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
    # Buckets and prefixes go into the host's IAM resource patterns and
    # conditions, where a wildcard or a quote would widen its grant. The renderer
    # refuses any other S3 region, so it stops here rather than on the host.
    condition = alltrue([for s in [var.installation.storage.build, var.installation.storage.file] : s.backend == "local" || (
      can(regex("^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*/?$", s.prefix))
      && (s.backend != "s3" || can(regex("^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$", s.bucket)))
      && (s.backend != "s3" || can(regex("^[a-z0-9-]+$", s.region)))
      && (s.backend != "gcs" || can(regex("^[a-z0-9][a-z0-9._-]{1,61}[a-z0-9]$", s.bucket)))
    )])
    error_message = "S3 and GCS storage need a valid bucket name and a prefix of letters, digits, '.', '_', '-', and '/' separators; an S3 region takes lowercase letters, digits, and '-'."
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
    # The renderer parses these strictly: IPv4 network addresses, without
    # host bits.
    condition = (can(cidrnetmask(var.installation.network.cidr))
      && try(cidrsubnet(var.installation.network.cidr, 0, 0) == var.installation.network.cidr && tonumber(split("/", var.installation.network.cidr)[1]) <= 24, false)
      && alltrue([for c in var.installation.network.client_cidrs :
    can(cidrnetmask(c)) && try(cidrsubnet(c, 0, 0) == c && tonumber(split("/", c)[1]) > 0, false)]))
    error_message = "network.cidr must be an IPv4 network of /24 or larger, and client ranges IPv4 networks narrower than /0, each without host bits (10.68.0.0/16, not 10.68.0.1/16)."
  }
  validation {
    # A new AWS network is a VPC, at most /16, carved into subnets 8 bits
    # longer: AWS allows subnets down to /28, and a load balancer's to /27.
    condition = var.installation.cloud != "aws" || var.installation.network.mode != "new" || try(
      tonumber(split("/", var.installation.network.cidr)[1]) >= 16 && tonumber(split("/", var.installation.network.cidr)[1]) <= (
        var.installation.preset == "single-machine" && contains([var.installation.app_exposure, var.installation.api_exposure], "public") ? 19 : 20
    ), false)
    error_message = "A new AWS network needs a /16 to /20 network.cidr, or /16 to /19 with a public endpoint."
  }
  validation {
    # A scalable installation's hosts share two private subnets, each 8 bits
    # longer than the new network, less the 5 addresses AWS keeps. Every
    # group spreads its hosts over both, and a refresh for a new host image
    # runs each at up to twice its size, so one subnet holds every role's
    # maximum, the scheduler, and an interface of each internal load balancer.
    # A range the rule above refuses is left to it.
    condition = var.installation.preset != "scalable" || var.installation.cloud != "aws" || var.installation.network.mode != "new" || try(
      (var.installation.scaling.app.max + var.installation.scaling.api.max + var.installation.scaling.worker.max
        + var.installation.scaling.task_worker.max + 1
      + length([for exposure in [var.installation.app_exposure, var.installation.api_exposure] : exposure if exposure != "public"]))
    <= pow(2, 24 - tonumber(split("/", var.installation.network.cidr)[1])) - 5, true)
    error_message = "The scaling maximums need more addresses than a private subnet of the new AWS network holds: the sum of the four maximums, plus 1 for the scheduler and 1 for each private endpoint, may be at most 11 on a /20 network.cidr, 27 on a /19, 59 on a /18, 123 on a /17, and 251 on a /16. Lower the maximums, or choose a larger network.cidr before the network exists."
  }
  validation {
    condition     = contains(["local", "managed", "existing"], var.installation.data.postgres.mode) && contains(["local", "managed", "existing"], var.installation.data.valkey.mode) && can(regex("^[a-z_][a-z0-9_]{0,62}$", var.installation.data.postgres.runtime_role)) && !contains(["hot_owner", "hot_admin", "postgres"], var.installation.data.postgres.runtime_role)
    error_message = "data.postgres.mode and data.valkey.mode must be local, managed, or existing; runtime_role a PostgreSQL role name other than hot_owner, hot_admin, or postgres."
  }
  validation {
    # The local database creates hot_runtime once, when it is initialized, and
    # the services log in as it. The renderer refuses any other role.
    condition     = var.installation.data.postgres.mode != "local" || var.installation.data.postgres.runtime_role == "hot_runtime"
    error_message = "data.postgres.runtime_role applies to managed and existing databases; a local database uses hot_runtime."
  }
  validation {
    # Networks, as written: an address with no prefix, host bits inside one, or a
    # /0 is more likely a mistake than a choice, and HOT_NETWORK_DENY_CIDRS takes
    # those. cidrsubnet gives back the canonical form, which a leading zero or an
    # overlong IPv6 group doesn't match, and reads an IPv4-mapped range as IPv4,
    # which doesn't match either. Hot and the agent also read ranges inside ::/96
    # and 64:ff9b::/96 as IPv4 (outbound::canonical_net), which makes ::/96 a
    # 0.0.0.0/0 that cuts all IPv4 egress, so IPv4 has to be written as IPv4.
    condition = alltrue([for cidr in concat(var.installation.data.postgres.endpoint.deny_cidrs, var.installation.data.valkey.endpoint.deny_cidrs) :
      can(regex("^[0-9A-Fa-f:.]+/[1-9][0-9]{0,2}$", cidr)) && try(cidrsubnet(cidr, 0, 0) == lower(cidr), false) &&
      !can(regex("^(::|64:ff9b::)([0-9a-f]{1,4}(:[0-9a-f]{1,4})?)?/(9[6-9]|1[0-2][0-9])$", lower(cidr)))
    ])
    error_message = "data.postgres.endpoint.deny_cidrs and data.valkey.endpoint.deny_cidrs must be IPv4 or IPv6 networks narrower than /0, without host bits, with IPv4 written as IPv4 (10.68.4.0/24, not 10.68.4.1/24, 10.68.4.1 or ::a44:400/120)."
  }
  validation {
    # A local service is on the bundle's own network, which user code is always
    # denied, and its endpoint settings reach nothing.
    condition = alltrue([for service in [var.installation.data.postgres, var.installation.data.valkey] :
      service.mode != "local" || length(service.endpoint.deny_cidrs) == 0
    ])
    error_message = "data.postgres.endpoint.deny_cidrs and data.valkey.endpoint.deny_cidrs apply to managed and existing services, not local ones."
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
    # A null stays in the map and reaches the host, and the renderer refuses every
    # release until another apply removes it. This comes before the rules that read
    # values, whose functions can't take one.
    condition     = alltrue([for value in values(var.installation.settings) : value != null])
    error_message = "settings values must be strings; to clear a setting, remove its entry."
  }
  validation {
    # The runtime bundle sets these itself or hard-codes what they would change,
    # and the renderer refuses them (infra/runtime/render.py). A section's own name
    # (HOT_APP, HOT_API, HOT_DB) takes JSON that replaces the whole section. A
    # NAME_FILE names a file in the container, and only the bundle mounts those.
    condition = alltrue([for key in keys(var.installation.settings) : !contains([
      "HOT_ENV", "HOT_PRODUCT_EXPERIENCE", "HOT_ENGINE_ISOLATION", "HOT_NETWORK_ALLOW_PRIVATE", "HOT_QUEUE_TYPE",
      "HOT_EMITTER_TYPE", "HOT_FILE_MODE", "HOT_BLOB_MODE", "HOT_LOG_FORMAT", "HOT_LOG_TARGET", "HOT_LOG_LEVEL",
      "HOT_APP_HOST", "HOT_API_HOST", "HOT_APP_URL", "HOT_API_URL", "HOT_OAUTH_REDIRECT_BASE_URL", "HOT_COOKIE_DOMAIN",
      "HOT_TASK_MAX_CONCURRENT", "HOT_TASK_CODE_MAX_CONCURRENT", "HOT_TASK_CONTAINER_MAX_CONCURRENT",
      "HOT_TASK_WORKER_MEMORY_MB", "HOT_TASK_CONTAINER_RESERVED_MEMORY_MB", "HOT_TASK_CODE_MEMORY_PERCENT",
      "HOT_TASK_WORKER_DISK_MB", "HOT_TASK_DATA_VOLUME_DIR", "HOT_BOX_ENABLED", "HOT_WORKER_THREADS",
      "HOT_ENGINE_THREADS", "HOT_BOX_BACKEND", "HOT_BOX_CONTAINERD_SOCKET", "HOT_BOX_VMM", "HOT_STORE_MODELS_PATH",
      "HOT_API_KNOWN_HOSTS", "HOT_APP", "HOT_APP_PORT", "HOT_API", "HOT_API_PORT", "HOT_DB", "HOT_DB_SCHEMA",
      "HOT_DB_RLS_ENFORCE", "HOT_RLS_ENFORCE", "HOT_STORE_TYPE",
      ], key) && !endswith(key, "_STORAGE_TYPE") && !endswith(key, "_STORAGE_PATH") && !endswith(key, "_FILE")
      && !strcontains(key, "_AWS_S3_") && !strcontains(key, "_GCS_") && !startswith(key, "HOT_NETWORK_CLIENT_IP_")
      && (var.installation.preset != "scalable" || key != "HOT_WORKER_DB_MAX_CONNECTIONS")
    ])
    error_message = "settings cannot change what the runtime bundle sets: isolation, private networking, queue and log options, hosts, URLs, and ports, the database and its schema, row-level security, the store type, task and box sizing, storage, file names (NAME_FILE), and client-address handling."
  }
  validation {
    # On managed Valkey on GCP the bundle sets how Hot authenticates, and mounts
    # the instance's CA as HOT_REDIS_CA_CERT_FILE. Hot stops when a NAME and its
    # NAME_FILE are both set.
    condition = (var.installation.cloud != "gcp" || var.installation.data.valkey.mode != "managed"
    || length(setintersection(keys(var.installation.settings), ["HOT_REDIS_AUTH", "HOT_REDIS_CA_CERT"])) == 0)
    error_message = "settings HOT_REDIS_AUTH and HOT_REDIS_CA_CERT are set by the runtime bundle for managed Valkey on GCP."
  }
  validation {
    # Comma-separated IPv4 or IPv6 addresses and networks, as the renderer and
    # Hot parse them. A bare address gets the prefix that makes it a network.
    # cidrhost also takes a leading zero in an IPv4 part or a prefix, and an IPv6
    # group longer than four digits, which they refuse; IPv6 groups such as
    # ::0001 are valid. No valid entry has five hex digits in a row.
    condition = alltrue([for entry in [for e in split(",", lookup(var.installation.settings, "HOT_NETWORK_DENY_CIDRS", "")) : trimspace(e) if trimspace(e) != ""] :
      can(cidrhost(strcontains(entry, "/") ? entry : "${entry}/${strcontains(entry, ":") ? 128 : 32}", 0))
      && can(regex("^[0-9A-Fa-f:.]+(/(0|[1-9][0-9]{0,2}))?$", entry))
      && (!strcontains(entry, ".") || !can(regex("(^|[.:])0[0-9]+([.]|/|$)", entry)))
      && !can(regex("[0-9A-Fa-f]{5}", entry))
    ])
    error_message = "settings HOT_NETWORK_DENY_CIDRS entries must be IP addresses or CIDRs."
  }
  validation {
    # The same forms as the deny list above. Hot won't start user code with an
    # entry it can't parse, and says so only in the worker's log.
    condition = alltrue([for entry in [for e in split(",", lookup(var.installation.settings, "HOT_NETWORK_ALLOW_CIDRS", "")) : trimspace(e) if trimspace(e) != ""] :
      can(cidrhost(strcontains(entry, "/") ? entry : "${entry}/${strcontains(entry, ":") ? 128 : 32}", 0))
      && can(regex("^[0-9A-Fa-f:.]+(/(0|[1-9][0-9]{0,2}))?$", entry))
      && (!strcontains(entry, ".") || !can(regex("(^|[.:])0[0-9]+([.]|/|$)", entry)))
      && !can(regex("[0-9A-Fa-f]{5}", entry))
    ])
    error_message = "settings HOT_NETWORK_ALLOW_CIDRS entries must be IP addresses or CIDRs."
  }
  validation {
    # Settings end up in tfvars and the installation parameter, so the renderer
    # refuses a proxy's credentials; refuse them here before they get there.
    condition     = !strcontains(lookup(var.installation.settings, "HOT_NETWORK_EGRESS_PROXY", ""), "@")
    error_message = "settings HOT_NETWORK_EGRESS_PROXY carries credentials; put it in the operator secret bundle."
  }
  validation {
    # What the renderer lets through (is_egress_proxy): http or https, a host, and
    # at most a port. Hot's workers exit on what its URL parser refuses, and that
    # parser reads a name ending in a number as an IPv4 address. An IPv6 address
    # has the same limits as the deny list below. Credentials have their message
    # above.
    condition = alltrue([for proxy in [trimspace(lookup(var.installation.settings, "HOT_NETWORK_EGRESS_PROXY", ""))] :
      proxy == "" || strcontains(proxy, "@") || (
        can(regex("^https?://(\\[[0-9A-Fa-f:.]+\\]|[A-Za-z0-9_-]+([.][A-Za-z0-9_-]+)*[.]?)(:(6553[0-5]|655[0-2][0-9]|65[0-4][0-9]{2}|6[0-4][0-9]{3}|[1-5]?[0-9]{1,4}))?/?$", proxy))
        && (!can(regex("^https?://([A-Za-z0-9_-]+[.])*([0-9]+|0[Xx][0-9A-Fa-f]*)[.]?(:[0-9]+)?/?$", proxy))
        || can(regex("^https?://((25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])[.]){3}(25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])(:[0-9]+)?/?$", proxy)))
        && alltrue([for address in regexall("^https?://\\[([^]]+)\\]", proxy) :
          strcontains(address[0], ":") && can(cidrhost("${address[0]}/128", 0)) && !can(regex("[0-9A-Fa-f]{5}", address[0]))
          && (!strcontains(address[0], ".") || !can(regex("[.:]0[0-9]+([.]|$)", address[0])))
        ])
      )
    ])
    error_message = "settings HOT_NETWORK_EGRESS_PROXY must be an http:// or https:// address such as http://proxy.internal:3128, without a path."
  }
  validation {
    # Exact names, or *.example.internal for any subdomain. Hot won't start user
    # code on a scheme, a port, or a wildcard elsewhere, and an entry with a
    # trailing dot or other characters never matches a request's host.
    condition = alltrue([for entry in [for e in split(",", lookup(var.installation.settings, "HOT_NETWORK_ALLOW_HOSTS", "")) : trimspace(e) if trimspace(e) != ""] :
      can(regex("^([*][.])?[A-Za-z0-9_-]+([.][A-Za-z0-9_-]+)*$", entry))
    ])
    error_message = "settings HOT_NETWORK_ALLOW_HOSTS entries must be hostnames, or *.example.internal for any subdomain, without schemes, ports, or paths."
  }
  validation {
    # The agent and Hot refuse anything but IPv4 addresses outside the ranges box
    # guests can never reach: loopback, link-local, multicast, reserved, and the
    # guest network. They parse strictly, so a leading zero is refused too.
    condition = alltrue([for entry in [for e in split(",", lookup(var.installation.settings, "HOT_BOX_GUEST_DNS", "")) : trimspace(e) if trimspace(e) != ""] :
      can(regex("^((25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])[.]){3}(25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])$", entry))
      && !can(regex("^(0|127|22[4-9]|2[3-5][0-9])[.]|^169[.]254[.]|^10[.]88[.]", entry))
    ])
    error_message = "settings HOT_BOX_GUEST_DNS entries must be IPv4 addresses that box guests can reach, not loopback, link-local, multicast, reserved, or in the guest network (10.88.0.0/16)."
  }
  validation {
    # The app won't start on anything else.
    condition     = contains(["open", "invite"], lookup(var.installation.settings, "HOT_APP_SIGNUP", "invite"))
    error_message = "settings HOT_APP_SIGNUP must be open or invite."
  }
  validation {
    # Hot refuses any other value. gcp-iam presents the VM's Google token, and Hot
    # refuses it without TLS, which a local Valkey does not use.
    condition = contains(["password", "gcp-iam"], lookup(var.installation.settings, "HOT_REDIS_AUTH", "password")) && (
      lookup(var.installation.settings, "HOT_REDIS_AUTH", "password") != "gcp-iam"
    || (var.installation.cloud == "gcp" && var.installation.data.valkey.mode != "local"))
    error_message = "settings HOT_REDIS_AUTH must be password or gcp-iam; gcp-iam needs cloud gcp and a Valkey that is not local."
  }
  validation {
    # Hot exits on these together: gcp-iam and a CA support standalone servers
    # only, and the bundle sets both for managed Valkey on GCP.
    condition = !contains(["1", "true", "on", "yes"], lower(trimspace(lookup(var.installation.settings, "HOT_REDIS_CLUSTER", "")))) || (
      lookup(var.installation.settings, "HOT_REDIS_AUTH", "") != "gcp-iam" && trimspace(lookup(var.installation.settings, "HOT_REDIS_CA_CERT", "")) == ""
    && (var.installation.cloud != "gcp" || var.installation.data.valkey.mode != "managed"))
    error_message = "settings HOT_REDIS_CLUSTER cannot be on with gcp-iam, a CA certificate, or managed Valkey on GCP, which support standalone servers only."
  }
  validation {
    # Hot stops on anything but PEM certificates. An unset value passes as one.
    condition     = strcontains(lookup(var.installation.settings, "HOT_REDIS_CA_CERT", "-----BEGIN CERTIFICATE-----"), "-----BEGIN CERTIFICATE-----")
    error_message = "settings HOT_REDIS_CA_CERT must hold PEM certificates."
  }
  validation {
    # The agent refuses a release's render on this when email goes through SMTP
    # (check_smtp), and on the next rule. The operator secret bundle can hold the
    # certificate or the username instead, and only the agent reads that copy.
    condition = alltrue([for email in [{ for key, value in var.installation.settings : key => trimspace(value) if startswith(key, "HOT_EMAIL_") }] :
      !contains([lower(lookup(email, "HOT_EMAIL_PROVIDER", "")), lower(lookup(email, "HOT_EMAIL_ALERTS_PROVIDER", ""))], "smtp")
      || lookup(email, "HOT_EMAIL_SMTP_CA_CERT", "") == ""
      || strcontains(lookup(email, "HOT_EMAIL_SMTP_CA_CERT", ""), "-----BEGIN CERTIFICATE-----")
    ])
    error_message = "settings HOT_EMAIL_SMTP_CA_CERT must hold PEM certificates."
  }
  validation {
    # A username is refused with or without its password in the bundle: with it,
    # none would send the password in plaintext; without it, the pair is incomplete.
    condition = alltrue([for email in [{ for key, value in var.installation.settings : key => trimspace(value) if startswith(key, "HOT_EMAIL_") }] :
      !contains([lower(lookup(email, "HOT_EMAIL_PROVIDER", "")), lower(lookup(email, "HOT_EMAIL_ALERTS_PROVIDER", ""))], "smtp")
      || lower(lookup(email, "HOT_EMAIL_SMTP_SECURITY", "")) != "none"
      || (lookup(email, "HOT_EMAIL_SMTP_CA_CERT", "") == "" && lookup(email, "HOT_EMAIL_SMTP_USERNAME", "") == "")
    ])
    error_message = "settings HOT_EMAIL_SMTP_SECURITY none sends without TLS, so it cannot go with HOT_EMAIL_SMTP_CA_CERT or HOT_EMAIL_SMTP_USERNAME; use starttls or tls, or remove them."
  }
  validation {
    # The agent refuses anything else, but only once a storage move starts.
    condition = alltrue([for entry in [for e in split(",", lookup(var.installation.settings, "HOT_MOVE_STORAGE_SKIP", "")) : trimspace(e) if trimspace(e) != ""] :
      can(regex("^[0-9a-fA-F]{8}(-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}$", entry))
    ])
    error_message = "settings HOT_MOVE_STORAGE_SKIP must list object ids separated by commas."
  }
  validation {
    # How many failed objects to leave behind, which must match what the last move
    # reported. The agent refuses anything else, but only once a storage move starts.
    condition     = can(regex("^[1-9][0-9]*$", lookup(var.installation.settings, "HOT_MOVE_STORAGE_SKIP_FAILED", "1")))
    error_message = "settings HOT_MOVE_STORAGE_SKIP_FAILED must be a whole number of 1 or more, the count of failed objects the last storage move reported; to clear it, remove its entry."
  }
  validation {
    # Hot keeps calls forever on anything else, so the renderer refuses it.
    # try is there because && still evaluates tonumber when the regex fails.
    condition = alltrue([for days in [lookup(var.installation.settings, "HOT_DB_RETENTION_CALL_DAYS", "-1")] :
      days == "-1" || (can(regex("^[1-9][0-9]{0,4}$", days)) && try(tonumber(days) <= 36500, false))
    ])
    error_message = "settings HOT_DB_RETENTION_CALL_DAYS must be -1, to keep calls forever, or a whole number of days from 1 to 36500."
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
  # database tier of each size accepts, leaving room for administration. The
  # GCP stacks set Cloud SQL's max_connections from that budget.
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
output "database_connections" {
  description = "The PostgreSQL connections the size's database tier is planned for, which the scalable maximums must fit. The GCP stacks set Cloud SQL's max_connections from it; the scalable one adds a rolling update's extra hosts."
  value       = lookup({ small = 400, medium = 800, large = 1600 }, var.installation.size, 400)
}
output "dashboard" {
  description = "Each stack's dashboard panels: a title, the statistic that reads the panel's agent metrics, and each metric's CloudWatch name and Cloud Monitoring descriptor key."
  value = [for panel in [
    { title = "Unhealthy services", stat = "Maximum", metrics = [["UnhealthyServices", "unhealthy_services"]], single = false },
    { title = "Ready, dependencies, scheduler (1 = yes)", stat = "Minimum", metrics = [["Ready", "ready"], ["DependenciesReachable", "dependencies_reachable"], ["SchedulerRunning", "scheduler_running"]], single = false },
    { title = "Queue backlog and dead letters", stat = "Maximum", metrics = [["QueueBacklog", "queue_backlog"], ["QueueDeadLetters", "queue_dead_letters"]], single = false },
    { title = "Jobs and tasks waiting", stat = "Maximum", metrics = [["JobsWaiting", "jobs_waiting"], ["TasksWaiting", "tasks_waiting"]], single = false },
    { title = "Oldest wait (seconds)", stat = "Maximum", metrics = [["JobWaitSeconds", "job_wait_seconds"], ["TaskWaitSeconds", "task_wait_seconds"]], single = false },
    { title = "Task slots used (%)", stat = "Average", metrics = [["TaskSlotsUsedPercent", "task_slots_used_percent"]], single = false },
    { title = "Synthetic check (1 = passed)", stat = "Minimum", metrics = [["SyntheticOk", "synthetic_ok"]], single = false },
    { title = "Disk used (%)", stat = "Maximum", metrics = [["DataDiskUsedPercent", "data_disk_used_percent"]], single = false },
    { title = "Memory used (%)", stat = "Maximum", metrics = [["MemoryUsedPercent", "memory_used_percent"]], single = false },
    { title = "Certificate days left", stat = "Minimum", metrics = [["CertificateDaysLeft", "certificate_days_left"]], single = false },
    { title = "Backup age (hours)", stat = "Maximum", metrics = [["BackupAgeHours", "backup_age_hours"]], single = true },
    ] : { title = panel.title, stat = panel.stat, metrics = [for m in panel.metrics : { aws = m[0], gcp = m[1] }] }
  if !panel.single || var.installation.preset != "scalable"]
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
