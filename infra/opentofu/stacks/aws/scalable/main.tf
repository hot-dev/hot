terraform {
  required_version = ">= 1.13.1, < 1.14.0"
  required_providers {
    aws    = { source = "hashicorp/aws", version = "6.69.1" }
    random = { source = "hashicorp/random", version = "3.9.1" }
  }
}
provider "aws" {
  region = var.installation.region
  default_tags {
    tags = { HotInstallation = var.installation.name, ManagedBy = "OpenTofu" }
  }
}
module "contract" {
  source       = "../../../modules/single-machine-contract"
  installation = var.installation
}
# The scalable preset on AWS: each Hot role in its own Auto Scaling group of
# the host image across two zones, following its own release pointer. The
# scheduler's single instance also migrates the database, from the migration
# pointer, before any role rolls. App and API each sit behind a passthrough
# Network Load Balancer (internal, or internet-facing when public), and TLS
# ends on the hosts' ingress with the operator's certificate.
locals {
  config             = module.contract.configuration
  new_network        = local.config.network.mode == "new"
  vpc_id             = local.new_network ? aws_vpc.hot[0].id : local.config.network.network_id
  roles              = module.contract.roles.groups
  web_roles          = toset(["app", "api"])
  exposure           = { app = local.config.app_exposure, api = local.config.api_exposure }
  s3_roles           = [for s in [local.config.storage.build, local.config.storage.file] : s if s.backend == "s3"]
  zones              = local.new_network ? [local.config.zone, local.second_zone] : []
  second_zone        = try([for zone in data.aws_availability_zones.available[0].names : zone if zone != local.config.zone][0], null)
  private_subnet_ids = local.new_network ? aws_subnet.private[*].id : local.config.network.private_subnet_ids
  public_subnet_ids  = local.new_network ? aws_subnet.public[*].id : local.config.network.public_subnet_ids
  # The hosts' subnets, where the internal load balancers' nodes, and so
  # their health checks, are too.
  private_subnet_cidrs = local.new_network ? aws_subnet.private[*].cidr_block : [for s in data.aws_subnet.private : s.cidr_block]
  private_zones        = local.new_network ? local.zones : distinct([for s in data.aws_subnet.private : s.availability_zone])
  public_zones         = local.new_network ? local.zones : distinct([for s in data.aws_subnet.public : s.availability_zone])
  host_config = { for role in keys(local.roles) : role => merge(local.config, {
    roles             = [role]
    migrations        = role == "scheduler"
    data_disk         = "scratch"
    db_pool           = module.contract.roles.db_pool
    instance_group    = "hot-${local.config.name}-${role}"
    release_pointer   = aws_ssm_parameter.pointer[role].arn
    migration_pointer = role == "scheduler" ? aws_ssm_parameter.pointer["migrate"].arn : ""
    status_reference  = aws_ssm_parameter.status.arn
    secret_references = { generated = aws_secretsmanager_secret.generated.arn, operator = aws_secretsmanager_secret.operator.arn }
    data              = local.host_data
  }) }
  # An idle task host leaves its group (see the agent's retire_if_idle):
  # it reads the group's size and terminates itself, shrinking the group.
  retire_task_hosts = local.roles["task-worker"].max > local.roles["task-worker"].min && local.config.scaling.task_worker.retire_idle_minutes > 0
  retire_statements = [
    { Effect = "Allow", Action = ["autoscaling:DescribeAutoScalingGroups"], Resource = "*" },
    { Effect = "Allow", Action = ["autoscaling:TerminateInstanceInAutoScalingGroup"],
    Resource = "arn:aws:autoscaling:${local.config.region}:*:autoScalingGroup:*:autoScalingGroupName/hot-${local.config.name}-task-worker" },
  ]
  # Pulling images from an ECR mirror with the host's own role.
  mirror_statements = module.contract.mirror.kind != "ecr" ? [] : [
    { Effect = "Allow", Action = ["ecr:GetAuthorizationToken"], Resource = "*" },
    { Effect = "Allow", Action = ["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer", "ecr:BatchCheckLayerAvailability"],
    Resource = "arn:aws:ecr:${module.contract.mirror.region}:${module.contract.mirror.account}:repository/${module.contract.mirror.repositories}" },
  ]
}

# -- Network: private hosts in two zones, a NAT gateway per zone ---------------

# Standard zones only, in every state: an opted-in Local Zone, or a zone that
# is impaired today, must not move the second zone and replace its subnets.
data "aws_availability_zones" "available" {
  count = local.new_network ? 1 : 0
  filter {
    name   = "zone-type"
    values = ["availability-zone"]
  }
}
# An existing network's subnets, for their zones and ranges.
data "aws_subnet" "private" {
  for_each = local.new_network ? toset([]) : toset(local.config.network.private_subnet_ids)
  id       = each.value
}
data "aws_subnet" "public" {
  for_each = local.new_network || !contains(values(local.exposure), "public") ? toset([]) : toset(local.config.network.public_subnet_ids)
  id       = each.value
}
resource "aws_vpc" "hot" {
  count                = local.new_network ? 1 : 0
  cidr_block           = local.config.network.cidr
  enable_dns_support   = true
  enable_dns_hostnames = true
}
resource "aws_subnet" "public" {
  count                   = local.new_network ? 2 : 0
  vpc_id                  = local.vpc_id
  availability_zone       = local.zones[count.index]
  cidr_block              = cidrsubnet(local.config.network.cidr, 8, count.index * 2)
  map_public_ip_on_launch = false
}
resource "aws_subnet" "private" {
  count                   = local.new_network ? 2 : 0
  vpc_id                  = local.vpc_id
  availability_zone       = local.zones[count.index]
  cidr_block              = cidrsubnet(local.config.network.cidr, 8, count.index * 2 + 1)
  map_public_ip_on_launch = false
}
resource "aws_internet_gateway" "egress" {
  count  = local.new_network ? 1 : 0
  vpc_id = local.vpc_id
}
resource "aws_eip" "egress" {
  count  = local.new_network ? 2 : 0
  domain = "vpc"
}
resource "aws_nat_gateway" "egress" {
  count         = local.new_network ? 2 : 0
  allocation_id = aws_eip.egress[count.index].id
  subnet_id     = aws_subnet.public[count.index].id
  depends_on    = [aws_internet_gateway.egress]
}
resource "aws_route_table" "public" {
  count  = local.new_network ? 1 : 0
  vpc_id = local.vpc_id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.egress[0].id
  }
}
resource "aws_route_table_association" "public" {
  count          = local.new_network ? 2 : 0
  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public[0].id
}
resource "aws_route_table" "private" {
  count  = local.new_network ? 2 : 0
  vpc_id = local.vpc_id
  route {
    cidr_block     = "0.0.0.0/0"
    nat_gateway_id = aws_nat_gateway.egress[count.index].id
  }
}
resource "aws_route_table_association" "private" {
  count          = local.new_network ? 2 : 0
  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private[count.index].id
}
resource "aws_vpc_endpoint" "s3" {
  count             = local.new_network ? 1 : 0
  vpc_id            = local.vpc_id
  service_name      = "com.amazonaws.${local.config.region}.s3"
  vpc_endpoint_type = "Gateway"
  route_table_ids   = aws_route_table.private[*].id
}
# Every host: outbound only. The database and cache admit this group.
resource "aws_security_group" "host" {
  name_prefix = "hot-${local.config.name}-"
  description = "Hot hosts: outbound only"
  vpc_id      = local.vpc_id
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}
# App and API hosts: HTTPS from the load balancer's health checks (from the
# hosts' subnets) and from clients, whose addresses the passthrough load
# balancer keeps: the client ranges for a private endpoint, anyone for a
# public one.
resource "aws_security_group" "web" {
  for_each    = local.web_roles
  name_prefix = "hot-${local.config.name}-${each.key}-"
  description = "HTTPS to Hot ${each.key} hosts"
  vpc_id      = local.vpc_id
  ingress {
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = local.exposure[each.key] == "public" ? ["0.0.0.0/0"] : distinct(concat(local.private_subnet_cidrs, local.config.network.client_cidrs))
  }
}

# -- Pointers, secrets, commands, logs -----------------------------------------

# A release pointer per role, and the migration pointer the scheduler follows.
# The release process owns their values.
resource "aws_ssm_parameter" "pointer" {
  for_each = toset(concat(keys(local.roles), ["migrate"]))
  name     = "/hot/${local.config.name}/release/${each.key}"
  type     = "String"
  value    = "none"
  lifecycle { ignore_changes = [value] }
}
resource "aws_ssm_parameter" "status" {
  name  = "/hot/${local.config.name}/status"
  type  = "String"
  value = "{}"
  lifecycle { ignore_changes = [value] }
}
resource "aws_secretsmanager_secret" "generated" {
  name                    = "hot/${local.config.name}/generated"
  recovery_window_in_days = 30
  lifecycle { prevent_destroy = true }
}
resource "aws_secretsmanager_secret" "operator" {
  name                    = "hot/${local.config.name}/operator"
  recovery_window_in_days = 30
}
# How Hot's workflows run anything on the hosts: a fixed set of agent commands,
# with every parameter constrained. No drain-and-stop: the app and API groups
# replace a host that fails the load balancer's health check, as a stopped one
# does, and the new host deploys and serves. Stopping Hot here means resizing
# its groups.
resource "aws_ssm_document" "command" {
  name            = "hot-${local.config.name}-command"
  document_type   = "Command"
  document_format = "JSON"
  content = jsonencode({
    schemaVersion = "2.2"
    description   = "Run one Hot agent command on an installation host."
    parameters = {
      operation = { type = "String", allowedValues = ["deploy-now", "status", "diagnostics", "logs", "restart-service", "test-alert"] }
      service   = { type = "String", default = "none", allowedValues = ["none", "app", "api", "worker", "task-worker", "scheduler", "ingress"] }
      requestId = { type = "String", allowedPattern = "^[A-Za-z0-9-]{1,64}$" }
      # When the helper stops waiting, in seconds since the epoch: past it the
      # agent reports `expired` instead of running the command. 0 is no expiry.
      expires = { type = "String", default = "0", allowedPattern = "^[0-9]{1,12}$" }
    }
    mainSteps = [{
      action = "aws:runShellScript"
      name   = "hotAgent"
      # The most Systems Manager allows (48 hours), not its 1-hour default:
      # a deploy is never stopped partway.
      inputs = { runCommand = ["/usr/local/bin/hot-agent command {{ operation }} --service {{ service }} --request-id {{ requestId }} --expires {{ expires }}"], timeoutSeconds = "172800" }
    }]
  })
}
resource "aws_cloudwatch_log_group" "host" {
  name              = "/hot/${local.config.name}"
  retention_in_days = local.config.logging.retention_days
}

# -- Identities: one role per Hot role -----------------------------------------

resource "aws_iam_role" "role" {
  for_each = local.roles
  name     = "hot-${local.config.name}-${each.key}"
  assume_role_policy = jsonencode({ Version = "2012-10-17", Statement = [{
    Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole"
  }] })
}
resource "aws_iam_role_policy_attachment" "ssm" {
  for_each   = aws_iam_role.role
  role       = each.value.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}
resource "aws_iam_role_policy" "runtime" {
  for_each = aws_iam_role.role
  role     = each.value.id
  policy = jsonencode({ Version = "2012-10-17", Statement = concat([
    # Only the scheduler's host signs in to the database as its administrator.
    { Effect = "Allow", Action = ["secretsmanager:GetSecretValue"], Resource = concat(
      [aws_secretsmanager_secret.generated.arn, aws_secretsmanager_secret.operator.arn], aws_secretsmanager_secret.valkey[*].arn,
    each.key == "scheduler" ? aws_db_instance.postgres[*].master_user_secret[0].secret_arn : []) },
    { Effect = "Allow", Action = ["ssm:GetParameter"], Resource = local.role_parameters[each.key] },
    { Effect = "Allow", Action = ["ssm:PutParameter"], Resource = aws_ssm_parameter.status.arn },
    { Effect = "Allow", Action = ["logs:CreateLogStream", "logs:PutLogEvents"], Resource = "${aws_cloudwatch_log_group.host.arn}:*" },
    { Effect = "Allow", Action = ["cloudwatch:PutMetricData"], Resource = "*", Condition = { StringEquals = { "cloudwatch:namespace" = "Hot/SelfHost" } } },
    # AmazonSSMManagedInstanceCore, which lets the SSM agent take commands and
    # carry port forwarding's data channels, also allows reading every
    # parameter. Each host reads only its own pointers. A host cannot tell a
    # shell from port forwarding on those channels, so who may start a session
    # is governed by ssm:StartSession on operator principals, limited to
    # AWS-StartPortForwardingSession.
    { Effect = "Deny", Action = ["ssm:GetParameter*"], NotResource = local.role_parameters[each.key] },
    ], each.key == "scheduler" ? [
    # The scheduler's host migrates first, so it creates the generated
    # secret; the other roles only read it.
    { Effect = "Allow", Action = ["secretsmanager:PutSecretValue"], Resource = aws_secretsmanager_secret.generated.arn },
    ] : [], local.mirror_statements, each.key == "task-worker" && local.retire_task_hosts ? local.retire_statements : [], [for s in local.s3_roles : {
      # Hot never lists a bucket, but S3 answers a read of a missing key with 403
      # rather than 404 to a caller without ListBucket. Whether this grant, limited
      # by prefix, counts for that is on the qualification list.
      Effect    = "Allow", Action = ["s3:ListBucket"], Resource = "arn:aws:s3:::${s.bucket}",
      Condition = { StringLike = { "s3:prefix" = [trim(s.prefix, "/"), "${trim(s.prefix, "/")}/*"] } }
      }], [for s in local.s3_roles : {
      Effect = "Allow", Action = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:AbortMultipartUpload"], Resource = "arn:aws:s3:::${s.bucket}/${trim(s.prefix, "/")}/*"
  }]) })
}
locals {
  # The pointers each role's hosts read: their own, and the scheduler's the
  # migration pointer too.
  role_parameters = { for role in keys(local.roles) : role => concat([aws_ssm_parameter.pointer[role].arn],
  role == "scheduler" ? [aws_ssm_parameter.pointer["migrate"].arn] : []) }
}
resource "aws_iam_instance_profile" "role" {
  for_each = aws_iam_role.role
  name     = each.value.name
  role     = each.value.name
}
# The release and command workflows may write and read the pointers, send the
# Hot command to this installation's hosts, read the agents' reports, silence
# this installation's alarms, and list each role's instances.
resource "aws_iam_role_policy" "release" {
  count = var.release_principal == "" ? 0 : 1
  name  = "hot-${local.config.name}-release"
  role  = element(split("/", var.release_principal), length(split("/", var.release_principal)) - 1)
  policy = jsonencode({ Version = "2012-10-17", Statement = [
    { Effect = "Allow", Action = ["ssm:PutParameter", "ssm:GetParameter"], Resource = [for p in aws_ssm_parameter.pointer : p.arn] },
    # selfhost.py sends by tag, and IAM authorizes that against instance/* and
    # the instance's tag, not against each instance's own ARN.
    { Effect = "Allow", Action = ["ssm:SendCommand"], Resource = aws_ssm_document.command.arn },
    { Effect = "Allow", Action = ["ssm:SendCommand"], Resource = "arn:aws:ec2:${local.config.region}:*:instance/*",
    Condition = { StringEquals = { "ssm:resourceTag/HotInstallation" = local.config.name } } },
    { Effect = "Allow", Action = ["logs:FilterLogEvents"], Resource = [aws_cloudwatch_log_group.host.arn, "${aws_cloudwatch_log_group.host.arn}:*"] },
    # selfhost.py maintenance: silence and restore this installation's alarms.
    # IAM authorizes a listing by name prefix against alarm:*, not against each
    # alarm; only the changes can be limited to this installation's names.
    { Effect = "Allow", Action = ["cloudwatch:DescribeAlarms"], Resource = "arn:aws:cloudwatch:${local.config.region}:*:alarm:*" },
    { Effect = "Allow", Action = ["cloudwatch:DisableAlarmActions", "cloudwatch:EnableAlarmActions"], Resource = "arn:aws:cloudwatch:${local.config.region}:*:alarm:hot-${local.config.name}-*" },
    # Instance listings cannot be scoped by resource.
    { Effect = "Allow", Action = ["autoscaling:DescribeAutoScalingGroups"], Resource = "*" },
  ] })
}

# -- Instances -----------------------------------------------------------------

resource "aws_launch_template" "role" {
  for_each               = local.roles
  name_prefix            = "hot-${local.config.name}-${each.key}-"
  image_id               = local.config.host_image
  instance_type          = each.value.aws
  update_default_version = true
  vpc_security_group_ids = concat([aws_security_group.host.id], contains(local.web_roles, each.key) ? [aws_security_group.web[each.key].id] : [])
  iam_instance_profile {
    name = aws_iam_instance_profile.role[each.key].name
  }
  metadata_options {
    http_tokens                 = "required"
    http_put_response_hop_limit = 2
    instance_metadata_tags      = "disabled"
  }
  # Only task-workers run box tasks in Kata VMs.
  dynamic "cpu_options" {
    for_each = each.key == "task-worker" ? [1] : []
    content {
      nested_virtualization = "enabled"
    }
  }
  block_device_mappings {
    device_name = "/dev/sda1"
    ebs {
      volume_size           = 40
      volume_type           = "gp3"
      encrypted             = true
      delete_on_termination = true
    }
  }
  # Scratch: releases, logs, and box storage. Nothing on it outlives the
  # instance; data lives in the managed services and buckets.
  block_device_mappings {
    device_name = "/dev/sdf"
    ebs {
      volume_size           = each.key == "task-worker" ? module.contract.capacity.data_gb : 20
      volume_type           = "gp3"
      encrypted             = true
      delete_on_termination = true
    }
  }
  user_data = base64encode(templatefile("${path.module}/../../../../runtime/bootstrap.sh.tftpl", {
    config_base64 = base64encode(jsonencode(local.host_config[each.key]))
  }))
  # The provider's default tags do not reach instances launched from a
  # template, so the instance and its volumes name the installation here:
  # commands, the release role, and the budget find them by it.
  dynamic "tag_specifications" {
    for_each = ["instance", "volume"]
    content {
      resource_type = tag_specifications.value
      tags          = { Name = "hot-${local.config.name}-${each.key}", HotRole = each.key, HotInstallation = local.config.name }
    }
  }
  # New hosts boot with this configuration: what it names exists first.
  depends_on = [aws_route_table_association.private, aws_iam_role_policy.runtime, aws_iam_role_policy_attachment.ssm, aws_s3_bucket_policy.storage, aws_secretsmanager_secret_version.valkey]
}
resource "aws_autoscaling_group" "role" {
  for_each            = local.roles
  name                = "hot-${local.config.name}-${each.key}"
  min_size            = each.value.min
  max_size            = each.value.max
  desired_capacity    = each.value.min
  vpc_zone_identifier = local.private_subnet_ids
  health_check_type   = contains(local.web_roles, each.key) ? "ELB" : "EC2"
  # Hosts pull their release on boot; give them time before health counts.
  health_check_grace_period = 600
  target_group_arns         = contains(local.web_roles, each.key) ? [aws_lb_target_group.web[each.key].arn] : []
  # Autoscaled task-workers are protected from scale-in while their tasks run,
  # and idle ones leave on their own. A fixed count has no scale-in to guard
  # against and its hosts never retire, so a lower count needs the group to
  # remove them: for protected hosts it only lowers the desired capacity.
  protect_from_scale_in = each.key == "task-worker" && each.value.max > each.value.min
  launch_template {
    id      = aws_launch_template.role[each.key].id
    version = aws_launch_template.role[each.key].latest_version
  }
  # A new host image or instance type replaces instances; releases roll in
  # place through the pointers. The scheduler is replaced stop-before-start,
  # so two schedulers never run. Task-workers are replaced too, despite any
  # scale-in protection; a task cut off goes back to the queue.
  instance_refresh {
    strategy = "Rolling"
    preferences {
      min_healthy_percentage       = each.key == "scheduler" ? 0 : 100
      max_healthy_percentage       = each.key == "scheduler" ? 100 : 200
      instance_warmup              = 600
      scale_in_protected_instances = "Refresh"
    }
  }
  tag {
    key                 = "HotRole"
    value               = each.key
    propagate_at_launch = true
  }
  # Queue-driven scaling divides the jobs waiting by the hosts in service.
  enabled_metrics = each.key == "worker" && each.value.max > each.value.min ? ["GroupInServiceInstances"] : []
  lifecycle { ignore_changes = [desired_capacity] }
}
# App and API follow CPU. Workers and task-workers keep fixed counts unless a
# maximum above the minimum is set; they then only scale out, because no
# drain stops a host's new claims and lets its work finish before the group
# removes it.
resource "aws_autoscaling_policy" "cpu" {
  for_each               = { for role, group in local.roles : role => group if role != "scheduler" && (contains(local.web_roles, role) || group.max > group.min) }
  name                   = "hot-${local.config.name}-${each.key}-cpu"
  autoscaling_group_name = aws_autoscaling_group.role[each.key].name
  policy_type            = "TargetTrackingScaling"
  target_tracking_configuration {
    target_value     = 60
    disable_scale_in = !contains(local.web_roles, each.key)
    predefined_metric_specification {
      predefined_metric_type = "ASGAverageCPUUtilization"
    }
  }
}
# Workers also follow the jobs waiting in Hot's queue, per worker host, which
# the scheduler's host measures every minute.
resource "aws_autoscaling_policy" "jobs" {
  count                  = local.roles.worker.max > local.roles.worker.min ? 1 : 0
  name                   = "hot-${local.config.name}-worker-jobs"
  autoscaling_group_name = aws_autoscaling_group.role["worker"].name
  policy_type            = "TargetTrackingScaling"
  target_tracking_configuration {
    target_value     = local.config.scaling.worker.waiting_per_host
    disable_scale_in = true
    customized_metric_specification {
      metrics {
        id          = "waiting"
        return_data = false
        metric_stat {
          stat = "Maximum"
          metric {
            namespace   = "Hot/SelfHost"
            metric_name = "JobsWaiting"
            dimensions {
              name  = "Installation"
              value = local.config.name
            }
          }
        }
      }
      metrics {
        id          = "hosts"
        return_data = false
        metric_stat {
          stat = "Average"
          metric {
            namespace   = "AWS/AutoScaling"
            metric_name = "GroupInServiceInstances"
            dimensions {
              name  = "AutoScalingGroupName"
              value = aws_autoscaling_group.role["worker"].name
            }
          }
        }
      }
      metrics {
        id          = "per_host"
        label       = "Jobs waiting per worker host"
        expression  = "waiting / hosts"
        return_data = true
      }
    }
  }
}
# Task-workers scale out on the share of their slots in use, averaged over
# the group's hosts. They never scale in through the group, which cannot
# tell which hosts run tasks; idle hosts leave on their own.
resource "aws_autoscaling_policy" "task_slots" {
  count                  = local.roles["task-worker"].max > local.roles["task-worker"].min ? 1 : 0
  name                   = "hot-${local.config.name}-task-worker-slots"
  autoscaling_group_name = aws_autoscaling_group.role["task-worker"].name
  policy_type            = "TargetTrackingScaling"
  target_tracking_configuration {
    target_value     = local.config.scaling.task_worker.slots_target
    disable_scale_in = true
    customized_metric_specification {
      namespace   = "Hot/SelfHost"
      metric_name = "TaskSlotsUsedPercent"
      statistic   = "Average"
      metric_dimension {
        name  = "Installation"
        value = local.config.name
      }
    }
  }
}

output "egress_ips" { value = aws_eip.egress[*].public_ip }
output "configuration" { value = local.host_config }
output "release_parameters" { value = { for role, p in aws_ssm_parameter.pointer : role => p.name } }
output "operator_secret_arn" { value = aws_secretsmanager_secret.operator.arn }
output "command_document" { value = aws_ssm_document.command.name }
output "groups" { value = { for role, group in aws_autoscaling_group.role : role => group.name } }
