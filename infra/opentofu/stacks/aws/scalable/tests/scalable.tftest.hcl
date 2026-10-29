mock_provider "aws" {
  mock_data "aws_vpc" {
    defaults = { cidr_block = "10.68.0.0/16" }
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
    error_message = "Task-workers are protected while their tasks run and only scale out."
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
    condition     = length(jsondecode(aws_iam_role_policy.runtime["scheduler"].policy).Statement[2].Resource) == 2 && length(jsondecode(aws_iam_role_policy.runtime["app"].policy).Statement[2].Resource) == 1
    error_message = "Each role reads its own pointer; only the scheduler also reads the migration pointer."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.runtime["scheduler"].policy, "rds!db") && !strcontains(aws_iam_role_policy.runtime["app"].policy, "rds!db")
    error_message = "Only the scheduler's host reads the database administrator's secret."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.release[0].policy, "autoscaling:DescribeAutoScalingGroups") && strcontains(aws_iam_role_policy.release[0].policy, "ssm:resourceTag/HotInstallation")
    error_message = "The release role lists instances and commands only this installation's hosts."
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
    condition     = contains(one(aws_security_group.web["app"].ingress).cidr_blocks, "0.0.0.0/0") && !contains(one(aws_security_group.web["api"].ingress).cidr_blocks, "0.0.0.0/0") && contains(one(aws_security_group.web["api"].ingress).cidr_blocks, "10.10.0.0/16")
    error_message = "A private endpoint admits the VPC (health checks) and its client ranges only."
  }
  assert {
    condition     = aws_autoscaling_group.role["api"].health_check_type == "ELB" && aws_autoscaling_group.role["worker"].health_check_type == "EC2"
    error_message = "Web groups replace hosts that fail the load balancer's health check."
  }
  assert {
    condition     = length(aws_cloudwatch_metric_alarm.hot) == 9 && aws_cloudwatch_metric_alarm.hot["scheduler-stopped"].treat_missing_data == "breaching"
    error_message = "Every signal alarms, and a missing scheduler status breaches."
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
    s.Resource == "arn:aws:ecr:us-east-1:123456789012:repository/hot-mirror/*"])])
    error_message = "Every role's hosts may pull from the mirror."
  }
  assert {
    condition     = aws_budgets_budget.monthly[0].limit_amount == "900" && length(aws_budgets_budget.monthly[0].notification) == 0
    error_message = "A budget without alert emails still tracks spend, and notifies no one."
  }
}
