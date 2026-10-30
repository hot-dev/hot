# Alerts for the scalable preset. Every host's agent publishes the same
# metrics as on a single machine, one series per instance, so a condition
# fires when any instance breaches it. Hosts hold no data, so there are no
# disk snapshots; the managed database and buckets keep their own backups.
locals {
  # Metric types are per installation: descriptors are project-wide, and
  # deleting one deletes its data, so installations never share one.
  metric_prefix = "custom.googleapis.com/hot/${local.config.name}/"
  metrics = {
    agent_heartbeat         = { unit = "1", name = "Agent heartbeat" }
    unhealthy_services      = { unit = "1", name = "Unhealthy services" }
    scheduler_running       = { unit = "1", name = "Scheduler running" }
    dependencies_reachable  = { unit = "1", name = "Dependencies reachable" }
    ready                   = { unit = "1", name = "App and API ready" }
    data_disk_used_percent  = { unit = "%", name = "Scratch disk used" }
    certificate_days_left   = { unit = "d", name = "Certificate days left" }
    queue_backlog           = { unit = "1", name = "Queue backlog" }
    queue_dead_letters      = { unit = "1", name = "Dead-lettered jobs" }
    new_dead_letters        = { unit = "1", name = "New dead-lettered jobs" }
    jobs_waiting            = { unit = "1", name = "Jobs waiting" }
    job_wait_seconds        = { unit = "s", name = "Oldest job's wait" }
    tasks_waiting           = { unit = "1", name = "Tasks waiting" }
    task_wait_seconds       = { unit = "s", name = "Oldest task's wait" }
    task_slots_used_percent = { unit = "%", name = "Task slots used" }
  }
  # Installation-wide queue measurements, written for the project by the
  # scheduler's host rather than per instance (see the agent's
  # QUEUE_METRICS), so the worker group can scale on them.
  project_metrics = ["queue_backlog", "queue_dead_letters", "new_dead_letters", "jobs_waiting", "job_wait_seconds", "tasks_waiting", "task_wait_seconds"]
  alerts = {
    services-unhealthy = {
      metric      = "unhealthy_services", comparison = "COMPARISON_GT", threshold = 0, aligner = "ALIGN_MIN", duration = "900s",
      description = "A Hot service or ingress on one host has not been running and healthy for 15 minutes."
    }
    dependencies-unreachable = {
      metric      = "dependencies_reachable", comparison = "COMPARISON_LT", threshold = 1, aligner = "ALIGN_MAX", duration = "600s",
      description = "A host has not reached PostgreSQL or Valkey for 10 minutes."
    }
    not-ready = {
      metric      = "ready", comparison = "COMPARISON_LT", threshold = 1, aligner = "ALIGN_MAX", duration = "900s",
      description = "An app or API host has not answered through its ingress for 15 minutes; its group replaces it after failed health checks."
    }
    disk-pressure = {
      metric      = "data_disk_used_percent", comparison = "COMPARISON_GT", threshold = 85, aligner = "ALIGN_MAX", duration = "900s",
      description = "A host's scratch disk is over 85% full."
    }
    certificate-expiring = {
      metric      = "certificate_days_left", comparison = "COMPARISON_LT", threshold = 14, aligner = "ALIGN_MIN", duration = "3600s",
      description = "The ingress TLS certificate expires within 14 days, or a host cannot read it."
    }
    queue-backlog = {
      metric      = "queue_backlog", comparison = "COMPARISON_GT", threshold = local.config.monitoring.queue_backlog_alert, aligner = "ALIGN_MIN", duration = "900s",
      description = "More jobs than monitoring.queue_backlog_alert have been waiting or running in Hot's queue for 15 minutes: workers are not keeping up."
    }
    new-dead-letters = {
      metric      = "new_dead_letters", comparison = "COMPARISON_GT", threshold = 0, aligner = "ALIGN_MAX", duration = "0s",
      description = "Jobs failed every attempt and were dead-lettered; the worker's logs say why."
    }
    job-wait = {
      metric      = "job_wait_seconds", comparison = "COMPARISON_GT", threshold = local.config.monitoring.queue_wait_alert, aligner = "ALIGN_MIN", duration = "600s",
      description = "A job has waited longer than monitoring.queue_wait_alert seconds for a worker, for 10 minutes: workers are not keeping up."
    }
    task-wait = {
      metric      = "task_wait_seconds", comparison = "COMPARISON_GT", threshold = local.config.monitoring.queue_wait_alert, aligner = "ALIGN_MIN", duration = "600s",
      description = "A task has waited longer than monitoring.queue_wait_alert seconds for a task worker, for 10 minutes: task capacity is short."
    }
  }
  channels = [for channel in google_monitoring_notification_channel.email : channel.id]
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
# Every agent silent: the whole installation is down. Single instances that
# stop are replaced by their groups, and scale-in ends series on purpose.
resource "google_monitoring_alert_policy" "hosts_down" {
  display_name          = "hot-${local.config.name}-hosts-down"
  combiner              = "OR"
  severity              = "CRITICAL"
  notification_channels = local.channels
  documentation {
    content   = "No host agent has reported for 5 minutes."
    mime_type = "text/markdown"
  }
  conditions {
    display_name = "No agent heartbeat from any host for 5 minutes"
    condition_absent {
      filter   = "resource.type = \"gce_instance\" AND metric.type = \"${google_monitoring_metric_descriptor.hot["agent_heartbeat"].type}\""
      duration = "300s"
      aggregations {
        alignment_period     = "60s"
        per_series_aligner   = "ALIGN_MAX"
        cross_series_reducer = "REDUCE_COUNT"
      }
    }
  }
}
# The singleton scheduler: stopped, stalled, or gone.
resource "google_monitoring_alert_policy" "scheduler" {
  display_name          = "hot-${local.config.name}-scheduler-stopped"
  combiner              = "OR"
  severity              = "CRITICAL"
  notification_channels = local.channels
  documentation {
    content   = "The scheduler has not dispatched for 10 minutes, or its host stopped reporting: schedules are not firing."
    mime_type = "text/markdown"
  }
  conditions {
    display_name = "Scheduler not running"
    condition_threshold {
      filter          = "resource.type = \"gce_instance\" AND metric.type = \"${google_monitoring_metric_descriptor.hot["scheduler_running"].type}\""
      comparison      = "COMPARISON_LT"
      threshold_value = 1
      duration        = "600s"
      aggregations {
        alignment_period     = "300s"
        per_series_aligner   = "ALIGN_MAX"
        cross_series_reducer = "REDUCE_MAX"
      }
    }
  }
  conditions {
    display_name = "No scheduler status reported"
    condition_absent {
      filter   = "resource.type = \"gce_instance\" AND metric.type = \"${google_monitoring_metric_descriptor.hot["scheduler_running"].type}\""
      duration = "600s"
      aggregations {
        alignment_period     = "300s"
        per_series_aligner   = "ALIGN_MAX"
        cross_series_reducer = "REDUCE_MAX"
      }
    }
  }
}
resource "google_monitoring_alert_policy" "hot" {
  for_each              = local.alerts
  display_name          = "hot-${local.config.name}-${each.key}"
  combiner              = "OR"
  severity              = "CRITICAL"
  notification_channels = local.channels
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
    monitoring_notification_channels = slice(local.channels, 0, min(5, length(local.channels)))
  }
  depends_on = [google_project_service.required]
}
