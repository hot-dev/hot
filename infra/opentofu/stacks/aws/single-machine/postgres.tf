# Managed PostgreSQL (data.postgres.mode = "managed"): RDS in private subnets,
# reachable only from the host and only over TLS. RDS keeps the administrator
# password in its own secret, so it never enters OpenTofu state; the host
# agent reads it to create Hot's logins, database, and schema.
locals {
  managed_postgres = local.config.data.postgres.mode == "managed"
  postgres         = local.config.data.postgres
  postgres_endpoint = !local.managed_postgres ? null : {
    host         = aws_db_instance.postgres[0].address
    port         = aws_db_instance.postgres[0].port
    admin_user   = aws_db_instance.postgres[0].username
    admin_secret = aws_db_instance.postgres[0].master_user_secret[0].secret_arn
  }
}
resource "aws_db_subnet_group" "postgres" {
  count      = local.managed_postgres ? 1 : 0
  name       = "hot-${local.config.name}"
  subnet_ids = local.data_subnet_ids
}
resource "aws_security_group" "postgres" {
  count       = local.managed_postgres ? 1 : 0
  name_prefix = "hot-${local.config.name}-postgres-"
  description = "PostgreSQL from the Hot host only"
  vpc_id      = local.vpc_id
  ingress {
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [aws_security_group.host.id]
  }
}
resource "aws_db_parameter_group" "postgres" {
  count       = local.managed_postgres ? 1 : 0
  name_prefix = "hot-${local.config.name}-"
  family      = "postgres${local.postgres.version}"
  parameter {
    name  = "rds.force_ssl"
    value = "1"
  }
  parameter {
    name  = "ssl_min_protocol_version"
    value = "TLSv1.2"
  }
  lifecycle { create_before_destroy = true }
}
resource "aws_db_instance" "postgres" {
  count                       = local.managed_postgres ? 1 : 0
  identifier                  = "hot-${local.config.name}"
  engine                      = "postgres"
  engine_version              = local.postgres.version
  allow_major_version_upgrade = false
  auto_minor_version_upgrade  = true
  instance_class              = module.contract.managed_postgres.tier
  allocated_storage           = module.contract.managed_postgres.storage_gb
  max_allocated_storage       = module.contract.managed_postgres.storage_gb * 4
  storage_type                = "gp3"
  storage_encrypted           = true
  # The agent creates the hot database, so RDS creates none.
  username                    = "hot_admin"
  manage_master_user_password = true
  db_subnet_group_name        = aws_db_subnet_group.postgres[0].name
  vpc_security_group_ids      = [aws_security_group.postgres[0].id]
  parameter_group_name        = aws_db_parameter_group.postgres[0].name
  publicly_accessible         = false
  ca_cert_identifier          = "rds-ca-rsa2048-g1"
  multi_az                    = local.postgres.high_availability
  availability_zone           = local.postgres.high_availability ? null : local.config.zone
  backup_retention_period     = local.postgres.backup_retention_days
  copy_tags_to_snapshot       = true
  delete_automated_backups    = false
  # Removing the database, including by changing data.postgres.mode, needs
  # deletion_protection turned off in a reviewed apply first, and still
  # leaves a final snapshot.
  deletion_protection       = local.postgres.deletion_protection
  skip_final_snapshot       = false
  final_snapshot_identifier = "hot-${local.config.name}-final"
  apply_immediately         = false
}
output "postgres_endpoint" { value = local.managed_postgres ? aws_db_instance.postgres[0].address : null }
