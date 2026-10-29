terraform {
  required_version = ">= 1.13.1, < 1.14.0"
  required_providers {
    google = { source = "hashicorp/google", version = "8.7.0" }
    random = { source = "hashicorp/random", version = "3.9.1" }
  }
}
provider "google" {
  project = var.installation.project
  region  = var.installation.region
  zone    = var.installation.zone
}
module "contract" {
  source       = "../../../modules/single-machine-contract"
  installation = var.installation
}
# The scalable preset: each Hot role on its own managed instance group of the
# host image, following its own release pointer. The scheduler's single
# instance also migrates the database, from the migration pointer, before any
# role rolls. App and API each sit behind a passthrough TCP load balancer
# (internal, or external when public), and TLS ends on the hosts' ingress
# with the operator's certificate, as on the single machine.
locals {
  config      = module.contract.configuration
  new_network = local.config.network.mode == "new"
  network_id  = local.new_network ? google_compute_network.hot[0].id : local.config.network.network_id
  subnet_id   = local.new_network ? google_compute_subnetwork.hot[0].id : local.config.network.subnet_id
  tag         = "hot-${local.config.name}"
  roles       = module.contract.roles.groups
  web_roles   = toset(["app", "api"])
  # Service account IDs are at most 30 characters.
  role_codes = { app = "app", api = "api", worker = "wrk", scheduler = "sch", task-worker = "tsk" }
  exposure   = { app = local.config.app_exposure, api = local.config.api_exposure }
  host_config = { for role in keys(local.roles) : role => merge(local.config, {
    roles                = [role]
    migrations           = role == "scheduler"
    data_disk            = "scratch"
    db_pool              = module.contract.roles.db_pool
    instance_group       = "${local.tag}-${role}"
    release_pointer      = google_secret_manager_secret.pointer[role].id
    migration_pointer    = role == "scheduler" ? google_secret_manager_secret.pointer["migrate"].id : ""
    status_reference     = "${local.tag}-status"
    command_subscription = google_pubsub_subscription.commands.id
    secret_references    = { generated = google_secret_manager_secret.generated.id, operator = google_secret_manager_secret.operator.id }
    data                 = local.host_data
  }) }
}
check "scalable_preset" {
  assert {
    condition     = local.config.preset == "scalable" && local.config.cloud == "gcp"
    error_message = "This stack needs installation.preset = scalable and installation.cloud = gcp."
  }
}
resource "google_project_service" "required" {
  for_each           = toset(["compute.googleapis.com", "secretmanager.googleapis.com", "logging.googleapis.com", "monitoring.googleapis.com", "pubsub.googleapis.com"])
  project            = local.config.project
  service            = each.value
  disable_on_destroy = false
}
resource "google_compute_network" "hot" {
  count                   = local.new_network ? 1 : 0
  name                    = local.tag
  auto_create_subnetworks = false
  routing_mode            = "REGIONAL"
  depends_on              = [google_project_service.required]
}
resource "google_compute_subnetwork" "hot" {
  count                    = local.new_network ? 1 : 0
  name                     = local.tag
  network                  = local.network_id
  region                   = local.config.region
  ip_cidr_range            = local.config.network.cidr
  private_ip_google_access = true
}
resource "google_compute_router" "egress" {
  count   = local.new_network ? 1 : 0
  name    = local.tag
  network = local.network_id
  region  = local.config.region
}
resource "google_compute_address" "egress" {
  count  = local.new_network ? 1 : 0
  name   = "${local.tag}-egress"
  region = local.config.region
}
resource "google_compute_router_nat" "egress" {
  count                              = local.new_network ? 1 : 0
  name                               = local.tag
  router                             = google_compute_router.egress[0].name
  region                             = local.config.region
  nat_ip_allocate_option             = "MANUAL_ONLY"
  nat_ips                            = [google_compute_address.egress[0].self_link]
  source_subnetwork_ip_ranges_to_nat = "LIST_OF_SUBNETWORKS"
  subnetwork {
    name                    = google_compute_subnetwork.hot[0].id
    source_ip_ranges_to_nat = ["ALL_IP_RANGES"]
  }
  log_config {
    enable = true
    filter = "ERRORS_ONLY"
  }
}

# -- Identities and secrets ---------------------------------------------------

# One service account per role.
resource "google_service_account" "role" {
  for_each     = local.roles
  account_id   = "${substr(local.tag, 0, 24)}-${local.role_codes[each.key]}"
  display_name = "Hot ${local.config.name} ${each.key}"
  depends_on   = [google_project_service.required]
}
resource "google_project_iam_member" "telemetry" {
  for_each = { for pair in setproduct(keys(local.roles), ["roles/logging.logWriter", "roles/monitoring.metricWriter"]) : "${pair[0]} ${pair[1]}" => pair }
  project  = local.config.project
  role     = each.value[1]
  member   = "serviceAccount:${google_service_account.role[each.value[0]].email}"
}
# A release pointer per role, and the migration pointer the scheduler follows.
# The release process adds versions {"release": "X.Y.Z"}.
resource "google_secret_manager_secret" "pointer" {
  for_each  = toset(concat(keys(local.roles), ["migrate"]))
  secret_id = "${local.tag}-release-${each.key}"
  replication {
    auto {}
  }
  depends_on = [google_project_service.required]
}
resource "google_secret_manager_secret" "generated" {
  secret_id = "${local.tag}-generated"
  replication {
    auto {}
  }
  lifecycle { prevent_destroy = true }
  depends_on = [google_project_service.required]
}
resource "google_secret_manager_secret" "operator" {
  secret_id = "${local.tag}-operator"
  replication {
    auto {}
  }
  depends_on = [google_project_service.required]
}
locals {
  # Each role reads its own pointer and the shared secrets; the scheduler also
  # reads the migration pointer. The generated secret is created by whichever
  # host starts first.
  secret_reads = merge(
    { for role in keys(local.roles) : "${role} pointer" => { role = role, secret = google_secret_manager_secret.pointer[role].id } },
    { for role in keys(local.roles) : "${role} generated" => { role = role, secret = google_secret_manager_secret.generated.id } },
    { for role in keys(local.roles) : "${role} operator" => { role = role, secret = google_secret_manager_secret.operator.id } },
    { "scheduler migrate" = { role = "scheduler", secret = google_secret_manager_secret.pointer["migrate"].id } },
  )
}
resource "google_secret_manager_secret_iam_member" "read" {
  for_each  = local.secret_reads
  project   = local.config.project
  secret_id = each.value.secret
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.role[each.value.role].email}"
}
resource "google_secret_manager_secret_iam_member" "store_generated" {
  for_each  = google_service_account.role
  project   = local.config.project
  secret_id = google_secret_manager_secret.generated.id
  role      = "roles/secretmanager.secretVersionAdder"
  member    = "serviceAccount:${each.value.email}"
}
# Agent commands: one subscription, so a command reaches one instance.
resource "google_pubsub_topic" "commands" {
  name                       = "${local.tag}-commands"
  message_retention_duration = "3600s"
  depends_on                 = [google_project_service.required]
}
resource "google_pubsub_subscription" "commands" {
  name                       = "${local.tag}-agent"
  topic                      = google_pubsub_topic.commands.id
  ack_deadline_seconds       = 60
  message_retention_duration = "3600s"
  expiration_policy {
    ttl = ""
  }
}
resource "google_pubsub_subscription_iam_member" "agent" {
  for_each     = google_service_account.role
  subscription = google_pubsub_subscription.commands.name
  role         = "roles/pubsub.subscriber"
  member       = "serviceAccount:${each.value.email}"
}
# The release workflow may add pointer versions, publish commands, read the
# agents' status logs, and list each role's instances.
resource "google_secret_manager_secret_iam_member" "release" {
  for_each = var.release_principal == "" ? {} : {
    for pair in setproduct(keys(google_secret_manager_secret.pointer), ["roles/secretmanager.secretVersionAdder", "roles/secretmanager.secretAccessor"]) :
    "${pair[0]} ${pair[1]}" => { pointer = pair[0], role = pair[1] }
  }
  project   = local.config.project
  secret_id = google_secret_manager_secret.pointer[each.value.pointer].id
  role      = each.value.role
  member    = "serviceAccount:${var.release_principal}"
}
resource "google_pubsub_topic_iam_member" "release" {
  count  = var.release_principal == "" ? 0 : 1
  topic  = google_pubsub_topic.commands.name
  role   = "roles/pubsub.publisher"
  member = "serviceAccount:${var.release_principal}"
}
resource "google_project_iam_member" "release" {
  for_each = var.release_principal == "" ? toset([]) : toset(["roles/logging.viewer", "roles/compute.viewer"])
  project  = local.config.project
  role     = each.value
  member   = "serviceAccount:${var.release_principal}"
}

# -- Instances ----------------------------------------------------------------

resource "google_compute_instance_template" "role" {
  for_each     = local.roles
  name_prefix  = "${local.tag}-${local.role_codes[each.key]}-"
  machine_type = each.value.gcp
  labels       = { hot-installation = local.config.name, hot-role = each.key, managed-by = "opentofu" }
  disk {
    boot         = true
    source_image = local.config.host_image
    disk_size_gb = 40
    disk_type    = "pd-balanced"
    auto_delete  = true
  }
  # Scratch: releases, logs, and box storage. Nothing on it outlives the
  # instance; data lives in the managed services and buckets.
  disk {
    device_name  = "hot-data"
    disk_size_gb = each.key == "task-worker" ? module.contract.capacity.data_gb : 20
    disk_type    = "pd-balanced"
    auto_delete  = true
  }
  network_interface {
    subnetwork = local.subnet_id
    # No access_config: no external IP.
  }
  service_account {
    email  = google_service_account.role[each.key].email
    scopes = ["https://www.googleapis.com/auth/cloud-platform"]
  }
  # Only task-workers run box tasks in Kata VMs.
  advanced_machine_features {
    enable_nested_virtualization = each.key == "task-worker"
  }
  min_cpu_platform = each.key == "task-worker" ? "Intel Cascade Lake" : null
  shielded_instance_config {
    enable_secure_boot          = true
    enable_vtpm                 = true
    enable_integrity_monitoring = true
  }
  scheduling {
    on_host_maintenance = each.key == "task-worker" ? "TERMINATE" : "MIGRATE"
    automatic_restart   = true
  }
  metadata = {
    block-project-ssh-keys = "TRUE"
    enable-oslogin         = "FALSE"
    serial-port-enable     = "FALSE"
    startup-script = templatefile("${path.module}/../../../../runtime/bootstrap.sh.tftpl", {
      config_base64 = base64encode(jsonencode(local.host_config[each.key]))
    })
  }
  lifecycle { create_before_destroy = true }
  depends_on = [google_compute_router_nat.egress, google_project_iam_member.telemetry, google_secret_manager_secret_iam_member.read]
}
# Every role but the scheduler spreads across the region's zones.
resource "google_compute_region_instance_group_manager" "role" {
  for_each           = { for role, group in local.roles : role => group if role != "scheduler" }
  name               = "${local.tag}-${each.key}"
  region             = local.config.region
  base_instance_name = "${local.tag}-${local.role_codes[each.key]}"
  target_size        = each.value.min
  version {
    instance_template = google_compute_instance_template.role[each.key].id
  }
  dynamic "named_port" {
    for_each = contains(local.web_roles, each.key) ? [443] : []
    content {
      name = "https"
      port = named_port.value
    }
  }
  dynamic "auto_healing_policies" {
    for_each = contains(local.web_roles, each.key) ? [1] : []
    content {
      health_check      = google_compute_region_health_check.web.id
      initial_delay_sec = 600
    }
  }
  # A new host image or machine type replaces instances with surge capacity;
  # releases themselves roll in place through the release pointers.
  update_policy {
    type                  = "PROACTIVE"
    minimal_action        = "REPLACE"
    max_surge_fixed       = 3
    max_unavailable_fixed = 0
  }
  # Autoscaled roles set their own size.
  lifecycle { ignore_changes = [target_size] }
}
# The scheduler: one instance, replaced stop-before-start so two schedulers
# never run at once.
resource "google_compute_instance_group_manager" "scheduler" {
  name               = "${local.tag}-scheduler"
  zone               = local.config.zone
  base_instance_name = "${local.tag}-${local.role_codes["scheduler"]}"
  target_size        = 1
  version {
    instance_template = google_compute_instance_template.role["scheduler"].id
  }
  update_policy {
    type                  = "PROACTIVE"
    minimal_action        = "REPLACE"
    max_surge_fixed       = 0
    max_unavailable_fixed = 1
    replacement_method    = "RECREATE"
  }
}
# App and API follow CPU. Workers and task-workers keep fixed counts unless a
# maximum above the minimum is set; task-workers then only scale out, since a
# managed instance group cannot protect an instance that is running tasks.
resource "google_compute_region_autoscaler" "role" {
  for_each = { for role, group in local.roles : role => group if role != "scheduler" && (contains(local.web_roles, role) || group.max > group.min) }
  name     = "${local.tag}-${each.key}"
  region   = local.config.region
  target   = google_compute_region_instance_group_manager.role[each.key].id
  autoscaling_policy {
    min_replicas    = each.value.min
    max_replicas    = each.value.max
    cooldown_period = 300
    mode            = each.key == "task-worker" ? "ONLY_SCALE_OUT" : "ON"
    cpu_utilization {
      target = 0.6
    }
  }
}

output "egress_ips" { value = google_compute_address.egress[*].address }
output "configuration" { value = local.host_config }
output "release_secrets" { value = { for role, secret in google_secret_manager_secret.pointer : role => secret.id } }
output "operator_secret" { value = google_secret_manager_secret.operator.id }
output "command_topic" { value = google_pubsub_topic.commands.id }
output "instance_groups" { value = merge({ for role, group in google_compute_region_instance_group_manager.role : role => group.name }, { scheduler = google_compute_instance_group_manager.scheduler.name }) }
