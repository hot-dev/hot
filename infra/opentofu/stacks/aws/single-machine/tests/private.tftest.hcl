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
    condition     = aws_ssm_document.command.document_type == "Command" && length(jsondecode(aws_ssm_document.command.content).parameters.operation.allowedValues) == 6
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
