# App and API endpoints: one passthrough TCP load balancer each, internal by
# default and external when the role's exposure is public. Passthrough keeps
# the client's address and port, and TLS ends on the hosts' ingress (port 443
# there routes by Host header), so no certificate or key enters OpenTofu, and
# long-lived streams see no proxy timeout.
locals {
  # Google's health-check and passthrough load balancer source ranges.
  health_check_ranges = ["35.191.0.0/16", "130.211.0.0/22", "209.85.152.0/22", "209.85.204.0/22"]
}
resource "google_compute_region_health_check" "web" {
  name                = "${local.tag}-web"
  region              = local.config.region
  check_interval_sec  = 10
  timeout_sec         = 5
  healthy_threshold   = 2
  unhealthy_threshold = 3
  https_health_check {
    port         = 443
    request_path = "/hot-lb-health"
  }
}
resource "google_compute_region_backend_service" "web" {
  for_each              = local.web_roles
  name                  = "${local.tag}-${each.key}"
  region                = local.config.region
  protocol              = "TCP"
  load_balancing_scheme = local.exposure[each.key] == "public" ? "EXTERNAL" : "INTERNAL"
  health_checks         = [google_compute_region_health_check.web.id]
  # In-flight uploads and streams finish before an instance leaves.
  connection_draining_timeout_sec = 300
  backend {
    group          = google_compute_region_instance_group_manager.role[each.key].instance_group
    balancing_mode = "CONNECTION"
  }
}
resource "google_compute_address" "web" {
  for_each     = local.web_roles
  name         = "${local.tag}-${each.key}"
  region       = local.config.region
  address_type = local.exposure[each.key] == "public" ? "EXTERNAL" : "INTERNAL"
  subnetwork   = local.exposure[each.key] == "public" ? null : local.subnet_id
}
resource "google_compute_forwarding_rule" "web" {
  for_each              = local.web_roles
  name                  = "${local.tag}-${each.key}"
  region                = local.config.region
  load_balancing_scheme = local.exposure[each.key] == "public" ? "EXTERNAL" : "INTERNAL"
  ip_protocol           = "TCP"
  ports                 = ["443"]
  ip_address            = google_compute_address.web[each.key].address
  backend_service       = google_compute_region_backend_service.web[each.key].id
  network               = local.exposure[each.key] == "public" ? null : local.network_id
  subnetwork            = local.exposure[each.key] == "public" ? null : local.subnet_id
}
# Health checks, and clients: the client ranges for a private endpoint, any
# address for a public one. Nothing else reaches the hosts.
resource "google_compute_firewall" "web" {
  for_each                = local.web_roles
  name                    = "${local.tag}-${each.key}-https"
  network                 = local.network_id
  source_ranges           = local.exposure[each.key] == "public" ? ["0.0.0.0/0"] : concat(local.health_check_ranges, local.config.network.client_cidrs)
  target_service_accounts = [google_service_account.role[each.key].email]
  allow {
    protocol = "tcp"
    ports    = ["443"]
  }
}
output "endpoints" {
  description = "Point app_hostname and api_hostname at these addresses."
  value       = { for role, address in google_compute_address.web : role => address.address }
}
