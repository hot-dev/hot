# Managed Valkey (data.valkey.mode = "managed"): an ElastiCache replication
# group in private subnets, reachable only from the host, TLS-only, with an
# AUTH token. The token is generated during apply and written only to
# write-only attributes, so it never enters OpenTofu state or plans; the host
# agent reads it from Secrets Manager.
locals {
  managed_valkey = local.config.data.valkey.mode == "managed"
  valkey         = local.config.data.valkey
  valkey_endpoint = !local.managed_valkey ? null : {
    host        = aws_elasticache_replication_group.valkey[0].primary_endpoint_address
    port        = aws_elasticache_replication_group.valkey[0].port
    auth_secret = aws_secretsmanager_secret.valkey[0].arn
  }
}
ephemeral "random_password" "valkey" {
  count   = local.managed_valkey ? 1 : 0
  length  = 64
  special = false
}
resource "aws_elasticache_subnet_group" "valkey" {
  count      = local.managed_valkey ? 1 : 0
  name       = "hot-${local.config.name}"
  subnet_ids = local.data_subnet_ids
}
resource "aws_security_group" "valkey" {
  count       = local.managed_valkey ? 1 : 0
  name_prefix = "hot-${local.config.name}-valkey-"
  description = "Valkey from the Hot host only"
  vpc_id      = local.vpc_id
  ingress {
    from_port       = 6379
    to_port         = 6379
    protocol        = "tcp"
    security_groups = [aws_security_group.host.id]
  }
}
# Hot's queues must never lose entries to eviction.
resource "aws_elasticache_parameter_group" "valkey" {
  count  = local.managed_valkey ? 1 : 0
  name   = "hot-${local.config.name}"
  family = "valkey8"
  parameter {
    name  = "maxmemory-policy"
    value = "noeviction"
  }
}
# A raised data.valkey.auth_token_version rotates the token: ElastiCache takes
# the new one beside the one before it and drops any older one (ROTATE, its
# strategy when none is named), so hosts stay connected until they have the new
# token, and a leaked token stops working at the second raise. The provider
# sends a token change at once, whatever apply_immediately says. No
# auth_token_update_strategy is set: SET takes only the token the cache already
# has, which no later apply can supply, and naming a strategy on a cache that
# exists sends ElastiCache a change with no token in it.
resource "aws_elasticache_replication_group" "valkey" {
  count                       = local.managed_valkey ? 1 : 0
  replication_group_id        = "hot-${local.config.name}"
  description                 = "Hot ${local.config.name} queues"
  engine                      = "valkey"
  engine_version              = "8.1"
  auto_minor_version_upgrade  = true
  node_type                   = module.contract.managed_valkey.node_type
  num_cache_clusters          = local.valkey.high_availability ? 2 : 1
  automatic_failover_enabled  = local.valkey.high_availability
  multi_az_enabled            = local.valkey.high_availability
  preferred_cache_cluster_azs = local.valkey.high_availability ? [local.data_zone, local.data_replica_zone] : [local.data_zone]
  port                        = 6379
  subnet_group_name           = aws_elasticache_subnet_group.valkey[0].name
  security_group_ids          = [aws_security_group.valkey[0].id]
  parameter_group_name        = aws_elasticache_parameter_group.valkey[0].name
  at_rest_encryption_enabled  = true
  transit_encryption_enabled  = true
  transit_encryption_mode     = "required"
  auth_token_wo               = ephemeral.random_password.valkey[0].result
  auth_token_wo_version       = local.valkey.auth_token_version
  snapshot_retention_limit    = 1
  apply_immediately           = false
  # The token's secret exists before the cache does, so a secret that
  # cannot be created stops the apply before the cache has a token.
  depends_on = [aws_secretsmanager_secret.valkey]
  lifecycle {
    precondition {
      condition     = !local.valkey.high_availability || local.data_replica_zone != null
      error_message = "High-availability Valkey needs data subnets in two zones."
    }
  }
}
resource "aws_secretsmanager_secret" "valkey" {
  count                   = local.managed_valkey ? 1 : 0
  name                    = "hot/${local.config.name}/valkey"
  recovery_window_in_days = 7
}
resource "aws_secretsmanager_secret_version" "valkey" {
  count                    = local.managed_valkey ? 1 : 0
  secret_id                = aws_secretsmanager_secret.valkey[0].id
  secret_string_wo         = jsonencode({ password = ephemeral.random_password.valkey[0].result })
  secret_string_wo_version = local.valkey.auth_token_version
  # Stored only after the cache has the token, so a failed create never
  # leaves a stored token the cache does not have. The token exists only
  # during the apply that creates the cache: if this write fails after
  # that, a re-run stores a different token and the cache keeps its own, so
  # raise data.valkey.auth_token_version to give both a new one.
  depends_on = [aws_elasticache_replication_group.valkey]
}
output "valkey_endpoint" { value = local.managed_valkey ? aws_elasticache_replication_group.valkey[0].primary_endpoint_address : null }
