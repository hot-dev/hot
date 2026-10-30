# Managed data services share private subnets and tell the host agent where
# they are through the installation configuration.
locals {
  managed_data = local.managed_postgres || local.managed_valkey
  # Database and cache subnet groups span two zones, even for single-zone
  # services.
  data_subnet_ids = local.new_network ? aws_subnet.data[*].id : local.config.network.data_subnet_ids
  host_data = merge(local.config.data, {
    postgres = merge(local.postgres, { endpoint = local.postgres_endpoint })
    valkey   = merge(local.valkey, { endpoint = local.valkey_endpoint })
  })
}
resource "aws_subnet" "data" {
  count                   = local.managed_data && local.new_network ? 2 : 0
  vpc_id                  = local.vpc_id
  availability_zone       = count.index == 0 ? local.config.zone : local.second_zone
  cidr_block              = cidrsubnet(local.config.network.cidr, 8, 4 + count.index)
  map_public_ip_on_launch = false
}
