terraform {
  required_version = ">= 1.13.1, < 1.14.0"
  required_providers {
    aws = { source = "hashicorp/aws", version = "6.69.1" }
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
resource "aws_cloudwatch_log_group" "host" {
  name              = "/hot/${local.config.name}"
  retention_in_days = 30
}
resource "aws_iam_role_policy" "runtime" {
  role = aws_iam_role.host.id
  policy = jsonencode({ Version = "2012-10-17", Statement = concat([
    { Effect = "Allow", Action = ["secretsmanager:GetSecretValue"], Resource = values(local.config.secret_references) },
    { Effect = "Allow", Action = ["ssm:GetParameter"], Resource = local.config.release_pointer },
    { Effect = "Allow", Action = ["logs:CreateLogStream", "logs:PutLogEvents"], Resource = "${aws_cloudwatch_log_group.host.arn}:*" },
    { Effect = "Allow", Action = ["cloudwatch:PutMetricData"], Resource = "*", Condition = { StringEquals = { "cloudwatch:namespace" = "Hot/SelfHost" } } }
    ], [for s in local.s3_roles : {
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
  tags              = { Name = "hot-${local.config.name}-data" }
  lifecycle { prevent_destroy = true }
}
resource "aws_instance" "host" {
  ami                         = local.config.host_image
  instance_type               = module.contract.capacity.aws
  availability_zone           = local.config.zone
  subnet_id                   = local.subnet_id
  associate_public_ip_address = false
  vpc_security_group_ids      = [aws_security_group.host.id]
  iam_instance_profile        = aws_iam_instance_profile.host.name
  user_data_replace_on_change = true
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
    config_base64 = base64encode(jsonencode(merge(local.config, { data_volume_id = aws_ebs_volume.data.id })))
  })
  tags = { Name = "hot-${local.config.name}" }
  lifecycle {
    precondition {
      condition     = local.config.cloud == "aws"
      error_message = "The AWS root requires installation.cloud = aws."
    }
  }
  depends_on = [aws_route_table_association.private, aws_iam_role_policy.runtime, aws_iam_role_policy_attachment.ssm]
}
resource "aws_volume_attachment" "data" {
  device_name = "/dev/sdf"
  volume_id   = aws_ebs_volume.data.id
  instance_id = aws_instance.host.id
}
output "private_ip" { value = aws_instance.host.private_ip }
output "instance_id" { value = aws_instance.host.id }
output "retained_data_volume_id" { value = aws_ebs_volume.data.id }
output "egress_ips" { value = aws_eip.egress[*].public_ip }
output "configuration" { value = local.config }
