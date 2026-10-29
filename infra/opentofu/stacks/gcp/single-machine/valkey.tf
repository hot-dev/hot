# Managed Valkey (data.valkey.mode = "managed"): Memorystore for Valkey with
# cluster mode disabled (one shard, so Hot's standalone client applies),
# reached through Private Service Connect, TLS-only, and authenticated with
# IAM: the host's service account presents short-lived access tokens, so
# there is no password to generate or store. The agent gives Hot services
# redis.auth = gcp-iam and the instance's CA.
locals {
  managed_valkey = local.config.data.valkey.mode == "managed"
  valkey         = local.config.data.valkey
  valkey_primary = !local.managed_valkey ? null : one([
    for connection in google_memorystore_instance.valkey[0].endpoints[0].connections : connection.psc_auto_connection[0]
    if connection.psc_auto_connection[0].connection_type == "CONNECTION_TYPE_PRIMARY"
  ])
  valkey_endpoint = !local.managed_valkey ? null : {
    host = local.valkey_primary.ip_address
    port = local.valkey_primary.port
    auth = "gcp-iam"
    # The instance's own CA. Its certificates name no address, and only this
    # instance's servers hold them.
    ca_certificate = join("", flatten([for ca in google_memorystore_instance.valkey[0].managed_server_ca[0].ca_certs : ca.certificates]))
  }
}
resource "google_project_service" "valkey" {
  for_each           = toset(local.managed_valkey ? ["memorystore.googleapis.com", "networkconnectivity.googleapis.com", "serviceconsumermanagement.googleapis.com"] : [])
  project            = local.config.project
  service            = each.value
  disable_on_destroy = false
}
# Memorystore places its endpoints in the network through Private Service
# Connect, which needs a service connection policy for the region. An
# existing network must have one already; see infra/README.md.
resource "google_network_connectivity_service_connection_policy" "valkey" {
  count         = local.managed_valkey && local.new_network ? 1 : 0
  name          = "${local.tag}-memorystore"
  location      = local.config.region
  service_class = "gcp-memorystore"
  network       = local.network_id
  psc_config {
    subnetworks = [local.subnet_id]
  }
  depends_on = [google_project_service.valkey]
}
resource "google_memorystore_instance" "valkey" {
  count                       = local.managed_valkey ? 1 : 0
  instance_id                 = "${local.tag}-valkey"
  location                    = local.config.region
  mode                        = "CLUSTER_DISABLED"
  shard_count                 = 1
  replica_count               = local.valkey.high_availability ? 1 : 0
  node_type                   = module.contract.managed_valkey.node_type
  engine_version              = "VALKEY_8_0"
  authorization_mode          = "IAM_AUTH"
  transit_encryption_mode     = "SERVER_AUTHENTICATION"
  deletion_protection_enabled = true
  # Queued work must never be evicted.
  engine_configs = { "maxmemory-policy" = "noeviction" }
  persistence_config {
    mode = "AOF"
    aof_config {
      append_fsync = "EVERY_SEC"
    }
  }
  zone_distribution_config {
    mode = local.valkey.high_availability ? "MULTI_ZONE" : "SINGLE_ZONE"
    zone = local.valkey.high_availability ? null : local.config.zone
  }
  desired_auto_created_endpoints {
    network    = local.network_id
    project_id = local.config.project
  }
  labels     = { hot-installation = local.config.name, managed-by = "opentofu" }
  depends_on = [google_project_service.valkey, google_network_connectivity_service_connection_policy.valkey]
}
# The host may connect to this instance only.
resource "google_project_iam_member" "valkey" {
  count   = local.managed_valkey ? 1 : 0
  project = local.config.project
  role    = "roles/memorystore.dbConnectionUser"
  member  = "serviceAccount:${google_service_account.host.email}"
  condition {
    title      = "${local.tag}-valkey"
    expression = "resource.name == \"projects/${local.config.project}/locations/${local.config.region}/instances/${google_memorystore_instance.valkey[0].instance_id}\""
  }
}
output "valkey_endpoint" { value = local.managed_valkey ? local.valkey_endpoint.host : null }
