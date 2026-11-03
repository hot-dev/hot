# Public app and/or API access: a global external Application Load Balancer
# with Google-managed certificates re-encrypts to the host's load-balancer
# port, which serves only the public hostnames. The VM keeps no external IP.
# Each hostname's certificate becomes active once its DNS points at the
# output address.
locals {
  public_roles = [for role in ["app", "api"] : role if local.config["${role}_exposure"] == "public"]
  public_hosts = [for role in local.public_roles : local.config["${role}_hostname"]]
  public       = length(local.public_hosts) > 0
  # A firewall expands a network's bare name, but the provider sends a group's
  # network to the API as given. The full form also matches the self link of a
  # group that already exists, so spelling it out does not replace the group.
  public_group_network = strcontains(local.network_id, "projects/") ? local.network_id : "projects/${local.config.project}/global/networks/${basename(local.network_id)}"
}
resource "google_compute_global_address" "public" {
  count      = local.public ? 1 : 0
  name       = "${local.tag}-public"
  depends_on = [google_project_service.required]
}
# A certificate per public hostname, so making another endpoint public or
# renaming one leaves the other's certificate serving. A certificate's
# domains cannot change: a new hostname gets a new certificate, named after
# it, which the proxy takes before the old one is deleted.
resource "google_compute_managed_ssl_certificate" "public" {
  for_each = toset(local.public_roles)
  name     = "${local.tag}-${each.key}-${substr(sha1(local.config["${each.key}_hostname"]), 0, 8)}"
  managed {
    domains = [local.config["${each.key}_hostname"]]
  }
  lifecycle { create_before_destroy = true }
  depends_on = [google_project_service.required]
}
# The single certificate earlier versions made; it is replaced like a
# renamed hostname. With only the API public, move it to ["api"] with
# `tofu state mv` before applying instead. This upgrade renames every
# certificate at once, so public TLS fails until the new ones are active,
# which needs each hostname's DNS to point at the load balancer.
moved {
  from = google_compute_managed_ssl_certificate.public[0]
  to   = google_compute_managed_ssl_certificate.public["app"]
}
# The network is explicit because a backend service takes a group only if it
# has one, and this group is empty until the host joins and again while a
# replaced host is gone.
resource "google_compute_instance_group" "public" {
  count   = local.public ? 1 : 0
  name    = "${local.tag}-public"
  zone    = local.config.zone
  network = local.public_group_network
  named_port {
    name = "hot-lb"
    port = 9443
  }
}
# The host joins the group through a membership that is replaced with the
# VM: a new VM keeps the old one's name and self link, and the group drops
# the deleted VM, so a list of instances would see no change and leave the
# load balancer with no backend.
#
# OpenTofu removes the membership before it deletes the VM, and removing it only
# forgets it (deletion_policy): the group drops the VM when the VM goes. A VM
# whose deletion protection was not lifted refuses the delete, which stops the
# apply with the VM running and still in the group, so the endpoint keeps
# serving. To recover, lift protection and apply again. To undo the change
# instead (restore host_image, or drop -replace), import the membership as below
# before applying, since adding the VM again fails. A membership from before this
# setting still takes the VM out on its first replacement. create_before_destroy
# here would spread to the VM, whose name is fixed.
#
# Upgrading a VM that is already public: earlier versions added it through the
# group, and adding it again fails. Once, before the first apply, run
#   tofu import 'google_compute_instance_group_membership.public[0]' ZONE/hot-NAME-public/hot-NAME
# with the variables you apply with, or take it out of the group, which takes
# the endpoint down until the apply adds it back:
#   gcloud compute instance-groups unmanaged remove-instances hot-NAME-public --zone ZONE --instances hot-NAME
# An apply that also replaces the VM needs neither, and the first apply after
# v3.0.1 does, for the host's new address.
resource "google_compute_instance_group_membership" "public" {
  count           = local.public ? 1 : 0
  zone            = local.config.zone
  instance_group  = google_compute_instance_group.public[0].name
  instance        = google_compute_instance.host.self_link
  deletion_policy = "ABANDON"
  lifecycle { replace_triggered_by = [google_compute_instance.host.instance_id] }
}
resource "google_compute_health_check" "public" {
  count = local.public ? 1 : 0
  name  = "${local.tag}-public"
  https_health_check {
    port         = 9443
    request_path = "/hot-lb-health"
  }
  depends_on = [google_project_service.required]
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
  depends_on      = [google_project_service.required]
}
resource "google_compute_target_https_proxy" "public" {
  count            = local.public ? 1 : 0
  name             = "${local.tag}-public"
  url_map          = google_compute_url_map.public[0].id
  ssl_certificates = [for role in local.public_roles : google_compute_managed_ssl_certificate.public[role].id]
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
