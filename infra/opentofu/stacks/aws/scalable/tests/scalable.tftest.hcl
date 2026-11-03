mock_provider "aws" {
  mock_data "aws_subnet" {
    defaults = { availability_zone = "us-east-1a", cidr_block = "172.31.0.0/20" }
  }
  mock_resource "aws_launch_template" {
    defaults = { id = "lt-0123456789abcdef0", latest_version = 1 }
  }
  mock_data "aws_availability_zones" {
    defaults = { names = ["us-east-1a", "us-east-1b"] }
  }
  mock_resource "aws_lb" {
    defaults = { arn = "arn:aws:elasticloadbalancing:us-east-1:123456789012:loadbalancer/app/hot-example/0123" }
  }
  mock_resource "aws_lb_target_group" {
    defaults = { arn = "arn:aws:elasticloadbalancing:us-east-1:123456789012:targetgroup/hot-example/0123" }
  }
  mock_resource "aws_lb_listener" {
    defaults = { arn = "arn:aws:elasticloadbalancing:us-east-1:123456789012:listener/app/hot-example/0123/4567" }
  }
  mock_resource "aws_iam_role" {
    defaults = { arn = "arn:aws:iam::123456789012:role/hot-example" }
  }
  mock_resource "aws_sns_topic" {
    defaults = { arn = "arn:aws:sns:us-east-1:123456789012:hot-example-alerts" }
  }
  mock_resource "aws_elasticache_replication_group" {
    defaults = { primary_endpoint_address = "master.hot-example.abc123.use1.cache.amazonaws.com" }
  }
  mock_resource "aws_secretsmanager_secret" {
    defaults = { arn = "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/secret-AbCdEf" }
  }
  mock_resource "aws_db_instance" {
    defaults = {
      address            = "hot-example.c0123.us-east-1.rds.amazonaws.com"
      port               = 5432
      master_user_secret = [{ secret_arn = "arn:aws:secretsmanager:us-east-1:123456789012:secret:rds!db-0123-abcdef", kms_key_id = "", secret_status = "active" }]
    }
  }
}
variables {
  acknowledge_unqualified     = true
  state_encryption_passphrase = "test-only-encryption-passphrase-never-use-in-production"
  release_principal           = "arn:aws:iam::123456789012:role/hot-github-release"
  installation = {
    "schema_version" : 1,
    "preset" : "scalable",
    "name" : "hot-example",
    "cloud" : "aws",
    "region" : "us-east-1",
    "zone" : "us-east-1a",
    "size" : "small",
    "host_image" : "ami-0123456789abcdef0",
    "app_hostname" : "app.hot.internal",
    "api_hostname" : "api.hot.internal",
    "app_exposure" : "public",
    "network" : { "client_cidrs" : ["10.10.0.0/16"] },
    "data" : { "postgres" : { "mode" : "managed", "high_availability" : true }, "valkey" : { "mode" : "managed", "high_availability" : true } },
    "storage" : {
      "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds", "provision" : true },
      "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files", "provision" : true }
    },
    "scaling" : { "task_worker" : { "min" : 1, "max" : 3 } }
  }
}

run "each_role_runs_in_its_own_group" {
  command = plan
  assert {
    condition     = length(aws_autoscaling_group.role) == 5 && aws_autoscaling_group.role["scheduler"].max_size == 1 && aws_autoscaling_group.role["app"].min_size == 2
    error_message = "A group per role: the scheduler exactly one, the app and API from two."
  }
  assert {
    condition     = aws_autoscaling_group.role["scheduler"].instance_refresh[0].preferences[0].min_healthy_percentage == 0 && aws_autoscaling_group.role["scheduler"].instance_refresh[0].preferences[0].max_healthy_percentage == 100
    error_message = "The scheduler is replaced stop-before-start, so two never run at once."
  }
  assert {
    condition     = aws_autoscaling_group.role["task-worker"].protect_from_scale_in && aws_autoscaling_policy.cpu["task-worker"].target_tracking_configuration[0].disable_scale_in
    error_message = "Autoscaled task-workers are protected while their tasks run and only scale out."
  }
  assert {
    condition     = alltrue([for group in aws_autoscaling_group.role : group.instance_refresh[0].preferences[0].scale_in_protected_instances == "Refresh"])
    error_message = "A rolling refresh replaces protected task-workers too, so they get a new host image and configuration."
  }
  assert {
    condition     = length(aws_launch_template.role["task-worker"].cpu_options) == 1 && length(aws_launch_template.role["app"].cpu_options) == 0
    error_message = "Only task-workers run Kata VMs."
  }
  assert {
    condition     = aws_launch_template.role["task-worker"].instance_type == "m8i.large" && aws_launch_template.role["worker"].instance_type == "m7i.large"
    error_message = "Task-workers use the nested family; other roles general-purpose instances."
  }
  assert {
    condition     = aws_launch_template.role["app"].metadata_options[0].http_tokens == "required" && length(aws_nat_gateway.egress) == 2 && length(aws_subnet.private) == 2
    error_message = "IMDSv2 only, and private hosts in two zones with a NAT gateway each."
  }
  assert {
    condition     = data.aws_availability_zones.available[0].state == null && one(data.aws_availability_zones.available[0].filter).name == "zone-type" && contains(tolist(one(data.aws_availability_zones.available[0].filter).values), "availability-zone")
    error_message = "The second zone is a standard zone, chosen the same way whatever state the zones are in."
  }
  assert {
    condition     = alltrue([for spec in aws_launch_template.role["worker"].tag_specifications : spec.tags["HotInstallation"] == "hot-example"]) && length(aws_launch_template.role["worker"].tag_specifications) == 2
    error_message = "Instances and their volumes carry the installation tag that commands and the budget select by."
  }
}

run "hosts_know_their_role_and_pointers" {
  command = plan
  assert {
    condition     = local.host_config["api"].roles == ["api"] && !local.host_config["api"].migrations && local.host_config["api"].data_disk == "scratch"
    error_message = "An API host runs only the API, never migrates, and has a scratch disk."
  }
  assert {
    condition     = local.host_config["scheduler"].migrations && local.host_config["scheduler"].migration_pointer == aws_ssm_parameter.pointer["migrate"].arn
    error_message = "The scheduler's host migrates, from the migration pointer."
  }
  assert {
    condition     = length(aws_ssm_parameter.pointer) == 6 && aws_ssm_parameter.pointer["worker"].name == "/hot/hot-example/release/worker"
    error_message = "A pointer per role plus the migration pointer, where the release helper writes them."
  }
  assert {
    condition = alltrue([for role, policy in aws_iam_role_policy.runtime : length(one([for s in jsondecode(policy.policy).Statement : s.Resource if s.Effect == "Allow" && s.Action == ["ssm:GetParameter"]])) == (role == "scheduler" ? 2 : 1)
    && toset(one([for s in jsondecode(policy.policy).Statement : s.NotResource if s.Effect == "Deny" && s.Action == ["ssm:GetParameter*"]])) == toset(local.role_parameters[role])])
    error_message = "Each role reads its own pointer, and no other parameter; only the scheduler also reads the migration pointer."
  }
  assert {
    condition     = alltrue([for attachment in aws_iam_role_policy_attachment.ssm : attachment.policy_arn == "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"]) && alltrue([for policy in aws_iam_role_policy.runtime : !strcontains(policy.policy, "ssmmessages")])
    error_message = "Every host keeps the data channels of its managed policy: port forwarding to the ingress needs them, and ssm:StartSession on operator principals decides who may use it."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.runtime["scheduler"].policy, "secretsmanager:PutSecretValue") && alltrue([for role, policy in aws_iam_role_policy.runtime : !strcontains(policy.policy, "secretsmanager:PutSecretValue") if role != "scheduler"])
    error_message = "Only the scheduler's host, which migrates first, may create the generated secret."
  }
  assert {
    condition     = one(jsondecode(aws_ssm_document.command.content).mainSteps).inputs.timeoutSeconds == "172800"
    error_message = "Systems Manager must never stop a long deploy partway."
  }
  assert {
    condition     = toset(jsondecode(aws_ssm_document.command.content).parameters.operation.allowedValues) == toset(["deploy-now", "status", "diagnostics", "logs", "restart-service", "test-alert"])
    error_message = "No drain-and-stop on a scalable installation: its groups replace a stopped web host, and the new host deploys and serves."
  }
  assert {
    condition     = jsondecode(aws_ssm_document.command.content).parameters.expires.allowedPattern == "^[0-9]{1,12}$" && jsondecode(aws_ssm_document.command.content).parameters.expires.default == "0"
    error_message = "A command's expiry must be constrained to digits, with 0 (no expiry) as its default."
  }
  assert {
    condition     = strcontains(one(one(jsondecode(aws_ssm_document.command.content).mainSteps).inputs.runCommand), "--expires {{ expires }}")
    error_message = "The agent must be told when a command expires, or one the helper gave up on runs later."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.runtime["scheduler"].policy, "rds!db") && !strcontains(aws_iam_role_policy.runtime["app"].policy, "rds!db")
    error_message = "Only the scheduler's host reads the database administrator's secret."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.release[0].policy, "autoscaling:DescribeAutoScalingGroups") && strcontains(aws_iam_role_policy.release[0].policy, "ssm:resourceTag/HotInstallation")
    error_message = "The release role lists instances and commands only this installation's hosts."
  }
  assert {
    condition     = one([for s in jsondecode(aws_iam_role_policy.release[0].policy).Statement : s.Action if contains(s.Action, "ssm:GetParameter")]) == ["ssm:PutParameter", "ssm:GetParameter"]
    error_message = "The release role reads only the pointers it writes: the helper reads reports from the log, not the status parameter."
  }
  assert {
    condition     = !strcontains(aws_iam_role_policy.release[0].policy, "ssm:StartSession") && !strcontains(aws_iam_role_policy.release[0].policy, "ssmmessages")
    error_message = "The release role starts no Session Manager session: that is for operator principals, limited to port forwarding."
  }
  assert {
    condition     = anytrue([for s in jsondecode(aws_iam_role_policy.release[0].policy).Statement : s.Action == ["cloudwatch:DescribeAlarms"] && s.Resource == "arn:aws:cloudwatch:us-east-1:*:alarm:*"])
    error_message = "Maintenance lists alarms by name prefix, which IAM authorizes against alarm:*, not against each alarm."
  }
  assert {
    condition     = anytrue([for s in jsondecode(aws_iam_role_policy.release[0].policy).Statement : s.Action == ["cloudwatch:DisableAlarmActions", "cloudwatch:EnableAlarmActions"] && s.Resource == "arn:aws:cloudwatch:us-east-1:*:alarm:hot-hot-example-*"])
    error_message = "Maintenance silences and restores only this installation's alarms."
  }
  assert {
    condition     = alltrue([for policy in aws_iam_role_policy.runtime : !strcontains(policy.policy, "s3:GetBucketLocation") && !strcontains(policy.policy, "s3:ListMultipartUploadParts") && strcontains(policy.policy, "s3:AbortMultipartUpload")])
    error_message = "Hosts hold the object calls Hot makes, and not the bucket-location or list-parts calls it never makes."
  }
}

run "endpoints" {
  command = plan
  assert {
    condition     = !aws_lb.web["app"].internal && aws_lb.web["api"].internal && aws_lb.web["app"].load_balancer_type == "network"
    error_message = "Each endpoint's exposure is independent: the app public, the API private."
  }
  assert {
    condition     = aws_lb_target_group.web["api"].port == 443 && aws_lb_target_group.web["api"].health_check[0].path == "/hot-lb-health" && aws_lb_target_group.web["api"].preserve_client_ip == "true"
    error_message = "Passthrough to the hosts' ingress on 443, health-checked through it, keeping client addresses."
  }
  assert {
    condition     = contains(one(aws_security_group.web["app"].ingress).cidr_blocks, "0.0.0.0/0") && toset(one(aws_security_group.web["api"].ingress).cidr_blocks) == toset(["10.68.1.0/24", "10.68.3.0/24", "10.10.0.0/16"])
    error_message = "A private endpoint admits the hosts' subnets (the load balancer's health checks) and its client ranges only."
  }
  assert {
    condition     = aws_autoscaling_group.role["api"].health_check_type == "ELB" && aws_autoscaling_group.role["worker"].health_check_type == "EC2"
    error_message = "Web groups replace hosts that fail the load balancer's health check."
  }
  assert {
    condition     = length(aws_cloudwatch_metric_alarm.hot) == 14 && length(jsondecode(aws_cloudwatch_dashboard.hot.dashboard_body).widgets) == 10 && aws_cloudwatch_metric_alarm.database_connections[0].threshold == 320 && aws_cloudwatch_metric_alarm.hot["scheduler-stopped"].treat_missing_data == "breaching"
    error_message = "Every signal alarms, and a missing scheduler status breaches."
  }
  assert {
    condition     = aws_cloudwatch_metric_alarm.hot["synthetic-failed"].treat_missing_data == "notBreaching" && aws_cloudwatch_metric_alarm.hot["synthetic-failed"].evaluation_periods == 2
    error_message = "The synthetic check reports once per run: one failure, even after a quiet spell, never fires the alarm."
  }
}

run "data_subnets_are_denied_on_every_role" {
  command = plan
  assert {
    condition     = alltrue([for config in values(local.host_config) : config.data.postgres.endpoint.deny_cidrs == tolist(["10.68.4.0/24", "10.68.5.0/24"]) && config.data.valkey.endpoint.deny_cidrs == tolist(["10.68.4.0/24", "10.68.5.0/24"])])
    error_message = "Every host learns the data subnets, where a standby's or replica's address is, so user code is denied them."
  }
}

run "the_operators_deny_cidrs_join_the_data_subnets" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : {
        "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.1.2.0/24", "10.68.4.0/24"] } },
        "valkey" : { "mode" : "existing", "endpoint" : { "deny_cidrs" : ["10.9.0.0/16"] } }
      },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
  assert {
    condition     = alltrue([for config in values(local.host_config) : config.data.postgres.endpoint.deny_cidrs == tolist(["10.1.2.0/24", "10.68.4.0/24", "10.68.5.0/24"]) && config.data.postgres.endpoint.host == "hot-example.c0123.us-east-1.rds.amazonaws.com"])
    error_message = "A managed service's endpoint adds the operator's networks to the data subnets."
  }
  assert {
    condition     = alltrue([for config in values(local.host_config) : config.data.valkey.endpoint == { deny_cidrs = tolist(["10.9.0.0/16"]) }])
    error_message = "An existing service's endpoint holds only the operator's networks, not the data subnets of another service."
  }
}

run "an_existing_service_without_deny_cidrs_has_no_endpoint" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "existing" }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
  assert {
    condition     = alltrue([for config in values(local.host_config) : config.data.postgres.endpoint == null && config.data.valkey.endpoint.deny_cidrs == tolist(["10.68.4.0/24", "10.68.5.0/24"])])
    error_message = "The data subnets of a managed service say nothing about where an existing one is."
  }
}

run "every_data_key_in_the_contract_is_accepted" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : {
        "postgres" : { "mode" : "managed", "runtime_role" : "hot_runtime", "version" : "18", "tier" : "db.t4g.medium", "storage_gb" : 50, "high_availability" : false, "backup_retention_days" : 7, "deletion_protection" : true, "admin_password_version" : 1, "endpoint" : { "deny_cidrs" : ["10.1.2.0/24"] } },
        "valkey" : { "mode" : "managed", "node_type" : "cache.t4g.small", "high_availability" : false, "auth_token_version" : 1, "endpoint" : { "deny_cidrs" : ["10.9.0.0/16"] } }
      },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
}

run "a_null_endpoint_is_not_an_unknown_key" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : null }, "valkey" : { "mode" : "managed", "endpoint" : null } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
}

run "refuses_a_misspelled_endpoint_key" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidr" : ["10.1.2.0/24"] } }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_misplaced_deny_list" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "existing", "deny_cidrs" : ["10.9.0.0/16"] } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_misspelled_data_service" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgresql" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.1.2.0/24"] } }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
  expect_failures = [var.installation]
}

# A mock cannot override actions_enabled, which the provider does not compute,
# so this reads the source: every alarm in the stack ignores it.
run "an_apply_keeps_the_maintenance_silence" {
  command = plan
  assert {
    condition = length(regexall("resource \"aws_cloudwatch_metric_alarm\"", join("\n", [for f in fileset(path.module, "*.tf") : file("${path.module}/${f}")]))) == 2 && alltrue([
      for alarm in regexall("(?s)resource \"aws_cloudwatch_metric_alarm\" \"\\w+\" \\{.*?\\n\\}", join("\n", [for f in fileset(path.module, "*.tf") : file("${path.module}/${f}")])) :
      strcontains(alarm, "lifecycle { ignore_changes = [actions_enabled] }")
    ])
    error_message = "Every alarm must ignore actions_enabled, or an apply during maintenance turns notifications back on. A new alarm also changes the count here."
  }
}

run "task_workers_scale_on_their_slots_and_retire_when_idle" {
  command = plan
  assert {
    condition     = aws_autoscaling_policy.task_slots[0].target_tracking_configuration[0].target_value == 70 && aws_autoscaling_policy.task_slots[0].target_tracking_configuration[0].disable_scale_in && aws_autoscaling_policy.task_slots[0].target_tracking_configuration[0].customized_metric_specification[0].metric_name == "TaskSlotsUsedPercent"
    error_message = "Task-workers scale out on the average share of their slots in use."
  }
  assert {
    condition     = length(aws_autoscaling_policy.jobs) == 0 && length(aws_autoscaling_group.role["worker"].enabled_metrics) == 0
    error_message = "Workers with a fixed count do not follow the queue."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.runtime["task-worker"].policy, "autoScalingGroupName/hot-hot-example-task-worker") && !strcontains(aws_iam_role_policy.runtime["worker"].policy, "TerminateInstanceInAutoScalingGroup")
    error_message = "Only task hosts may leave their own group."
  }
}

run "a_fixed_task_worker_count_can_shrink" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "size" : "small",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "task_worker" : { "min" : 2, "max" : 2 } }
    }
  }
  assert {
    condition     = !aws_autoscaling_group.role["task-worker"].protect_from_scale_in
    error_message = "A fixed count is not protected: the group could not remove protected hosts when the count is lowered, and they never retire."
  }
  assert {
    condition     = length(aws_autoscaling_policy.task_slots) == 0 && !contains(keys(aws_autoscaling_policy.cpu), "task-worker") && !strcontains(aws_iam_role_policy.runtime["task-worker"].policy, "TerminateInstanceInAutoScalingGroup")
    error_message = "A fixed count has no scaling policy for protection to guard against, and its hosts never leave on their own."
  }
  assert {
    condition     = aws_autoscaling_group.role["task-worker"].instance_refresh[0].preferences[0].scale_in_protected_instances == "Refresh"
    error_message = "A refresh still replaces hosts launched while the group was autoscaled, which keep their protection."
  }
}

run "workers_follow_the_jobs_waiting" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "size" : "small",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 6, "waiting_per_host" : 20 }, "task_worker" : { "min" : 1, "max" : 3, "retire_idle_minutes" : 0 } }
    }
  }
  assert {
    condition     = aws_autoscaling_policy.jobs[0].target_tracking_configuration[0].target_value == 20 && one([for m in aws_autoscaling_policy.jobs[0].target_tracking_configuration[0].customized_metric_specification[0].metrics : m if m.return_data]).expression == "waiting / hosts"
    error_message = "Workers target the jobs waiting per host in service."
  }
  assert {
    condition     = aws_autoscaling_group.role["worker"].enabled_metrics == toset(["GroupInServiceInstances"])
    error_message = "The worker group reports the hosts in service, which the policy divides by."
  }
  assert {
    condition     = aws_autoscaling_policy.jobs[0].target_tracking_configuration[0].disable_scale_in && aws_autoscaling_policy.cpu["worker"].target_tracking_configuration[0].disable_scale_in && !aws_autoscaling_policy.cpu["app"].target_tracking_configuration[0].disable_scale_in
    error_message = "Without a drain, the group never removes a worker host that may be running jobs; it only scales out."
  }
  assert {
    condition     = !strcontains(aws_iam_role_policy.runtime["task-worker"].policy, "TerminateInstanceInAutoScalingGroup")
    error_message = "retire_idle_minutes = 0 keeps every task host, and grants nothing."
  }
}

run "every_role_pulls_from_the_mirror" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "size" : "small",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "image_registry" : "123456789012.dkr.ecr.us-east-1.amazonaws.com/hot-mirror",
      "monitoring" : { "monthly_budget" : 900 },
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files", "provision" : true }
      }
    }
  }
  assert {
    condition = alltrue([for role, policy in aws_iam_role_policy.runtime : anytrue([for s in jsondecode(policy.policy).Statement :
    try(s.Resource, null) == "arn:aws:ecr:us-east-1:123456789012:repository/hot-mirror/*"])])
    error_message = "Every role's hosts may pull from the mirror."
  }
  assert {
    condition     = aws_budgets_budget.monthly[0].limit_amount == "900" && length(aws_budgets_budget.monthly[0].notification) == 0
    error_message = "A budget without alert emails still tracks spend, and notifies no one."
  }
}

run "refuses_a_single_machine_installation" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "existing_network_in_a_secondary_range" {
  command = plan
  override_data {
    target = data.aws_subnet.private["subnet-0aaaaaaaaaaaaaaaa"]
    values = { availability_zone = "us-east-1a", cidr_block = "100.64.0.0/20" }
  }
  override_data {
    target = data.aws_subnet.private["subnet-0bbbbbbbbbbbbbbbb"]
    values = { availability_zone = "us-east-1b", cidr_block = "100.64.16.0/20" }
  }
  override_data {
    target = data.aws_subnet.public["subnet-0eeeeeeeeeeeeeeee"]
    values = { availability_zone = "us-east-1a", cidr_block = "10.0.0.0/24" }
  }
  override_data {
    target = data.aws_subnet.public["subnet-0ffffffffffffffff"]
    values = { availability_zone = "us-east-1b", cidr_block = "10.0.1.0/24" }
  }
  override_data {
    target = data.aws_subnet.data["subnet-0cccccccccccccccc"]
    values = { availability_zone = "us-east-1b", cidr_block = "100.64.32.0/20" }
  }
  override_data {
    target = data.aws_subnet.data["subnet-0dddddddddddddddd"]
    values = { availability_zone = "us-east-1a", cidr_block = "100.64.48.0/20" }
  }
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.example",
      "api_hostname" : "api.hot.internal",
      "app_exposure" : "public",
      "network" : {
        "mode" : "existing",
        "network_id" : "vpc-0123456789abcdef0",
        "subnet_id" : "subnet-0aaaaaaaaaaaaaaaa",
        "client_cidrs" : ["192.168.50.0/24"],
        "private_subnet_ids" : ["subnet-0aaaaaaaaaaaaaaaa", "subnet-0bbbbbbbbbbbbbbbb"],
        "public_subnet_ids" : ["subnet-0eeeeeeeeeeeeeeee", "subnet-0ffffffffffffffff"],
        "data_subnet_ids" : ["subnet-0cccccccccccccccc", "subnet-0dddddddddddddddd"]
      },
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
  assert {
    condition     = toset(one(aws_security_group.web["api"].ingress).cidr_blocks) == toset(["100.64.0.0/20", "100.64.16.0/20", "192.168.50.0/24"])
    error_message = "A private endpoint admits health checks from the hosts' subnets, whichever VPC range they are in, and the client ranges, not the whole VPC."
  }
  assert {
    condition     = aws_elasticache_replication_group.valkey[0].preferred_cache_cluster_azs == tolist(["us-east-1a", "us-east-1b"]) && aws_db_instance.postgres[0].availability_zone == "us-east-1a"
    error_message = "On an existing network the cache's replica goes in the data subnets' second zone."
  }
  assert {
    condition     = alltrue([for config in values(local.host_config) : config.data.postgres.endpoint.deny_cidrs == tolist(["100.64.32.0/20", "100.64.48.0/20"]) && config.data.valkey.endpoint.deny_cidrs == tolist(["100.64.32.0/20", "100.64.48.0/20"])])
    error_message = "On an existing network, where the VPC's range is not denied, every host learns each data subnet's range."
  }
}

run "public_subnets_must_cover_the_hosts_zones" {
  command = plan
  override_data {
    target = data.aws_subnet.private["subnet-0aaaaaaaaaaaaaaaa"]
    values = { availability_zone = "us-east-1a", cidr_block = "10.0.0.0/24" }
  }
  override_data {
    target = data.aws_subnet.private["subnet-0bbbbbbbbbbbbbbbb"]
    values = { availability_zone = "us-east-1c", cidr_block = "10.0.1.0/24" }
  }
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.example",
      "api_hostname" : "api.hot.internal",
      "app_exposure" : "public",
      "network" : {
        "mode" : "existing",
        "network_id" : "vpc-0123456789abcdef0",
        "subnet_id" : "subnet-0aaaaaaaaaaaaaaaa",
        "private_subnet_ids" : ["subnet-0aaaaaaaaaaaaaaaa", "subnet-0bbbbbbbbbbbbbbbb"],
        "public_subnet_ids" : ["subnet-0eeeeeeeeeeeeeeee", "subnet-0ffffffffffffffff"],
        "data_subnet_ids" : ["subnet-0cccccccccccccccc", "subnet-0dddddddddddddddd"]
      },
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
  expect_failures = [aws_lb.web]
}
