# Managed data services share private subnets and tell the host agent where
# they are through the installation configuration.
locals {
  managed_data = local.managed_postgres || local.managed_valkey
  # Database and cache subnet groups span two zones, even for single-zone
  # services.
  data_subnet_ids = local.new_network ? aws_subnet.data[*].id : local.config.network.data_subnet_ids
  data_zones      = local.new_network ? aws_subnet.data[*].availability_zone : distinct(sort([for s in data.aws_subnet.data : s.availability_zone]))
  # Single-zone services, and a primary, go in the installation's zone when a
  # data subnet is there; a replica goes in another of the subnets' zones.
  data_zone         = contains(local.data_zones, local.config.zone) ? local.config.zone : try(local.data_zones[0], local.config.zone)
  data_replica_zone = try([for zone in local.data_zones : zone if zone != local.data_zone][0], null)
  # Where any node of a service can be, which user code is denied as well as
  # the endpoint's address when it was rendered: a standby or replica has an
  # address no endpoint names, and a failover moves the endpoint to it. A
  # managed service is in the data subnets, IPv4 only as nothing here sets a
  # network_type; where an existing one is, only the operator can say.
  data_subnet_cidrs   = sort(local.new_network ? aws_subnet.data[*].cidr_block : [for s in data.aws_subnet.data : s.cidr_block])
  postgres_deny_cidrs = sort(distinct(concat(local.managed_postgres ? local.data_subnet_cidrs : [], local.postgres.endpoint.deny_cidrs)))
  valkey_deny_cidrs   = sort(distinct(concat(local.managed_valkey ? local.data_subnet_cidrs : [], local.valkey.endpoint.deny_cidrs)))
  host_data = merge(local.config.data, {
    postgres = merge(local.postgres, { endpoint = length(local.postgres_deny_cidrs) > 0 ? merge(local.postgres_endpoint, { deny_cidrs = local.postgres_deny_cidrs }) : null })
    valkey   = merge(local.valkey, { endpoint = length(local.valkey_deny_cidrs) > 0 ? merge(local.valkey_endpoint, { deny_cidrs = local.valkey_deny_cidrs }) : null })
  })
}
resource "aws_subnet" "data" {
  count                   = local.managed_data && local.new_network ? 2 : 0
  vpc_id                  = local.vpc_id
  availability_zone       = count.index == 0 ? local.config.zone : local.second_zone
  cidr_block              = cidrsubnet(local.config.network.cidr, 8, 4 + count.index)
  map_public_ip_on_launch = false
}
# An existing network's data subnets, for their zones and ranges.
data "aws_subnet" "data" {
  for_each = local.managed_data && !local.new_network ? toset(local.config.network.data_subnet_ids) : toset([])
  id       = each.value
}
