# Alerts for the scalable preset. Every host's agent publishes the same
# metrics as on a single machine, under the installation's dimension, so each
# alarm reads the worst host: the maximum of unhealthy services, the minimum
# of readiness. Hosts hold no data, so there are no disk snapshots; the
# managed database and buckets keep their own backups.
resource "aws_sns_topic" "alerts" {
  name = "hot-${local.config.name}-alerts"
}
resource "aws_sns_topic_subscription" "alerts" {
  for_each  = toset(local.config.monitoring.alert_emails)
  topic_arn = aws_sns_topic.alerts.arn
  protocol  = "email"
  endpoint  = each.value
}
locals {
  alarms = {
    hosts-down = {
      metric      = "AgentHeartbeat", statistic = "Sum", comparison = "LessThanThreshold", threshold = 1,
      period      = 300, periods = 1, missing = "breaching",
      description = "No host agent has reported for 5 minutes. Single hosts that stop are replaced by their groups."
    }
    services-unhealthy = {
      metric      = "UnhealthyServices", statistic = "Maximum", comparison = "GreaterThanThreshold", threshold = 0,
      period      = 300, periods = 3, missing = "notBreaching",
      description = "A Hot service or ingress on one host has not been running and healthy for 15 minutes."
    }
    scheduler-stopped = {
      metric      = "SchedulerRunning", statistic = "Maximum", comparison = "LessThanThreshold", threshold = 1,
      period      = 300, periods = 2, missing = "breaching",
      description = "The scheduler has not dispatched for 10 minutes, or its host stopped reporting: schedules are not firing."
    }
    dependencies-unreachable = {
      metric      = "DependenciesReachable", statistic = "Minimum", comparison = "LessThanThreshold", threshold = 1,
      period      = 300, periods = 2, missing = "notBreaching",
      description = "A host has not reached PostgreSQL or Valkey for 10 minutes."
    }
    not-ready = {
      metric      = "Ready", statistic = "Minimum", comparison = "LessThanThreshold", threshold = 1,
      period      = 300, periods = 3, missing = "notBreaching",
      description = "An app or API host has not answered through its ingress for 15 minutes; its group replaces it after failed health checks."
    }
    disk-pressure = {
      metric      = "DataDiskUsedPercent", statistic = "Maximum", comparison = "GreaterThanThreshold", threshold = 85,
      period      = 300, periods = 3, missing = "notBreaching",
      description = "A host's scratch disk is over 85% full."
    }
    certificate-expiring = {
      metric      = "CertificateDaysLeft", statistic = "Minimum", comparison = "LessThanThreshold", threshold = 14,
      period      = 3600, periods = 1, missing = "notBreaching",
      description = "The ingress TLS certificate expires within 14 days, or a host cannot read it."
    }
  }
}
resource "aws_cloudwatch_metric_alarm" "hot" {
  for_each            = local.alarms
  alarm_name          = "hot-${local.config.name}-${each.key}"
  alarm_description   = each.value.description
  namespace           = "Hot/SelfHost"
  metric_name         = each.value.metric
  dimensions          = { Installation = local.config.name }
  statistic           = each.value.statistic
  comparison_operator = each.value.comparison
  threshold           = each.value.threshold
  period              = each.value.period
  evaluation_periods  = each.value.periods
  treat_missing_data  = each.value.missing
  alarm_actions       = [aws_sns_topic.alerts.arn]
  ok_actions          = [aws_sns_topic.alerts.arn]
}
