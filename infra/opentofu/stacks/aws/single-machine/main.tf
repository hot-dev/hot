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
locals {
  config      = module.contract.configuration
  new_network = local.config.network.mode == "new"
  vpc_id      = local.new_network ? aws_vpc.hot[0].id : local.config.network.network_id
  subnet_id   = local.new_network ? aws_subnet.private[0].id : local.config.network.subnet_id
  s3_roles    = [for s in [local.config.storage.build, local.config.storage.file] : s if s.backend == "s3"]
  # Load balancers and database subnet groups need a second zone.
  second_zone = try([for zone in data.aws_availability_zones.available[0].names : zone if zone != local.config.zone][0], null)
  # Pulling images from an ECR mirror with the host's own role.
  mirror_statements = module.contract.mirror.kind != "ecr" ? [] : [
    { Effect = "Allow", Action = ["ecr:GetAuthorizationToken"], Resource = "*" },
    { Effect = "Allow", Action = ["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer", "ecr:BatchCheckLayerAvailability"],
    Resource = "arn:aws:ecr:${module.contract.mirror.region}:${module.contract.mirror.account}:repository/${module.contract.mirror.repositories}" },
  ]
}
data "aws_availability_zones" "available" {
  count = local.new_network && (local.public || local.managed_data) ? 1 : 0
  state = "available"
}
resource "aws_vpc" "hot" {
  count                = local.new_network ? 1 : 0
  cidr_block           = local.config.network.cidr
  enable_dns_support   = true
  enable_dns_hostnames = true
}
resource "aws_subnet" "public" {
  count                   = local.new_network ? 1 : 0
  vpc_id                  = local.vpc_id
  availability_zone       = local.config.zone
  cidr_block              = cidrsubnet(local.config.network.cidr, 8, 0)
  map_public_ip_on_launch = false
}
resource "aws_subnet" "private" {
  count                   = local.new_network ? 1 : 0
  vpc_id                  = local.vpc_id
  availability_zone       = local.config.zone
  cidr_block              = cidrsubnet(local.config.network.cidr, 8, 1)
  map_public_ip_on_launch = false
}
resource "aws_internet_gateway" "egress" {
  count  = local.new_network ? 1 : 0
  vpc_id = local.vpc_id
}
resource "aws_eip" "egress" {
  count  = local.new_network ? 1 : 0
  domain = "vpc"
}
resource "aws_nat_gateway" "egress" {
  count         = local.new_network ? 1 : 0
  allocation_id = aws_eip.egress[0].id
  subnet_id     = aws_subnet.public[0].id
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
  count          = local.new_network ? 1 : 0
  subnet_id      = aws_subnet.public[0].id
  route_table_id = aws_route_table.public[0].id
}
resource "aws_route_table" "private" {
  count  = local.new_network ? 1 : 0
  vpc_id = local.vpc_id
  route {
    cidr_block     = "0.0.0.0/0"
    nat_gateway_id = aws_nat_gateway.egress[0].id
  }
}
resource "aws_route_table_association" "private" {
  count          = local.new_network ? 1 : 0
  subnet_id      = aws_subnet.private[0].id
  route_table_id = aws_route_table.private[0].id
}
resource "aws_vpc_endpoint" "s3" {
  count             = local.new_network ? 1 : 0
  vpc_id            = local.vpc_id
  service_name      = "com.amazonaws.${local.config.region}.s3"
  vpc_endpoint_type = "Gateway"
  route_table_ids   = [aws_route_table.private[0].id]
}
resource "aws_security_group" "host" {
  name_prefix = "hot-${local.config.name}-"
  description = "Private HTTPS only; no SSH or database/cache ingress"
  vpc_id      = local.vpc_id
  dynamic "ingress" {
    for_each = length(local.config.network.client_cidrs) > 0 ? [true] : []
    content {
      from_port   = 443
      to_port     = 443
      protocol    = "tcp"
      cidr_blocks = local.config.network.client_cidrs
    }
  }
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}
# The release process owns the pointer's value; OpenTofu only creates it.
resource "aws_ssm_parameter" "release" {
  name  = "/hot/${local.config.name}/release"
  type  = "String"
  value = "none"
  lifecycle { ignore_changes = [value] }
}
# The host's configuration, which the agent rereads every cycle: a change
# applied in place takes effect without replacing the VM. The user data holds
# the configuration the VM first booted with.
resource "aws_ssm_parameter" "installation" {
  name  = "/hot/${local.config.name}/installation"
  type  = "String"
  tier  = "Intelligent-Tiering"
  value = jsonencode(local.host_config)
}
resource "aws_ssm_parameter" "status" {
  name  = "/hot/${local.config.name}/status"
  type  = "String"
  value = "{}"
  lifecycle { ignore_changes = [value] }
}
# The agent stores values the installation generates for itself here, once;
# they never enter OpenTofu state.
resource "aws_secretsmanager_secret" "generated" {
  name                    = "hot/${local.config.name}/generated"
  recovery_window_in_days = 30
  lifecycle { prevent_destroy = true }
}
# The operator puts the TLS certificate and key, and any SMTP, OAuth, or S3
# credentials, here as one JSON object.
resource "aws_secretsmanager_secret" "operator" {
  name                    = "hot/${local.config.name}/operator"
  recovery_window_in_days = 30
}
# The only way to run anything on the host: a fixed set of agent commands,
# with every parameter constrained. There is no shell or SSH.
resource "aws_ssm_document" "command" {
  name            = "hot-${local.config.name}-command"
  document_type   = "Command"
  document_format = "JSON"
  content = jsonencode({
    schemaVersion = "2.2"
    description   = "Run one Hot agent command on the installation host."
    parameters = {
      operation = { type = "String", allowedValues = ["deploy-now", "status", "diagnostics", "logs", "backup", "restart-service", "drain-and-stop", "test-alert", "roll-back"] }
      service   = { type = "String", default = "none", allowedValues = ["none", "app", "api", "worker", "task-worker", "scheduler", "ingress", "postgres", "valkey"] }
      requestId = { type = "String", allowedPattern = "^[A-Za-z0-9-]{1,64}$" }
    }
    mainSteps = [{
      action = "aws:runShellScript"
      name   = "hotAgent"
      inputs = { runCommand = ["/usr/local/bin/hot-agent command {{ operation }} --service {{ service }} --request-id {{ requestId }}"] }
    }]
  })
}
locals {
  host_config = merge(local.config, {
    data_volume_id  = aws_ebs_volume.data.id
    release_pointer = aws_ssm_parameter.release.arn
    # Not the resource's ARN: the parameter holds this configuration.
    installation_parameter = "/hot/${local.config.name}/installation"
    status_reference       = aws_ssm_parameter.status.arn
    secret_references      = { generated = aws_secretsmanager_secret.generated.arn, operator = aws_secretsmanager_secret.operator.arn }
    data                   = local.host_data
  })
}
resource "aws_iam_role" "host" {
  name = "hot-${local.config.name}-host"
  assume_role_policy = jsonencode({ Version = "2012-10-17", Statement = [{
    Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole"
  }] })
}
resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.host.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}
# Container logs, shipped by Docker's awslogs driver with the host's role.
resource "aws_cloudwatch_log_group" "host" {
  name              = "/hot/${local.config.name}"
  retention_in_days = local.config.logging.retention_days
}
resource "aws_iam_role_policy" "runtime" {
  role = aws_iam_role.host.id
  policy = jsonencode({ Version = "2012-10-17", Statement = concat([
    { Effect = "Allow", Action = ["secretsmanager:GetSecretValue"], Resource = concat([aws_secretsmanager_secret.generated.arn, aws_secretsmanager_secret.operator.arn], aws_db_instance.postgres[*].master_user_secret[0].secret_arn, aws_secretsmanager_secret.valkey[*].arn) },
    { Effect = "Allow", Action = ["secretsmanager:PutSecretValue"], Resource = aws_secretsmanager_secret.generated.arn },
    { Effect = "Allow", Action = ["ssm:GetParameter"], Resource = [aws_ssm_parameter.release.arn, aws_ssm_parameter.installation.arn] },
    { Effect = "Allow", Action = ["ssm:PutParameter"], Resource = aws_ssm_parameter.status.arn },
    { Effect = "Allow", Action = ["logs:CreateLogStream", "logs:PutLogEvents"], Resource = "${aws_cloudwatch_log_group.host.arn}:*" },
    { Effect = "Allow", Action = ["cloudwatch:PutMetricData"], Resource = "*", Condition = { StringEquals = { "cloudwatch:namespace" = "Hot/SelfHost" } } },
    # Backup age for monitoring; snapshots cannot be scoped by resource.
    { Effect = "Allow", Action = ["ec2:DescribeSnapshots"], Resource = "*" }
    ], local.mirror_statements, [for s in local.s3_roles : {
      Effect = "Allow", Action = ["s3:GetBucketLocation"], Resource = "arn:aws:s3:::${s.bucket}"
      }], [for s in local.s3_roles : {
      Effect    = "Allow", Action = ["s3:ListBucket"], Resource = "arn:aws:s3:::${s.bucket}",
      Condition = { StringLike = { "s3:prefix" = [trim(s.prefix, "/"), "${trim(s.prefix, "/")}/*"] } }
      }], [for s in local.s3_roles : {
      Effect = "Allow", Action = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:AbortMultipartUpload", "s3:ListMultipartUploadParts"], Resource = "arn:aws:s3:::${s.bucket}/${trim(s.prefix, "/")}/*"
  }]) })
}
resource "aws_iam_instance_profile" "host" {
  name = "hot-${local.config.name}"
  role = aws_iam_role.host.name
}
resource "aws_ebs_volume" "data" {
  availability_zone = local.config.zone
  size              = module.contract.capacity.data_gb
  type              = "gp3"
  encrypted         = true
  # Restores create the volume from a snapshot; see "Recovery" in
  # infra/README.md. It is read only when the volume is created.
  snapshot_id = local.config.backup.restore_snapshot == "" ? null : local.config.backup.restore_snapshot
  # The snapshot policy in monitoring.tf selects the volume by this tag.
  tags = { Name = "hot-${local.config.name}-data", HotDataVolume = local.config.name }
  lifecycle {
    prevent_destroy = true
    ignore_changes  = [snapshot_id]
  }
}
# The host's address outlives the VM (a new host image replaces it), so a
# private hostname pointed at it keeps working. An interface made before the
# instance gets no public address.
resource "aws_network_interface" "host" {
  subnet_id       = local.subnet_id
  security_groups = [aws_security_group.host.id]
  tags            = { Name = "hot-${local.config.name}" }
}
resource "aws_instance" "host" {
  ami                  = local.config.host_image
  instance_type        = module.contract.capacity.aws
  availability_zone    = local.config.zone
  iam_instance_profile = aws_iam_instance_profile.host.name
  primary_network_interface {
    network_interface_id = aws_network_interface.host.id
  }
  metadata_options {
    http_tokens                 = "required"
    http_put_response_hop_limit = 2
    instance_metadata_tags      = "disabled"
  }
  cpu_options { nested_virtualization = "enabled" }
  root_block_device {
    volume_size = 40
    volume_type = "gp3"
    encrypted   = true
  }
  user_data = templatefile("${path.module}/../../../../runtime/bootstrap.sh.tftpl", {
    config_base64 = base64encode(jsonencode(local.host_config))
  })
  tags = { Name = "hot-${local.config.name}" }
  lifecycle {
    # The agent follows configuration changes from the installation
    # parameter; a new size stops and starts this instance in place.
    ignore_changes = [user_data]
    precondition {
      condition     = local.config.cloud == "aws"
      error_message = "The AWS root requires installation.cloud = aws."
    }
  }
  depends_on = [aws_route_table_association.private, aws_iam_role_policy.runtime, aws_iam_role_policy_attachment.ssm, aws_s3_bucket_policy.storage]
}
# The release and command workflows may write the pointer, read status, send
# the Hot command to this installation's host, and read the agent's reports
# from the installation's log, and nothing else.
resource "aws_iam_role_policy" "release" {
  count = var.release_principal == "" ? 0 : 1
  name  = "hot-${local.config.name}-release"
  role  = element(split("/", var.release_principal), length(split("/", var.release_principal)) - 1)
  policy = jsonencode({ Version = "2012-10-17", Statement = [
    { Effect = "Allow", Action = ["ssm:PutParameter"], Resource = aws_ssm_parameter.release.arn },
    { Effect = "Allow", Action = ["ssm:GetParameter"], Resource = aws_ssm_parameter.status.arn },
    { Effect = "Allow", Action = ["ssm:SendCommand"], Resource = [aws_ssm_document.command.arn, aws_instance.host.arn] },
    { Effect = "Allow", Action = ["logs:FilterLogEvents"], Resource = [aws_cloudwatch_log_group.host.arn, "${aws_cloudwatch_log_group.host.arn}:*"] },
    # selfhost.py maintenance: silence and restore this installation's alarms.
    { Effect = "Allow", Action = ["cloudwatch:DescribeAlarms", "cloudwatch:DisableAlarmActions", "cloudwatch:EnableAlarmActions"], Resource = "arn:aws:cloudwatch:${local.config.region}:*:alarm:hot-${local.config.name}-*" },
  ] })
}
resource "aws_volume_attachment" "data" {
  device_name = "/dev/sdf"
  volume_id   = aws_ebs_volume.data.id
  instance_id = aws_instance.host.id
}
output "private_ip" { value = aws_network_interface.host.private_ip }
output "instance_id" { value = aws_instance.host.id }
output "retained_data_volume_id" { value = aws_ebs_volume.data.id }
output "egress_ips" { value = aws_eip.egress[*].public_ip }
output "configuration" { value = local.host_config }
output "release_parameter" { value = aws_ssm_parameter.release.name }
output "status_parameter" { value = aws_ssm_parameter.status.name }
output "operator_secret_arn" { value = aws_secretsmanager_secret.operator.arn }
output "command_document" { value = aws_ssm_document.command.name }
