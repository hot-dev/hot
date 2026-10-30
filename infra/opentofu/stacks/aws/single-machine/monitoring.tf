# Backups and alerts for the single machine. Data Lifecycle Manager snapshots
# the retained data disk daily; the host cannot delete the snapshots. The
# host agent publishes health metrics to CloudWatch (namespace Hot/SelfHost),
# and these alarms notify an SNS topic, independently of Hot's own email.
resource "aws_iam_role" "snapshots" {
  name = "hot-${local.config.name}-snapshots"
  assume_role_policy = jsonencode({ Version = "2012-10-17", Statement = [{
    Effect = "Allow", Principal = { Service = "dlm.amazonaws.com" }, Action = "sts:AssumeRole"
  }] })
}
resource "aws_iam_role_policy_attachment" "snapshots" {
  role       = aws_iam_role.snapshots.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSDataLifecycleManagerServiceRole"
}
resource "aws_dlm_lifecycle_policy" "data" {
  description        = "Hot ${local.config.name} data disk"
  execution_role_arn = aws_iam_role.snapshots.arn
  state              = "ENABLED"
  policy_details {
    resource_types = ["VOLUME"]
    target_tags    = { HotDataVolume = local.config.name }
    schedule {
      name = "daily"
      create_rule {
        interval      = 24
        interval_unit = "HOURS"
        times         = [local.config.backup.time_utc]
      }
      retain_rule {
        interval      = local.config.backup.retention_days
        interval_unit = "DAYS"
      }
      copy_tags = true
    }
  }
}
# Alert email is not secret, so the topic uses no customer-managed key;
# CloudWatch cannot publish to a topic encrypted with the AWS-managed SNS key.
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
  # Thresholds and sustained windows; see the runbook in infra/README.md.
  alarms = {
    host-down = {
      metric      = "AgentHeartbeat", statistic = "Sum", comparison = "LessThanThreshold", threshold = 1,
      period      = 300, periods = 1, missing = "breaching",
      description = "The host agent stopped reporting: the VM, Docker, or the agent is down."
    }
    services-unhealthy = {
      metric      = "UnhealthyServices", statistic = "Minimum", comparison = "GreaterThanThreshold", threshold = 0,
      period      = 300, periods = 3, missing = "notBreaching",
      description = "A Hot, ingress, or local data service has not been running and healthy for 15 minutes."
    }
    scheduler-stopped = {
      metric      = "SchedulerRunning", statistic = "Maximum", comparison = "LessThanThreshold", threshold = 1,
      period      = 300, periods = 2, missing = "notBreaching",
      description = "The scheduler has not been running for 10 minutes: schedules are not firing."
    }
    dependencies-unreachable = {
      metric      = "DependenciesReachable", statistic = "Maximum", comparison = "LessThanThreshold", threshold = 1,
      period      = 300, periods = 2, missing = "notBreaching",
      description = "PostgreSQL or Valkey has been unreachable from the host for 10 minutes."
    }
    not-ready = {
      metric      = "Ready", statistic = "Maximum", comparison = "LessThanThreshold", threshold = 1,
      period      = 300, periods = 3, missing = "notBreaching",
      description = "The app or API has not answered through the ingress for 15 minutes."
    }
    disk-pressure = {
      metric      = "DataDiskUsedPercent", statistic = "Maximum", comparison = "GreaterThanThreshold", threshold = 85,
      period      = 300, periods = 3, missing = "notBreaching",
      description = "The data disk is over 85% full (space or inodes)."
    }
    backup-stale = {
      metric      = "BackupAgeHours", statistic = "Minimum", comparison = "GreaterThanThreshold", threshold = 30,
      period      = 3600, periods = 1, missing = "breaching",
      description = "No data-disk snapshot has completed in 30 hours, or the agent cannot tell."
    }
    certificate-expiring = {
      metric      = "CertificateDaysLeft", statistic = "Minimum", comparison = "LessThanThreshold", threshold = 14,
      period      = 3600, periods = 1, missing = "notBreaching",
      description = "The ingress TLS certificate expires within 14 days, or the agent cannot read it."
    }
    queue-backlog = {
      metric      = "QueueBacklog", statistic = "Minimum", comparison = "GreaterThanThreshold", threshold = local.config.monitoring.queue_backlog_alert,
      period      = 300, periods = 3, missing = "notBreaching",
      description = "More jobs than monitoring.queue_backlog_alert have been waiting or running in Hot's queue for 15 minutes: workers are not keeping up."
    }
    new-dead-letters = {
      metric      = "NewDeadLetters", statistic = "Maximum", comparison = "GreaterThanThreshold", threshold = 0,
      period      = 300, periods = 1, missing = "notBreaching",
      description = "Jobs failed every attempt and were dead-lettered; the worker's logs say why."
    }
    synthetic-failed = {
      metric      = "SyntheticOk", statistic = "Minimum", comparison = "LessThanThreshold", threshold = 1,
      period      = 300, periods = 2, missing = "notBreaching",
      description = "The synthetic event's run has not succeeded for 10 minutes: the API, queue, workers, or database are failing end to end."
    }
    alert-test = {
      metric      = "AlertTest", statistic = "Maximum", comparison = "GreaterThanThreshold", threshold = 0,
      period      = 60, periods = 1, missing = "notBreaching",
      description = "A test alert sent with the test-alert command; it resolves by itself. Nothing is wrong."
    }
    job-wait = {
      metric      = "JobWaitSeconds", statistic = "Minimum", comparison = "GreaterThanThreshold", threshold = local.config.monitoring.queue_wait_alert,
      period      = 300, periods = 2, missing = "notBreaching",
      description = "A job has waited longer than monitoring.queue_wait_alert seconds for a worker, for 10 minutes: workers are not keeping up."
    }
    task-wait = {
      metric      = "TaskWaitSeconds", statistic = "Minimum", comparison = "GreaterThanThreshold", threshold = local.config.monitoring.queue_wait_alert,
      period      = 300, periods = 2, missing = "notBreaching",
      description = "A task has waited longer than monitoring.queue_wait_alert seconds for a task worker, for 10 minutes: task capacity is short."
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
resource "aws_cloudwatch_metric_alarm" "instance_impaired" {
  alarm_name          = "hot-${local.config.name}-instance-impaired"
  alarm_description   = "EC2 status checks are failing for the Hot host."
  namespace           = "AWS/EC2"
  metric_name         = "StatusCheckFailed"
  dimensions          = { InstanceId = aws_instance.host.id }
  statistic           = "Maximum"
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 1
  period              = 60
  evaluation_periods  = 5
  treat_missing_data  = "breaching"
  alarm_actions       = [aws_sns_topic.alerts.arn]
  ok_actions          = [aws_sns_topic.alerts.arn]
}
output "alert_topic_arn" { value = aws_sns_topic.alerts.arn }
# An optional monthly cost budget over everything tagged with this
# installation. AWS counts the tag only once HotInstallation is activated as a
# cost allocation tag in the billing console.
resource "aws_budgets_budget" "monthly" {
  count        = local.config.monitoring.monthly_budget > 0 ? 1 : 0
  name         = "hot-${local.config.name}"
  budget_type  = "COST"
  limit_amount = tostring(local.config.monitoring.monthly_budget)
  limit_unit   = "USD"
  time_unit    = "MONTHLY"
  cost_filter {
    name   = "TagKeyValue"
    values = [format("user:HotInstallation$%s", local.config.name)]
  }
  dynamic "notification" {
    for_each = length(local.config.monitoring.alert_emails) > 0 ? { ACTUAL = 80, FORECASTED = 100 } : {}
    content {
      notification_type          = notification.key
      comparison_operator        = "GREATER_THAN"
      threshold                  = notification.value
      threshold_type             = "PERCENTAGE"
      subscriber_email_addresses = local.config.monitoring.alert_emails
    }
  }
}

# The installation's metrics on one page.
resource "aws_cloudwatch_dashboard" "hot" {
  dashboard_name = "hot-${local.config.name}"
  dashboard_body = jsonencode({ widgets = [for index, panel in module.contract.dashboard : {
    type = "metric", x = (index % 2) * 12, y = floor(index / 2) * 6, width = 12, height = 6
    properties = {
      title   = panel.title, region = local.config.region, stat = panel.stat, period = 300, view = "timeSeries"
      metrics = [for metric in panel.metrics : ["Hot/SelfHost", metric.aws, "Installation", local.config.name]]
    }
  }] })
}
