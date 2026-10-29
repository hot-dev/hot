# Managed PostgreSQL (data.postgres.mode = "managed"): Cloud SQL with only a
# private address, reached over TLS. The administrator password is generated
# during apply and written only to write-only attributes, so it never enters
# OpenTofu state or plans; the host agent reads it from Secret Manager to
# create Hot's logins, database, and schema.
locals {
  managed_postgres = local.config.data.postgres.mode == "managed"
  postgres         = local.config.data.postgres
  host_data = merge(local.config.data, {
    postgres = merge(local.postgres, {
      endpoint = !local.managed_postgres ? null : {
        host         = google_sql_database_instance.postgres[0].private_ip_address
        port         = 5432
        admin_user   = "postgres"
        admin_secret = google_secret_manager_secret.postgres_admin[0].id
        # The instance's own CA, which signs this instance's certificates only.
        # The provider marks it sensitive, but a CA certificate is public.
        ca_certificate = nonsensitive(google_sql_database_instance.postgres[0].server_ca_cert[0].cert)
      }
    })
  })
}
resource "google_project_service" "postgres" {
  for_each           = toset(local.managed_postgres ? ["sqladmin.googleapis.com", "servicenetworking.googleapis.com"] : [])
  project            = local.config.project
  service            = each.value
  disable_on_destroy = false
}
# Private services access on the new network. An existing network must have
# it already; see infra/README.md.
resource "google_compute_global_address" "services" {
  count         = local.managed_postgres && local.new_network ? 1 : 0
  name          = "${local.tag}-services"
  purpose       = "VPC_PEERING"
  address_type  = "INTERNAL"
  prefix_length = 20
  network       = local.network_id
}
resource "google_service_networking_connection" "services" {
  count                   = local.managed_postgres && local.new_network ? 1 : 0
  network                 = local.network_id
  service                 = "servicenetworking.googleapis.com"
  reserved_peering_ranges = [google_compute_global_address.services[0].name]
  depends_on              = [google_project_service.postgres]
}
ephemeral "random_password" "postgres_admin" {
  count   = local.managed_postgres ? 1 : 0
  length  = 40
  special = false
}
resource "google_sql_database_instance" "postgres" {
  count            = local.managed_postgres ? 1 : 0
  name             = local.tag
  region           = local.config.region
  database_version = "POSTGRES_${local.postgres.version}"
  # Removing the database, including by changing data.postgres.mode, needs
  # deletion_protection turned off in a reviewed apply first.
  deletion_protection      = local.postgres.deletion_protection
  root_password_wo         = ephemeral.random_password.postgres_admin[0].result
  root_password_wo_version = local.postgres.admin_password_version
  settings {
    edition                     = "ENTERPRISE"
    tier                        = module.contract.managed_postgres.tier
    availability_type           = local.postgres.high_availability ? "REGIONAL" : "ZONAL"
    disk_type                   = "PD_SSD"
    disk_size                   = module.contract.managed_postgres.storage_gb
    disk_autoresize             = true
    deletion_protection_enabled = local.postgres.deletion_protection
    user_labels                 = { hot-installation = local.config.name, managed-by = "opentofu" }
    location_preference {
      zone = local.config.zone
    }
    ip_configuration {
      ipv4_enabled    = false
      private_network = local.network_id
      ssl_mode        = "ENCRYPTED_ONLY"
      server_ca_mode  = "GOOGLE_MANAGED_INTERNAL_CA"
    }
    backup_configuration {
      enabled                        = true
      point_in_time_recovery_enabled = true
      transaction_log_retention_days = min(7, local.postgres.backup_retention_days)
      backup_retention_settings {
        retained_backups = local.postgres.backup_retention_days
        retention_unit   = "COUNT"
      }
    }
  }
  depends_on = [google_service_networking_connection.services, google_project_service.postgres]
}
# The administrator login for the agent. Raise
# data.postgres.admin_password_version to set a new password.
resource "google_secret_manager_secret" "postgres_admin" {
  count     = local.managed_postgres ? 1 : 0
  secret_id = "${local.tag}-postgres-admin"
  replication {
    auto {}
  }
  depends_on = [google_project_service.required]
}
resource "google_secret_manager_secret_version" "postgres_admin" {
  count                  = local.managed_postgres ? 1 : 0
  secret                 = google_secret_manager_secret.postgres_admin[0].id
  secret_data_wo         = jsonencode({ username = "postgres", password = ephemeral.random_password.postgres_admin[0].result })
  secret_data_wo_version = local.postgres.admin_password_version
  # Stored only after the instance has the password, so a failed create never
  # leaves a stored password the database does not have.
  depends_on = [google_sql_database_instance.postgres]
}
resource "google_secret_manager_secret_iam_member" "postgres_admin" {
  count     = local.managed_postgres ? 1 : 0
  project   = local.config.project
  secret_id = google_secret_manager_secret.postgres_admin[0].id
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.host.email}"
}
output "postgres_private_ip" { value = local.managed_postgres ? google_sql_database_instance.postgres[0].private_ip_address : null }
