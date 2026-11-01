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
  metric_prefix   = "custom.googleapis.com/hot/${local.config.name}/"
  budget_channels = [for channel in google_monitoring_notification_channel.email : channel.id]
  metrics = {
    agent_heartbeat         = { unit = "1", name = "Agent heartbeat" }
    unhealthy_services      = { unit = "1", name = "Unhealthy services" }
    scheduler_running       = { unit = "1", name = "Scheduler running" }
    dependencies_reachable  = { unit = "1", name = "Dependencies reachable" }
    ready                   = { unit = "1", name = "App and API ready" }
    data_disk_used_percent  = { unit = "%", name = "Data disk used" }
    backup_age_hours        = { unit = "h", name = "Backup age" }
    certificate_days_left   = { unit = "d", name = "Certificate days left" }
    queue_backlog           = { unit = "1", name = "Queue backlog" }
    queue_dead_letters      = { unit = "1", name = "Dead-lettered jobs" }
    new_dead_letters        = { unit = "1", name = "New dead-lettered jobs" }
    jobs_waiting            = { unit = "1", name = "Jobs waiting" }
    job_wait_seconds        = { unit = "s", name = "Oldest job's wait" }
    tasks_waiting           = { unit = "1", name = "Tasks waiting" }
    task_wait_seconds       = { unit = "s", name = "Oldest task's wait" }
    task_slots_used_percent = { unit = "%", name = "Task slots used" }
    alert_test              = { unit = "1", name = "Test alert" }
    synthetic_ok            = { unit = "1", name = "Synthetic check passed" }
    synthetic_seconds       = { unit = "s", name = "Synthetic check duration" }
    memory_used_percent     = { unit = "%", name = "Memory used" }
  }
  # Installation-wide queue measurements, written for the project rather
  # than the instance (see the agent's QUEUE_METRICS).
  project_metrics = ["queue_backlog", "queue_dead_letters", "new_dead_letters", "jobs_waiting", "job_wait_seconds", "tasks_waiting", "task_wait_seconds"]
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
    queue-backlog = {
      metric      = "queue_backlog", comparison = "COMPARISON_GT", threshold = local.config.monitoring.queue_backlog_alert, aligner = "ALIGN_MIN", duration = "900s", absent = false,
      description = "More jobs than monitoring.queue_backlog_alert have been waiting or running in Hot's queue for 15 minutes: workers are not keeping up."
    }
    new-dead-letters = {
      metric      = "new_dead_letters", comparison = "COMPARISON_GT", threshold = 0, aligner = "ALIGN_MAX", duration = "0s", absent = false,
      description = "Jobs failed every attempt and were dead-lettered; the worker's logs say why."
    }
    synthetic-failed = {
      metric      = "synthetic_ok", comparison = "COMPARISON_LT", threshold = 1, aligner = "ALIGN_MIN", duration = "600s", absent = false,
      description = "The synthetic event's run has not succeeded for 10 minutes: the API, queue, workers, or database are failing end to end."
    }
    memory-pressure = {
      metric      = "memory_used_percent", comparison = "COMPARISON_GT", threshold = 90, aligner = "ALIGN_MAX", duration = "900s", absent = false,
      description = "A host has used over 90% of its memory for 15 minutes; the kernel may soon kill containers."
    }
    alert-test = {
      metric      = "alert_test", comparison = "COMPARISON_GT", threshold = 0, aligner = "ALIGN_MAX", duration = "0s", absent = false,
      description = "A test alert sent with the test-alert command; it resolves by itself. Nothing is wrong."
    }
    job-wait = {
      metric      = "job_wait_seconds", comparison = "COMPARISON_GT", threshold = local.config.monitoring.queue_wait_alert, aligner = "ALIGN_MIN", duration = "600s", absent = false,
      description = "A job has waited longer than monitoring.queue_wait_alert seconds for a worker, for 10 minutes: workers are not keeping up."
    }
    task-wait = {
      metric      = "task_wait_seconds", comparison = "COMPARISON_GT", threshold = local.config.monitoring.queue_wait_alert, aligner = "ALIGN_MIN", duration = "600s", absent = false,
      description = "A task has waited longer than monitoring.queue_wait_alert seconds for a task worker, for 10 minutes: task capacity is short."
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
      filter          = "resource.type = \"${contains(local.project_metrics, each.value.metric) ? "global" : "gce_instance"}\" AND metric.type = \"${google_monitoring_metric_descriptor.hot[each.value.metric].type}\""
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
        filter   = "resource.type = \"${contains(local.project_metrics, each.value.metric) ? "global" : "gce_instance"}\" AND metric.type = \"${google_monitoring_metric_descriptor.hot[each.value.metric].type}\""
        duration = each.value.duration
        aggregations {
          alignment_period   = "300s"
          per_series_aligner = "ALIGN_MAX"
        }
      }
    }
  }
}
# An optional monthly cost budget over the installation's project, on the
# billing account that pays for it. Creating one needs a billing role on that
# account; see "Cost" in infra/README.md.
data "google_project" "budget" {
  count      = local.config.monitoring.monthly_budget > 0 ? 1 : 0
  project_id = local.config.project
}
resource "google_billing_budget" "monthly" {
  count           = local.config.monitoring.monthly_budget > 0 ? 1 : 0
  billing_account = local.config.monitoring.billing_account
  display_name    = "hot-${local.config.name}"
  budget_filter {
    projects = ["projects/${data.google_project.budget[0].number}"]
  }
  amount {
    specified_amount {
      units = tostring(local.config.monitoring.monthly_budget)
    }
  }
  threshold_rules {
    threshold_percent = 0.8
  }
  threshold_rules {
    threshold_percent = 1.0
    spend_basis       = "FORECASTED_SPEND"
  }
  all_updates_rule {
    # A budget notifies at most five channels.
    monitoring_notification_channels = slice(local.budget_channels, 0, min(5, length(local.budget_channels)))
  }
  depends_on = [google_project_service.required]
}

# The installation's metrics on one page.
locals {
  dashboard_aligners = { Maximum = "ALIGN_MAX", Minimum = "ALIGN_MIN", Average = "ALIGN_MEAN" }
  dashboard_reducers = { Maximum = "REDUCE_MAX", Minimum = "REDUCE_MIN", Average = "REDUCE_MEAN" }
}
resource "google_monitoring_dashboard" "hot" {
  dashboard_json = jsonencode({
    displayName = "hot-${local.config.name}"
    mosaicLayout = { columns = 12, tiles = [for index, panel in module.contract.dashboard : {
      xPos = (index % 2) * 6, yPos = floor(index / 2) * 4, width = 6, height = 4
      widget = { title = panel.title, xyChart = { dataSets = [for metric in panel.metrics : {
        plotType       = "LINE"
        legendTemplate = metric.gcp
        timeSeriesQuery = { timeSeriesFilter = {
          filter = "metric.type = \"${google_monitoring_metric_descriptor.hot[metric.gcp].type}\" resource.type = \"${contains(local.project_metrics, metric.gcp) ? "global" : "gce_instance"}\""
          aggregation = {
            alignmentPeriod    = "300s"
            perSeriesAligner   = local.dashboard_aligners[panel.stat]
            crossSeriesReducer = local.dashboard_reducers[panel.stat]
          }
        } }
      }] } }
    }] }
  })
}

# Connections to a managed database near the size's connection budget.
resource "google_monitoring_alert_policy" "database_connections" {
  count                 = local.managed_postgres ? 1 : 0
  display_name          = "hot-${local.config.name}-database-connections"
  combiner              = "OR"
  severity              = "WARNING"
  notification_channels = [for channel in google_monitoring_notification_channel.email : channel.id]
  documentation {
    content   = "PostgreSQL has had over 80% of the size's planned connections for 15 minutes. Check for connection leaks, then lower the scaling maximums or choose a larger size."
    mime_type = "text/markdown"
  }
  conditions {
    display_name = "Connections over 80% of the planned budget"
    condition_threshold {
      filter          = "resource.type = \"cloudsql_database\" AND resource.labels.database_id = \"${local.config.project}:${google_sql_database_instance.postgres[0].name}\" AND metric.type = \"cloudsql.googleapis.com/database/postgresql/num_backends\""
      comparison      = "COMPARISON_GT"
      threshold_value = floor(0.8 * module.contract.database_connections)
      duration        = "900s"
      aggregations {
        alignment_period     = "300s"
        per_series_aligner   = "ALIGN_MAX"
        cross_series_reducer = "REDUCE_SUM"
        group_by_fields      = ["resource.label.database_id"]
      }
    }
  }
}
