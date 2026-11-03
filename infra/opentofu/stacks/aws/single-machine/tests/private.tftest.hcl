mock_provider "aws" {
  mock_data "aws_availability_zones" {
    defaults = { names = ["us-east-1a", "us-east-1b"] }
  }
  mock_data "aws_subnet" {
    defaults = { availability_zone = "us-east-1a", cidr_block = "172.31.0.0/20" }
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
    condition     = length(aws_instance.host.primary_network_interface) == 1 && aws_network_interface.host.subnet_id != null && aws_instance.host.metadata_options[0].http_tokens == "required"
    error_message = "The VM must have no public address and must require IMDSv2."
  }
  assert {
    condition     = aws_instance.host.root_block_device[0].encrypted && aws_ebs_volume.data.encrypted
    error_message = "Both boot and retained data disks must be encrypted."
  }
  assert {
    condition     = length(aws_vpc_security_group_ingress_rule.clients) == 0 && length(aws_vpc_security_group_ingress_rule.from_load_balancer) == 0
    error_message = "Without explicit client ranges the VM must have no inbound security-group rules."
  }
  assert {
    condition     = aws_volume_attachment.data_disk.stop_instance_before_detaching
    error_message = "Replacing the VM stops it before its data disk is detached."
  }
  assert {
    condition     = aws_security_group.host.description != "Private HTTPS only; no SSH or database/cache ingress"
    error_message = "The v3.0.1 description would keep the group, and with it the inline rule that fails the upgrade and leaves old client ranges open."
  }
  assert {
    condition     = !contains(keys(output.configuration), "load_balancer_cidrs")
    error_message = "Without a public endpoint the host trusts no load balancer."
  }
  assert {
    condition     = output.configuration.data.postgres.endpoint == null && output.configuration.data.valkey.endpoint == null && length(local.data_subnet_cidrs) == 0
    error_message = "Local data services have no endpoint and no data subnets to deny."
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

run "refuses_a_scalable_installation" {
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
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
  expect_failures = [var.installation]
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
  override_resource {
    target = aws_ssm_parameter.release
    values = { arn = "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release" }
  }
  assert {
    condition     = one(jsondecode(aws_ssm_document.command.content).mainSteps).inputs.timeoutSeconds == "172800"
    error_message = "Systems Manager must never stop a long deploy, backup, or roll-back partway."
  }
  assert {
    condition = anytrue([for s in jsondecode(aws_iam_role_policy.runtime.policy).Statement : s.Effect == "Deny" && s.Action == ["ssm:GetParameter*"]
    && toset(s.NotResource) == toset(["arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release", "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/installation"])])
    error_message = "The host reads its own pointer and configuration, and no other parameter."
  }
  assert {
    condition     = aws_iam_role_policy_attachment.ssm.policy_arn == "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore" && !strcontains(aws_iam_role_policy.runtime.policy, "ssmmessages")
    error_message = "The host keeps the data channels of its managed policy: port forwarding to the ingress needs them, and ssm:StartSession on operator principals decides who may use it."
  }
  assert {
    condition     = aws_ssm_document.command.document_type == "Command" && length(jsondecode(aws_ssm_document.command.content).parameters.operation.allowedValues) == 9
    error_message = "Host commands must be a fixed allowlist in a Hot command document."
  }
  assert {
    condition     = jsondecode(aws_ssm_document.command.content).parameters.requestId.allowedPattern == "^[A-Za-z0-9-]{1,64}$"
    error_message = "Every command parameter must be constrained."
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
    condition     = aws_ssm_parameter.release.value == "none" && aws_ssm_parameter.status.type == "String" && jsondecode(aws_ssm_parameter.installation.value).installation_parameter == "/hot/hot-example/installation" && jsondecode(aws_ssm_parameter.installation.value).name == "hot-example"
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
    error_message = "The release role is granted only this installation's pointer, command, and reports."
  }
  assert {
    condition     = anytrue([for s in jsondecode(aws_iam_role_policy.release[0].policy).Statement : s.Action == ["ssm:SendCommand"] && s.Resource == aws_ssm_document.command.arn])
    error_message = "The release role sends the Hot command document."
  }
  assert {
    condition = anytrue([for s in jsondecode(aws_iam_role_policy.release[0].policy).Statement : s.Action == ["ssm:SendCommand"] && s.Resource == "arn:aws:ec2:us-east-1:*:instance/*"
    && try(s.Condition.StringEquals["ssm:resourceTag/HotInstallation"], "") == "hot-example"])
    error_message = "selfhost.py sends by tag, which IAM authorizes against instance/* and the instance's tag, not its ARN."
  }
  assert {
    condition     = !strcontains(aws_iam_role_policy.release[0].policy, "ssm:GetParameter")
    error_message = "The release role reads no parameter: the helper reads reports from the log, not the status parameter."
  }
  assert {
    condition     = !strcontains(aws_iam_role_policy.release[0].policy, "ssm:StartSession") && !strcontains(aws_iam_role_policy.release[0].policy, "ssmmessages")
    error_message = "The release role starts no Session Manager session: that is for operator principals, limited to port forwarding."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.release[0].policy, "logs:FilterLogEvents") && !strcontains(aws_iam_role_policy.release[0].policy, "logs:GetLogEvents")
    error_message = "The release role reads the agent's reports from this installation's log group only."
  }
  assert {
    condition     = anytrue([for s in jsondecode(aws_iam_role_policy.release[0].policy).Statement : s.Action == ["cloudwatch:DescribeAlarms"] && s.Resource == "arn:aws:cloudwatch:us-east-1:*:alarm:*"])
    error_message = "Maintenance lists alarms by name prefix, which IAM authorizes against alarm:*, not against each alarm."
  }
  assert {
    condition     = anytrue([for s in jsondecode(aws_iam_role_policy.release[0].policy).Statement : s.Action == ["cloudwatch:DisableAlarmActions", "cloudwatch:EnableAlarmActions"] && s.Resource == "arn:aws:cloudwatch:us-east-1:*:alarm:hot-hot-example-*"])
    error_message = "Maintenance silences and restores only this installation's alarms."
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
    condition     = length(aws_instance.host.primary_network_interface) == 1 && aws_lb.public[0].internal == false
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
  assert {
    condition     = output.configuration.load_balancer_cidrs == tolist(["10.68.0.0/24", "10.68.2.0/24"])
    error_message = "The host trusts forwarded client addresses from the load balancer's subnets only."
  }
  assert {
    condition     = data.aws_availability_zones.available[0].state == null && one(data.aws_availability_zones.available[0].filter).name == "zone-type" && contains(tolist(one(data.aws_availability_zones.available[0].filter).values), "availability-zone")
    error_message = "The second zone is a standard zone, chosen the same way whatever state the zones are in."
  }
}

run "public_on_an_existing_network_with_client_ranges" {
  command = plan
  override_data {
    target = data.aws_subnet.public["subnet-0aaaaaaaaaaaaaaaa"]
    values = { availability_zone = "us-east-1a", cidr_block = "172.31.0.0/20" }
  }
  override_data {
    target = data.aws_subnet.public["subnet-0bbbbbbbbbbbbbbbb"]
    values = { availability_zone = "us-east-1b", cidr_block = "172.31.16.0/20" }
  }
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
      "network" : {
        "mode" : "existing",
        "network_id" : "vpc-0123456789abcdef0",
        "subnet_id" : "subnet-0123456789abcdef0",
        "public_subnet_ids" : ["subnet-0aaaaaaaaaaaaaaaa", "subnet-0bbbbbbbbbbbbbbbb"],
        "client_cidrs" : ["10.10.0.0/16", "10.20.0.0/16"]
      },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = one(aws_security_group.load_balancer[0].egress).from_port == 9443 && one(aws_security_group.load_balancer[0].egress).cidr_blocks == null && length(one(aws_security_group.load_balancer[0].egress).security_groups) == 1
    error_message = "The load balancer reaches the host's group, not network.cidr, which does not describe an existing network."
  }
  assert {
    condition     = toset(output.configuration.load_balancer_cidrs) == toset(["172.31.0.0/20", "172.31.16.0/20"])
    error_message = "The host trusts forwarded client addresses from the existing load balancer subnets."
  }
  assert {
    condition     = keys(aws_vpc_security_group_ingress_rule.clients) == ["10.10.0.0/16", "10.20.0.0/16"] && aws_vpc_security_group_ingress_rule.clients["10.10.0.0/16"].from_port == 443 && length(aws_vpc_security_group_ingress_rule.from_load_balancer) == 1
    error_message = "Client ranges and the load balancer each get their own rule, so neither apply removes the other."
  }
  assert {
    condition = alltrue([for description in concat([aws_security_group.host.description, aws_vpc_security_group_ingress_rule.from_load_balancer[0].description], [for rule in aws_vpc_security_group_ingress_rule.clients : rule.description]) :
    can(regex("^[A-Za-z0-9 ._:/()#,@\\[\\]+=;{}!$*-]+$", description))])
    error_message = "EC2 refuses any other character in a security group or rule description, an apostrophe included, and only at apply."
  }
}

run "public_subnets_must_reach_the_host_zone" {
  command = plan
  override_data {
    target = data.aws_subnet.public["subnet-0aaaaaaaaaaaaaaaa"]
    values = { availability_zone = "us-east-1b", cidr_block = "172.31.16.0/20" }
  }
  override_data {
    target = data.aws_subnet.public["subnet-0bbbbbbbbbbbbbbbb"]
    values = { availability_zone = "us-east-1c", cidr_block = "172.31.32.0/20" }
  }
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
      "app_exposure" : "public",
      "network" : {
        "mode" : "existing",
        "network_id" : "vpc-0123456789abcdef0",
        "subnet_id" : "subnet-0123456789abcdef0",
        "public_subnet_ids" : ["subnet-0aaaaaaaaaaaaaaaa", "subnet-0bbbbbbbbbbbbbbbb"]
      },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [aws_lb.public]
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
    condition     = aws_db_instance.postgres[0].instance_class == "db.m7g.large" && aws_db_instance.postgres[0].allocated_storage == 50 && !aws_db_instance.postgres[0].multi_az && aws_db_instance.postgres[0].availability_zone == "us-east-1a"
    error_message = "A small installation starts with a single-zone db.m7g.large and 50 GB, in the host's zone."
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
  assert {
    condition     = output.configuration.data.postgres.endpoint.deny_cidrs == tolist(["10.68.4.0/24", "10.68.5.0/24"])
    error_message = "The host learns the data subnets, where a standby's address is, so user code is denied them."
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
  assert {
    condition     = output.configuration.data.valkey.endpoint.deny_cidrs == tolist(["10.68.4.0/24", "10.68.5.0/24"])
    error_message = "The host learns the data subnets, where a replica's address is, so user code is denied them."
  }
}

run "managed_valkey_on_an_existing_network" {
  command = plan
  override_data {
    target = data.aws_subnet.data["subnet-0cccccccccccccccc"]
    values = { availability_zone = "us-east-1b", cidr_block = "172.31.48.0/20" }
  }
  override_data {
    target = data.aws_subnet.data["subnet-0dddddddddddddddd"]
    values = { availability_zone = "us-east-1a", cidr_block = "172.31.64.0/20" }
  }
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
      "network" : {
        "mode" : "existing",
        "network_id" : "vpc-0123456789abcdef0",
        "subnet_id" : "subnet-0123456789abcdef0",
        "data_subnet_ids" : ["subnet-0cccccccccccccccc", "subnet-0dddddddddddddddd"]
      },
      "data" : { "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = aws_elasticache_replication_group.valkey[0].preferred_cache_cluster_azs == tolist(["us-east-1a", "us-east-1b"])
    error_message = "On an existing network the primary goes in the host's zone and the replica in the data subnets' other zone."
  }
  assert {
    condition     = output.configuration.data.valkey.endpoint.deny_cidrs == tolist(["172.31.48.0/20", "172.31.64.0/20"])
    error_message = "On an existing network, where the VPC's range is not denied, the host learns each data subnet's range."
  }
}

run "the_operators_deny_cidrs_join_the_data_subnets" {
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
      "data" : {
        "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.1.2.0/24", "10.68.4.0/24"] } },
        "valkey" : { "mode" : "existing", "endpoint" : { "deny_cidrs" : ["10.9.0.0/16"] } }
      },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.data.postgres.endpoint.deny_cidrs == tolist(["10.1.2.0/24", "10.68.4.0/24", "10.68.5.0/24"]) && output.configuration.data.postgres.endpoint.host == "hot-example.c0123.us-east-1.rds.amazonaws.com"
    error_message = "A managed service's endpoint adds the operator's networks to the data subnets."
  }
  assert {
    condition     = output.configuration.data.valkey.endpoint == { deny_cidrs = tolist(["10.9.0.0/16"]) }
    error_message = "An existing service's endpoint holds only the operator's networks, not the data subnets of another service."
  }
}

run "an_existing_service_without_deny_cidrs_has_no_endpoint" {
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
      "data" : { "postgres" : { "mode" : "existing" }, "valkey" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.data.postgres.endpoint == null && output.configuration.data.valkey.endpoint.deny_cidrs == tolist(["10.68.4.0/24", "10.68.5.0/24"])
    error_message = "The data subnets of a managed service say nothing about where an existing one is."
  }
}

run "every_data_key_in_the_contract_is_accepted" {
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
      "data" : {
        "postgres" : { "mode" : "managed", "runtime_role" : "hot_runtime", "version" : "18", "tier" : "db.t4g.medium", "storage_gb" : 50, "high_availability" : false, "backup_retention_days" : 7, "deletion_protection" : true, "admin_password_version" : 1, "endpoint" : { "deny_cidrs" : ["10.1.2.0/24"] } },
        "valkey" : { "mode" : "managed", "node_type" : "cache.t4g.small", "high_availability" : false, "auth_token_version" : 1, "endpoint" : { "deny_cidrs" : ["10.9.0.0/16"] } }
      },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
}

run "a_null_endpoint_is_not_an_unknown_key" {
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
      "data" : { "postgres" : { "mode" : "local", "endpoint" : null } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
}

run "null_data_is_not_an_unknown_key" {
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
      "data" : null,
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
}

run "refuses_a_misspelled_endpoint_key" {
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
      "data" : { "postgres" : { "mode" : "existing", "endpoint" : { "deny_cidr" : ["10.1.2.0/24"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_misplaced_deny_list" {
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
      "data" : { "valkey" : { "mode" : "existing", "deny_cidrs" : ["10.9.0.0/16"] } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_misspelled_data_service" {
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
      "data" : { "postgresql" : { "mode" : "existing", "endpoint" : { "deny_cidrs" : ["10.1.2.0/24"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "managed_valkey_replica_needs_a_second_zone" {
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
      "network" : {
        "mode" : "existing",
        "network_id" : "vpc-0123456789abcdef0",
        "subnet_id" : "subnet-0123456789abcdef0",
        "data_subnet_ids" : ["subnet-0cccccccccccccccc", "subnet-0dddddddddddddddd"]
      },
      "data" : { "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [aws_elasticache_replication_group.valkey]
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
    condition     = length(aws_sns_topic_subscription.alerts) == 2 && length(aws_cloudwatch_metric_alarm.hot) == 15
    error_message = "Every alert email is subscribed and every critical signal has an alarm."
  }
  assert {
    condition     = aws_cloudwatch_metric_alarm.hot["alert-test"].period == 60 && length(jsondecode(aws_cloudwatch_dashboard.hot.dashboard_body).widgets) == 11 && jsondecode(aws_cloudwatch_dashboard.hot.dashboard_body).widgets[3].properties.metrics[0] == ["Hot/SelfHost", "JobsWaiting", "Installation", "hot-example"]
    error_message = "A test alarm for the alert path, and a dashboard of the installation's metrics."
  }
  assert {
    condition     = aws_cloudwatch_metric_alarm.hot["host-down"].treat_missing_data == "breaching" && aws_cloudwatch_metric_alarm.hot["backup-stale"].treat_missing_data == "breaching"
    error_message = "A missing heartbeat or backup status alarms; absence is not health."
  }
  assert {
    condition     = aws_cloudwatch_metric_alarm.hot["synthetic-failed"].treat_missing_data == "notBreaching" && aws_cloudwatch_metric_alarm.hot["synthetic-failed"].evaluation_periods == 2
    error_message = "The synthetic check reports once per run: one failure, even after a quiet spell, never fires the alarm."
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

# A mock cannot override actions_enabled, which the provider does not compute,
# so this reads the source: every alarm in the stack ignores it.
run "an_apply_keeps_the_maintenance_silence" {
  command = plan
  assert {
    condition = length(regexall("resource \"aws_cloudwatch_metric_alarm\"", join("\n", [for f in fileset(path.module, "*.tf") : file("${path.module}/${f}")]))) == 3 && alltrue([
      for alarm in regexall("(?s)resource \"aws_cloudwatch_metric_alarm\" \"\\w+\" \\{.*?\\n\\}", join("\n", [for f in fileset(path.module, "*.tf") : file("${path.module}/${f}")])) :
      strcontains(alarm, "lifecycle { ignore_changes = [actions_enabled] }")
    ])
    error_message = "Every alarm must ignore actions_enabled, or an apply during maintenance turns notifications back on. A new alarm also changes the count here."
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
    condition     = one([for r in aws_s3_bucket_lifecycle_configuration.storage["acme-hot-data"].rule : r if r.id == "old-versions"]).expiration[0].expired_object_delete_marker
    error_message = "A deleted object's marker goes once its old versions have expired."
  }
  assert {
    condition     = strcontains(aws_iam_role_policy.runtime.policy, "arn:aws:s3:::acme-hot-data/builds/*") && strcontains(aws_iam_role_policy.runtime.policy, "arn:aws:s3:::acme-hot-data/files/*")
    error_message = "The host reaches each role's prefix only."
  }
  assert {
    condition     = !strcontains(aws_iam_role_policy.runtime.policy, "s3:GetBucketLocation") && !strcontains(aws_iam_role_policy.runtime.policy, "s3:ListMultipartUploadParts") && strcontains(aws_iam_role_policy.runtime.policy, "s3:AbortMultipartUpload")
    error_message = "The host holds the object calls Hot makes, and not the bucket-location or list-parts calls it never makes."
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
    try(s.Resource, null) == "arn:aws:ecr:us-east-1:123456789012:repository/hot-mirror/*" && contains(s.Action, "ecr:BatchGetImage")])
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
