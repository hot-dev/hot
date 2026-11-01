# Public app and/or API access: a global external Application Load Balancer
# with a Google-managed certificate re-encrypts to the host's load-balancer
# port, which serves only the public hostnames. The VM keeps no external IP.
# The certificate becomes active once each public hostname's DNS points at
# the output address.
locals {
  public_hosts = [for role in ["app", "api"] : local.config["${role}_hostname"] if local.config["${role}_exposure"] == "public"]
  public       = length(local.public_hosts) > 0
}
resource "google_compute_global_address" "public" {
  count = local.public ? 1 : 0
  name  = "${local.tag}-public"
}
resource "google_compute_managed_ssl_certificate" "public" {
  count = local.public ? 1 : 0
  name  = "${local.tag}-public"
  managed {
    domains = local.public_hosts
  }
}
resource "google_compute_instance_group" "public" {
  count     = local.public ? 1 : 0
  name      = "${local.tag}-public"
  zone      = local.config.zone
  instances = [google_compute_instance.host.self_link]
  named_port {
    name = "hot-lb"
    port = 9443
  }
}
resource "google_compute_health_check" "public" {
  count = local.public ? 1 : 0
  name  = "${local.tag}-public"
  https_health_check {
    port         = 9443
    request_path = "/hot-lb-health"
  }
}
resource "google_compute_backend_service" "public" {
  count                 = local.public ? 1 : 0
  name                  = "${local.tag}-public"
  protocol              = "HTTPS"
  port_name             = "hot-lb"
  load_balancing_scheme = "EXTERNAL_MANAGED"
  timeout_sec           = 3600
  health_checks         = [google_compute_health_check.public[0].id]
  backend {
    group = google_compute_instance_group.public[0].id
  }
}
# The host refuses hostnames that are not public on this port as well.
resource "google_compute_url_map" "public" {
  count           = local.public ? 1 : 0
  name            = "${local.tag}-public"
  default_service = google_compute_backend_service.public[0].id
  host_rule {
    hosts        = local.public_hosts
    path_matcher = "hot"
  }
  path_matcher {
    name            = "hot"
    default_service = google_compute_backend_service.public[0].id
  }
}
resource "google_compute_ssl_policy" "public" {
  count           = local.public ? 1 : 0
  name            = "${local.tag}-public"
  profile         = "MODERN"
  min_tls_version = "TLS_1_2"
}
resource "google_compute_target_https_proxy" "public" {
  count            = local.public ? 1 : 0
  name             = "${local.tag}-public"
  url_map          = google_compute_url_map.public[0].id
  ssl_certificates = [google_compute_managed_ssl_certificate.public[0].id]
  ssl_policy       = google_compute_ssl_policy.public[0].id
}
resource "google_compute_global_forwarding_rule" "public" {
  count                 = local.public ? 1 : 0
  name                  = "${local.tag}-public"
  target                = google_compute_target_https_proxy.public[0].id
  ip_address            = google_compute_global_address.public[0].id
  port_range            = "443"
  load_balancing_scheme = "EXTERNAL_MANAGED"
}
# Google's front ends and health checkers reach the load-balancer port only.
resource "google_compute_firewall" "public" {
  count                   = local.public ? 1 : 0
  name                    = "${local.tag}-public"
  network                 = local.network_id
  source_ranges           = ["35.191.0.0/16", "130.211.0.0/22"]
  target_service_accounts = [google_service_account.host.email]
  allow {
    protocol = "tcp"
    ports    = ["9443"]
  }
}
output "public_endpoint" {
  description = "Point each public hostname's DNS A record here."
  value       = local.public ? google_compute_global_address.public[0].address : ""
}
