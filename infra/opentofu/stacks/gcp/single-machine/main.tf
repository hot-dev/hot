terraform {
  required_version = ">= 1.13.1, < 1.14.0"
  required_providers {
    google = { source = "hashicorp/google", version = "8.7.0" }
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
locals {
  config      = module.contract.configuration
  new_network = local.config.network.mode == "new"
  network_id  = local.new_network ? google_compute_network.hot[0].id : local.config.network.network_id
  subnet_id   = local.new_network ? google_compute_subnetwork.hot[0].id : local.config.network.subnet_id
  tag         = "hot-${local.config.name}"
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
resource "google_service_account" "host" {
  account_id   = local.tag
  display_name = "Hot ${local.config.name} host"
  depends_on   = [google_project_service.required]
}
resource "google_project_iam_member" "telemetry" {
  for_each = toset(["roles/logging.logWriter", "roles/monitoring.metricWriter"])
  project  = local.config.project
  role     = each.value
  member   = "serviceAccount:${google_service_account.host.email}"
}
# The release pointer: the release process adds a version {"release": "X.Y.Z"};
# OpenTofu only creates the secret. Secret Manager has a "latest" alias, which
# the agent reads.
resource "google_secret_manager_secret" "release" {
  secret_id = "${local.tag}-release"
  replication {
    auto {}
  }
  depends_on = [google_project_service.required]
}
# Values the installation generates for itself, stored once by the agent.
resource "google_secret_manager_secret" "generated" {
  secret_id = "${local.tag}-generated"
  replication {
    auto {}
  }
  lifecycle { prevent_destroy = true }
  depends_on = [google_project_service.required]
}
# The operator's TLS certificate and key, and any SMTP, OAuth, or S3
# credentials, as one JSON object.
resource "google_secret_manager_secret" "operator" {
  secret_id = "${local.tag}-operator"
  replication {
    auto {}
  }
  depends_on = [google_project_service.required]
}
resource "google_secret_manager_secret_iam_member" "read" {
  for_each  = { release = google_secret_manager_secret.release.id, generated = google_secret_manager_secret.generated.id, operator = google_secret_manager_secret.operator.id }
  project   = local.config.project
  secret_id = each.value
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.host.email}"
}
resource "google_secret_manager_secret_iam_member" "store_generated" {
  project   = local.config.project
  secret_id = google_secret_manager_secret.generated.id
  role      = "roles/secretmanager.secretVersionAdder"
  member    = "serviceAccount:${google_service_account.host.email}"
}
# The only way to run anything on the host: agent commands published here.
# The agent pulls them, so the host needs no inbound connectivity.
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
  subscription = google_pubsub_subscription.commands.name
  role         = "roles/pubsub.subscriber"
  member       = "serviceAccount:${google_service_account.host.email}"
}
# Existing GCS buckets: the host may use objects under its prefix only. The
# bucket must use uniform bucket-level access for the condition to apply.
resource "google_storage_bucket_iam_member" "storage" {
  for_each = { for role, s in local.config.storage : role => s if s.backend == "gcs" }
  bucket   = each.value.bucket
  role     = "roles/storage.objectUser"
  member   = "serviceAccount:${google_service_account.host.email}"
  condition {
    title      = "hot-${local.config.name}-${each.key}"
    expression = "resource.type == \"storage.googleapis.com/Bucket\" || resource.name.startsWith(\"projects/_/buckets/${each.value.bucket}/objects/${trim(each.value.prefix, "/")}/\")"
  }
}
# The release workflow may add pointer versions, publish commands, and read
# the agent's status logs.
resource "google_secret_manager_secret_iam_member" "release" {
  count     = var.release_principal == "" ? 0 : 1
  project   = local.config.project
  secret_id = google_secret_manager_secret.release.id
  role      = "roles/secretmanager.secretVersionAdder"
  member    = "serviceAccount:${var.release_principal}"
}
resource "google_pubsub_topic_iam_member" "release" {
  count  = var.release_principal == "" ? 0 : 1
  topic  = google_pubsub_topic.commands.name
  role   = "roles/pubsub.publisher"
  member = "serviceAccount:${var.release_principal}"
}
resource "google_project_iam_member" "release_logs" {
  count   = var.release_principal == "" ? 0 : 1
  project = local.config.project
  role    = "roles/logging.viewer"
  member  = "serviceAccount:${var.release_principal}"
}
locals {
  host_config = merge(local.config, {
    data_volume_id       = google_compute_disk.data.id
    release_pointer      = google_secret_manager_secret.release.id
    status_reference     = "${local.tag}-status"
    command_subscription = google_pubsub_subscription.commands.id
    secret_references    = { generated = google_secret_manager_secret.generated.id, operator = google_secret_manager_secret.operator.id }
  })
}
resource "google_compute_firewall" "https" {
  name    = "${local.tag}-https"
  network = local.network_id
  # IAP forwards HTTPS, never SSH. IAM on IAP controls who can use the tunnel.
  source_ranges           = concat(local.config.network.client_cidrs, ["35.235.240.0/20"])
  target_service_accounts = [google_service_account.host.email]
  allow {
    protocol = "tcp"
    ports    = ["443"]
  }
}
resource "google_compute_disk" "data" {
  name   = "${local.tag}-data"
  zone   = local.config.zone
  type   = "pd-balanced"
  size   = module.contract.capacity.data_gb
  labels = { hot-installation = local.config.name }
  # Google-managed at-rest encryption is unconditional for Persistent Disk.
  lifecycle { prevent_destroy = true }
  depends_on = [google_project_service.required]
}
resource "google_compute_instance" "host" {
  name                = local.tag
  zone                = local.config.zone
  machine_type        = module.contract.capacity.gcp
  min_cpu_platform    = "Intel Cascade Lake"
  deletion_protection = true
  can_ip_forward      = false
  labels              = { hot-installation = local.config.name, managed-by = "opentofu" }
  boot_disk {
    auto_delete = true
    initialize_params {
      image = local.config.host_image
      size  = 40
      type  = "pd-balanced"
    }
  }
  attached_disk {
    source      = google_compute_disk.data.id
    device_name = "hot-data"
    mode        = "READ_WRITE"
  }
  network_interface {
    subnetwork = local.subnet_id
    # Intentionally no access_config or ipv6_access_config: no external IP.
  }
  service_account {
    email  = google_service_account.host.email
    scopes = ["https://www.googleapis.com/auth/cloud-platform"]
  }
  advanced_machine_features { enable_nested_virtualization = true }
  shielded_instance_config {
    enable_secure_boot          = true
    enable_vtpm                 = true
    enable_integrity_monitoring = true
  }
  scheduling {
    on_host_maintenance = "TERMINATE"
    automatic_restart   = true
  }
  metadata = {
    block-project-ssh-keys = "TRUE"
    enable-oslogin         = "FALSE"
    serial-port-enable     = "FALSE"
    startup-script = templatefile("${path.module}/../../../../runtime/bootstrap.sh.tftpl", {
      config_base64 = base64encode(jsonencode(local.host_config))
    })
  }
  lifecycle {
    precondition {
      condition     = local.config.cloud == "gcp"
      error_message = "The GCP root requires installation.cloud = gcp."
    }
  }
  depends_on = [google_compute_router_nat.egress, google_project_iam_member.telemetry, google_secret_manager_secret_iam_member.read, google_pubsub_subscription_iam_member.agent]
}
output "private_ip" { value = google_compute_instance.host.network_interface[0].network_ip }
output "instance_id" { value = google_compute_instance.host.id }
output "retained_data_volume_id" { value = google_compute_disk.data.id }
output "egress_ips" { value = google_compute_address.egress[*].address }
output "configuration" { value = local.host_config }
output "release_secret" { value = google_secret_manager_secret.release.id }
output "operator_secret" { value = google_secret_manager_secret.operator.id }
output "command_topic" { value = google_pubsub_topic.commands.id }
