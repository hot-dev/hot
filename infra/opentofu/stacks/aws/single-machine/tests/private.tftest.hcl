mock_provider "aws" {
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
  installation = {
    "schema_version" : 1,
    "name" : "hot-example",
    "cloud" : "aws",
    "region" : "us-east-1",
    "zone" : "us-east-1a",
    "size" : "small",
    "host_image" : "ami-0123456789abcdef0",
    "project" : "",
    "release_pointer" : "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
    "secret_references" : {
      "app" : "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/app-example"
    },
    "app_hostname" : "app.hot.internal",
    "api_hostname" : "api.hot.internal",
    "storage" : {
      "build" : {
        "backend" : "local"
      },
      "file" : {
        "backend" : "local"
      }
    }
  }
}

run "private_local" {
  command = plan
  assert {
    condition     = aws_instance.host.associate_public_ip_address == false && aws_instance.host.metadata_options[0].http_tokens == "required"
    error_message = "The VM must have no public address and must require IMDSv2."
  }
  assert {
    condition     = aws_instance.host.root_block_device[0].encrypted && aws_ebs_volume.data.encrypted
    error_message = "Both boot and retained data disks must be encrypted."
  }
  assert {
    condition     = length(aws_security_group.host.ingress) == 0
    error_message = "Without explicit client ranges the VM must have no inbound security-group rules."
  }
  assert {
    condition     = aws_instance.host.cpu_options[0].nested_virtualization == "enabled"
    error_message = "The task host must enable nested virtualization."
  }
}

run "private_existing_s3" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "size" : "small",
      "host_image" : "ami-0123456789abcdef0",
      "project" : "",
      "release_pointer" : "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
      "secret_references" : {
        "app" : "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/app-example"
      },
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "storage" : {
        "build" : {
          "backend" : "s3",
          "bucket" : "customer-hot-storage",
          "region" : "us-east-1",
          "prefix" : "hot-example/builds"
        },
        "file" : {
          "backend" : "s3",
          "bucket" : "customer-hot-storage",
          "region" : "us-east-1",
          "prefix" : "hot-example/files"
        }
      }
    }
  }
  assert {
    condition     = output.configuration.storage.file.backend == "s3" && output.configuration.storage.file.bucket == "customer-hot-storage"
    error_message = "S3 selection must remain independent of compute cloud."
  }
}

run "requires_development_acknowledgment" {
  command = plan
  variables { acknowledge_unqualified = false }
  expect_failures = [var.acknowledge_unqualified]
}

run "existing_network_ownership" {
  command = plan
  variables { installation = {
    "schema_version" : 1,
    "name" : "hot-example",
    "cloud" : "aws",
    "region" : "us-east-1",
    "zone" : "us-east-1a",
    "size" : "small",
    "host_image" : "ami-0123456789abcdef0",
    "project" : "",
    "release_pointer" : "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
    "secret_references" : {
      "app" : "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/app-example"
    },
    "app_hostname" : "app.hot.internal",
    "api_hostname" : "api.hot.internal",
    "storage" : {
      "build" : {
        "backend" : "local"
      },
      "file" : {
        "backend" : "local"
      }
    },
    "network" : {
      "mode" : "existing",
      "network_id" : "vpc-0123456789abcdef0",
      "subnet_id" : "subnet-0123456789abcdef0"
    }
  } }
  assert {
    condition     = length(aws_vpc.hot) == 0 && length(aws_nat_gateway.egress) == 0 && length(aws_subnet.private) == 0
    error_message = "Existing networking must not be recreated or re-routed."
  }
}

run "agent_channel" {
  command = plan
  assert {
    condition     = aws_ssm_document.command.document_type == "Command" && length(jsondecode(aws_ssm_document.command.content).parameters.operation.allowedValues) == 9
    error_message = "Host commands must be a fixed allowlist in a Hot command document."
  }
  assert {
    condition     = jsondecode(aws_ssm_document.command.content).parameters.requestId.allowedPattern == "^[A-Za-z0-9-]{1,64}$"
    error_message = "Every command parameter must be constrained."
  }
  assert {
    condition     = aws_ssm_parameter.release.value == "none" && aws_ssm_parameter.status.type == "String"
    error_message = "The stack creates the release pointer and status; the release process owns the pointer's value."
  }
  assert {
    condition     = !strcontains(aws_iam_role_policy.runtime.policy, "\"*\",\"Effect\":\"Allow\",\"Resource\":\"*\"") && strcontains(aws_iam_role_policy.runtime.policy, "secretsmanager:PutSecretValue")
    error_message = "The host may store only its generated secrets."
  }
}

run "release_principal_is_scoped" {
  command = plan
  variables {
    release_principal = "arn:aws:iam::123456789012:role/ci/hot-github-release"
  }
  assert {
    condition     = aws_iam_role_policy.release[0].role == "hot-github-release" && !strcontains(aws_iam_role_policy.release[0].policy, "\"Resource\":\"*\"")
    error_message = "The release role is granted only this installation's pointer, status, and command."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.release[0].policy, "logs:FilterLogEvents") && !strcontains(aws_iam_role_policy.release[0].policy, "logs:GetLogEvents")
    error_message = "The release role reads the agent's reports from this installation's log group only."
  }
}

run "public_api_only" {
  command = plan
  variables {
    public_certificate_arn = "arn:aws:acm:us-east-1:123456789012:certificate/0123"
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.example",
      "api_hostname" : "api.hot.example",
      "api_exposure" : "public",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = aws_instance.host.associate_public_ip_address == false && aws_lb.public[0].internal == false
    error_message = "Public access goes through the load balancer; the VM stays private."
  }
  assert {
    condition     = toset(one(aws_lb_listener_rule.public[0].condition).host_header[0].values) == toset(["api.hot.example"])
    error_message = "Only the public endpoint's hostname is forwarded."
  }
  assert {
    condition     = aws_lb_target_group.public[0].port == 9443 && aws_lb_target_group.public[0].health_check[0].path == "/hot-lb-health"
    error_message = "The load balancer reaches the host's public-endpoint port."
  }
}

run "public_needs_a_certificate" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.example",
      "api_hostname" : "api.hot.example",
      "app_exposure" : "public",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [aws_lb.public]
}

run "managed_postgres" {
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
      "data" : { "postgres" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = aws_db_instance.postgres[0].manage_master_user_password && aws_db_instance.postgres[0].password == null && aws_db_instance.postgres[0].password_wo == null
    error_message = "RDS keeps the administrator password in its own secret, never in state."
  }
  assert {
    condition     = !aws_db_instance.postgres[0].publicly_accessible && aws_db_instance.postgres[0].storage_encrypted && aws_db_instance.postgres[0].deletion_protection && !aws_db_instance.postgres[0].skip_final_snapshot
    error_message = "The database is private, encrypted, protected from deletion, and snapshotted if removed."
  }
  assert {
    condition     = aws_db_instance.postgres[0].instance_class == "db.m7g.large" && aws_db_instance.postgres[0].allocated_storage == 50 && !aws_db_instance.postgres[0].multi_az
    error_message = "A small installation starts with a single-zone db.m7g.large and 50 GB."
  }
  assert {
    condition     = one(aws_security_group.postgres[0].ingress).from_port == 5432 && one(aws_security_group.postgres[0].ingress).cidr_blocks == null && length(one(aws_security_group.postgres[0].ingress).security_groups) == 1
    error_message = "Only the host's security group reaches PostgreSQL."
  }
  assert {
    condition     = contains([for p in aws_db_parameter_group.postgres[0].parameter : "${p.name}=${p.value}"], "rds.force_ssl=1")
    error_message = "RDS must refuse connections without TLS."
  }
  assert {
    condition     = length(toset(aws_subnet.data[*].availability_zone)) == 2
    error_message = "The DB subnet group spans two zones."
  }
  assert {
    condition     = output.configuration.data.postgres.endpoint.admin_secret == "arn:aws:secretsmanager:us-east-1:123456789012:secret:rds!db-0123-abcdef" && strcontains(aws_iam_role_policy.runtime.policy, "rds!db-0123-abcdef")
    error_message = "The agent learns, and may read, only the administrator secret RDS created."
  }
}

run "managed_valkey" {
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
      "data" : { "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = aws_elasticache_replication_group.valkey[0].auth_token == null && aws_elasticache_replication_group.valkey[0].auth_token_wo_version == 1 && aws_secretsmanager_secret_version.valkey[0].secret_string == null
    error_message = "The AUTH token reaches ElastiCache and Secrets Manager only through write-only attributes."
  }
  assert {
    condition     = aws_elasticache_replication_group.valkey[0].transit_encryption_enabled && aws_elasticache_replication_group.valkey[0].transit_encryption_mode == "required" && aws_elasticache_replication_group.valkey[0].at_rest_encryption_enabled
    error_message = "Valkey requires TLS and encrypts data at rest."
  }
  assert {
    condition     = one(aws_elasticache_parameter_group.valkey[0].parameter).value == "noeviction"
    error_message = "Queues must never be evicted."
  }
  assert {
    condition     = aws_elasticache_replication_group.valkey[0].num_cache_clusters == 2 && aws_elasticache_replication_group.valkey[0].automatic_failover_enabled && length(toset(aws_elasticache_replication_group.valkey[0].preferred_cache_cluster_azs)) == 2
    error_message = "High availability adds a replica in a second zone with automatic failover."
  }
  assert {
    condition     = one(aws_security_group.valkey[0].ingress).from_port == 6379 && one(aws_security_group.valkey[0].ingress).cidr_blocks == null
    error_message = "Only the host's security group reaches Valkey."
  }
  assert {
    condition     = output.configuration.data.valkey.endpoint.host == "master.hot-example.abc123.use1.cache.amazonaws.com" && output.configuration.data.postgres.endpoint == null && length(aws_db_instance.postgres) == 0
    error_message = "Each data service is managed independently."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.runtime.policy, "secret:hot/hot-example/secret-AbCdEf") && length(aws_subnet.data) == 2
    error_message = "The agent may read the token's secret; the cache gets subnets in two zones."
  }
}

run "backups_and_alerts" {
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
      "backup" : { "retention_days" : 14, "time_utc" : "05:00" },
      "monitoring" : { "alert_emails" : ["ops@example.com", "oncall@example.com"] },
      "logging" : { "retention_days" : 90 },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = aws_dlm_lifecycle_policy.data.policy_details[0].target_tags["HotDataVolume"] == "hot-example" && aws_ebs_volume.data.tags["HotDataVolume"] == "hot-example"
    error_message = "The snapshot policy selects exactly this installation's data volume."
  }
  assert {
    condition     = one(one(aws_dlm_lifecycle_policy.data.policy_details[0].schedule).retain_rule).interval == 14 && one(one(aws_dlm_lifecycle_policy.data.policy_details[0].schedule).create_rule).times[0] == "05:00"
    error_message = "Snapshots run daily at the chosen hour and are kept for the chosen days."
  }
  assert {
    condition     = length(aws_sns_topic_subscription.alerts) == 2 && length(aws_cloudwatch_metric_alarm.hot) == 13
    error_message = "Every alert email is subscribed and every critical signal has an alarm."
  }
  assert {
    condition     = aws_cloudwatch_metric_alarm.hot["alert-test"].period == 60 && length(jsondecode(aws_cloudwatch_dashboard.hot.dashboard_body).widgets) == 9 && jsondecode(aws_cloudwatch_dashboard.hot.dashboard_body).widgets[3].properties.metrics[0] == ["Hot/SelfHost", "JobsWaiting", "Installation", "hot-example"]
    error_message = "A test alarm for the alert path, and a dashboard of the installation's metrics."
  }
  assert {
    condition     = aws_cloudwatch_metric_alarm.hot["host-down"].treat_missing_data == "breaching" && aws_cloudwatch_metric_alarm.hot["backup-stale"].treat_missing_data == "breaching"
    error_message = "A missing heartbeat or backup status alarms; absence is not health."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.runtime.policy, "ec2:DescribeSnapshots") && !strcontains(aws_iam_role_policy.runtime.policy, "ec2:DeleteSnapshot")
    error_message = "The host may read snapshot times but never delete snapshots."
  }
  assert {
    condition     = aws_cloudwatch_log_group.host.retention_in_days == 90
    error_message = "Container logs are kept for the chosen days."
  }
}

run "restore_from_a_snapshot" {
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
      "backup" : { "restore_snapshot" : "snap-0123456789abcdef0" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = aws_ebs_volume.data.snapshot_id == "snap-0123456789abcdef0"
    error_message = "A restore creates the data disk from the chosen snapshot."
  }
}

run "provisioned_buckets" {
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
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot-data", "region" : "us-east-1", "prefix" : "builds", "provision" : true, "noncurrent_days" : 14 },
        "file" : { "backend" : "s3", "bucket" : "acme-hot-data", "region" : "us-east-1", "prefix" : "files", "provision" : true, "noncurrent_days" : 14 }
      }
    }
  }
  assert {
    condition     = keys(aws_s3_bucket.storage) == ["acme-hot-data"] && !aws_s3_bucket.storage["acme-hot-data"].force_destroy
    error_message = "A bucket shared by both roles is created once and is never emptied on destroy."
  }
  assert {
    condition     = aws_s3_bucket_public_access_block.storage["acme-hot-data"].restrict_public_buckets && strcontains(aws_s3_bucket_policy.storage["acme-hot-data"].policy, "aws:SecureTransport")
    error_message = "Provisioned buckets are private and TLS-only."
  }
  assert {
    condition     = one(aws_s3_bucket_versioning.storage["acme-hot-data"].versioning_configuration).status == "Enabled" && one([for r in aws_s3_bucket_lifecycle_configuration.storage["acme-hot-data"].rule : r if r.id == "old-versions"]).noncurrent_version_expiration[0].noncurrent_days == 14
    error_message = "Old versions are kept for the chosen days."
  }
  assert {
    condition     = one([for r in aws_s3_bucket_lifecycle_configuration.storage["acme-hot-data"].rule : r if r.id == "abandoned-uploads"]).abort_incomplete_multipart_upload[0].days_after_initiation == 7
    error_message = "Abandoned multipart uploads are cleaned up."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.runtime.policy, "arn:aws:s3:::acme-hot-data/builds/*") && strcontains(aws_iam_role_policy.runtime.policy, "arn:aws:s3:::acme-hot-data/files/*")
    error_message = "The host reaches each role's prefix only."
  }
}

run "mirrors_queues_and_budgets" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "image_registry" : "123456789012.dkr.ecr.us-east-1.amazonaws.com/hot-mirror",
      "monitoring" : { "alert_emails" : ["ops@example.com"], "queue_backlog_alert" : 250, "monthly_budget" : 400 },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition = anytrue([for s in jsondecode(aws_iam_role_policy.runtime.policy).Statement :
    s.Resource == "arn:aws:ecr:us-east-1:123456789012:repository/hot-mirror/*" && contains(s.Action, "ecr:BatchGetImage")])
    error_message = "The host role pulls only from the mirror's repositories."
  }
  assert {
    condition     = aws_cloudwatch_metric_alarm.hot["queue-backlog"].threshold == 250 && aws_cloudwatch_metric_alarm.hot["new-dead-letters"].metric_name == "NewDeadLetters"
    error_message = "Queue alarms use the chosen backlog threshold and alert on new dead letters."
  }
  assert {
    condition     = aws_budgets_budget.monthly[0].limit_amount == "400" && contains(one(aws_budgets_budget.monthly[0].cost_filter).values, "user:HotInstallation$hot-example") && length(aws_budgets_budget.monthly[0].notification) == 2
    error_message = "The budget covers this installation's tagged resources and emails the alert addresses."
  }
}

run "no_mirror_or_budget_by_default" {
  command = plan
  assert {
    condition     = length(aws_budgets_budget.monthly) == 0 && !strcontains(aws_iam_role_policy.runtime.policy, "ecr:")
    error_message = "Without a mirror or budget, the host has no ECR access and there is no budget."
  }
}
