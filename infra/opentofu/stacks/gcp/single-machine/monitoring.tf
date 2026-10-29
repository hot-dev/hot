# Backups and alerts for the single machine. A snapshot schedule snapshots the
# retained data disk daily; the host cannot delete the snapshots. The host
# agent publishes health metrics to Cloud Monitoring, and these alert
# policies notify email channels, independently of Hot's own email.
resource "google_compute_resource_policy" "snapshots" {
  name   = "${local.tag}-snapshots"
  region = local.config.region
  snapshot_schedule_policy {
    schedule {
      daily_schedule {
        days_in_cycle = 1
        start_time    = local.config.backup.time_utc
      }
    }
    retention_policy {
      max_retention_days    = local.config.backup.retention_days
      on_source_disk_delete = "KEEP_AUTO_SNAPSHOTS"
    }
    snapshot_properties {
      # The agent finds the installation's snapshots by this label.
      labels            = { hot-installation = local.config.name }
      storage_locations = [local.config.region]
    }
  }
  depends_on = [google_project_service.required]
}
resource "google_compute_disk_resource_policy_attachment" "snapshots" {
  name = google_compute_resource_policy.snapshots.name
  disk = google_compute_disk.data.name
  zone = local.config.zone
}
# The agent reads snapshot times to report backup age. Custom role IDs stay
# reserved for a week after deletion.
resource "google_project_iam_custom_role" "snapshots" {
  role_id     = "hot_${replace(local.config.name, "-", "_")}_snapshots"
  title       = "Hot ${local.config.name} snapshot reader"
  permissions = ["compute.snapshots.list"]
}
resource "google_project_iam_member" "snapshots" {
  project = local.config.project
  role    = google_project_iam_custom_role.snapshots.id
  member  = "serviceAccount:${google_service_account.host.email}"
}
locals {
  # Metric types are per installation: descriptors are project-wide, and
  # deleting one deletes its data, so installations never share one.
  metric_prefix = "custom.googleapis.com/hot/${local.config.name}/"
  metrics = {
    agent_heartbeat        = { unit = "1", name = "Agent heartbeat" }
    unhealthy_services     = { unit = "1", name = "Unhealthy services" }
    scheduler_running      = { unit = "1", name = "Scheduler running" }
    dependencies_reachable = { unit = "1", name = "Dependencies reachable" }
    ready                  = { unit = "1", name = "App and API ready" }
    data_disk_used_percent = { unit = "%", name = "Data disk used" }
    backup_age_hours       = { unit = "h", name = "Backup age" }
    certificate_days_left  = { unit = "d", name = "Certificate days left" }
  }
  # Thresholds and sustained windows match the AWS alarms; see the runbook in
  # infra/README.md. A missing signal alerts where an absent value is unsafe.
  alerts = {
    services-unhealthy = {
      metric      = "unhealthy_services", comparison = "COMPARISON_GT", threshold = 0, aligner = "ALIGN_MIN", duration = "900s", absent = false,
      description = "A Hot, ingress, or local data service has not been running and healthy for 15 minutes."
    }
    scheduler-stopped = {
      metric      = "scheduler_running", comparison = "COMPARISON_LT", threshold = 1, aligner = "ALIGN_MAX", duration = "600s", absent = false,
      description = "The scheduler has not been running for 10 minutes: schedules are not firing."
    }
    dependencies-unreachable = {
      metric      = "dependencies_reachable", comparison = "COMPARISON_LT", threshold = 1, aligner = "ALIGN_MAX", duration = "600s", absent = false,
      description = "PostgreSQL or Valkey has been unreachable from the host for 10 minutes."
    }
    not-ready = {
      metric      = "ready", comparison = "COMPARISON_LT", threshold = 1, aligner = "ALIGN_MAX", duration = "900s", absent = false,
      description = "The app or API has not answered through the ingress for 15 minutes."
    }
    disk-pressure = {
      metric      = "data_disk_used_percent", comparison = "COMPARISON_GT", threshold = 85, aligner = "ALIGN_MAX", duration = "900s", absent = false,
      description = "The data disk is over 85% full (space or inodes)."
    }
    backup-stale = {
      metric      = "backup_age_hours", comparison = "COMPARISON_GT", threshold = 30, aligner = "ALIGN_MIN", duration = "3600s", absent = true,
      description = "No data-disk snapshot has completed in 30 hours, or the agent cannot tell."
    }
    certificate-expiring = {
      metric      = "certificate_days_left", comparison = "COMPARISON_LT", threshold = 14, aligner = "ALIGN_MIN", duration = "3600s", absent = false,
      description = "The ingress TLS certificate expires within 14 days, or the agent cannot read it."
    }
  }
}
resource "google_monitoring_metric_descriptor" "hot" {
  for_each     = local.metrics
  type         = "${local.metric_prefix}${each.key}"
  display_name = "Hot ${local.config.name}: ${each.value.name}"
  description  = "Published by the Hot host agent."
  metric_kind  = "GAUGE"
  value_type   = "DOUBLE"
  unit         = each.value.unit
  depends_on   = [google_project_service.required]
}
resource "google_monitoring_notification_channel" "email" {
  for_each     = toset(local.config.monitoring.alert_emails)
  display_name = "Hot ${local.config.name} alerts: ${each.value}"
  type         = "email"
  labels       = { email_address = each.value }
  depends_on   = [google_project_service.required]
}
resource "google_monitoring_alert_policy" "host_down" {
  display_name          = "hot-${local.config.name}-host-down"
  combiner              = "OR"
  severity              = "CRITICAL"
  notification_channels = [for channel in google_monitoring_notification_channel.email : channel.id]
  documentation {
    content   = "The host agent stopped reporting: the VM, Docker, or the agent is down."
    mime_type = "text/markdown"
  }
  conditions {
    display_name = "No agent heartbeat for 5 minutes"
    condition_absent {
      filter   = "resource.type = \"gce_instance\" AND metric.type = \"${google_monitoring_metric_descriptor.hot["agent_heartbeat"].type}\""
      duration = "300s"
      aggregations {
        alignment_period   = "60s"
        per_series_aligner = "ALIGN_MAX"
      }
    }
  }
}
resource "google_monitoring_alert_policy" "hot" {
  for_each              = local.alerts
  display_name          = "hot-${local.config.name}-${each.key}"
  combiner              = "OR"
  severity              = "CRITICAL"
  notification_channels = [for channel in google_monitoring_notification_channel.email : channel.id]
  documentation {
    content   = each.value.description
    mime_type = "text/markdown"
  }
  conditions {
    display_name = each.value.description
    condition_threshold {
      filter          = "resource.type = \"gce_instance\" AND metric.type = \"${google_monitoring_metric_descriptor.hot[each.value.metric].type}\""
      comparison      = each.value.comparison
      threshold_value = each.value.threshold
      duration        = each.value.duration
      aggregations {
        alignment_period   = "300s"
        per_series_aligner = each.value.aligner
      }
    }
  }
  dynamic "conditions" {
    for_each = each.value.absent ? [true] : []
    content {
      display_name = "No ${each.value.metric} reported"
      condition_absent {
        filter   = "resource.type = \"gce_instance\" AND metric.type = \"${google_monitoring_metric_descriptor.hot[each.value.metric].type}\""
        duration = each.value.duration
        aggregations {
          alignment_period   = "300s"
          per_series_aligner = "ALIGN_MAX"
        }
      }
    }
  }
}
