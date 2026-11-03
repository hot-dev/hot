# Public app and/or API access: an internet-facing Application Load Balancer
# terminates TLS with the operator's ACM certificate and re-encrypts to the
# host's load-balancer port, which serves only the public hostnames. The VM
# itself keeps no public address.
locals {
  public_hosts = [for role in ["app", "api"] : local.config["${role}_hostname"] if local.config["${role}_exposure"] == "public"]
  public       = length(local.public_hosts) > 0
  # The ALB needs subnets in two zones.
  public_subnet_ids   = local.new_network ? concat(aws_subnet.public[*].id, aws_subnet.public_second[*].id) : local.config.network.public_subnet_ids
  public_subnet_cidrs = local.new_network ? concat(aws_subnet.public[*].cidr_block, aws_subnet.public_second[*].cidr_block) : [for s in data.aws_subnet.public : s.cidr_block]
  public_zones        = local.new_network ? [local.config.zone, local.second_zone] : distinct([for s in data.aws_subnet.public : s.availability_zone])
}
# An existing network's load balancer subnets, for their zones and ranges.
data "aws_subnet" "public" {
  for_each = local.public && !local.new_network ? toset(local.config.network.public_subnet_ids) : toset([])
  id       = each.value
}
resource "aws_subnet" "public_second" {
  count                   = local.public && local.new_network ? 1 : 0
  vpc_id                  = local.vpc_id
  availability_zone       = local.second_zone
  cidr_block              = cidrsubnet(local.config.network.cidr, 8, 2)
  map_public_ip_on_launch = false
}
resource "aws_route_table_association" "public_second" {
  count          = local.public && local.new_network ? 1 : 0
  subnet_id      = aws_subnet.public_second[0].id
  route_table_id = aws_route_table.public[0].id
}
resource "aws_security_group" "load_balancer" {
  count       = local.public ? 1 : 0
  name_prefix = "hot-${local.config.name}-lb-"
  description = "Public HTTPS to the Hot load balancer"
  vpc_id      = local.vpc_id
  ingress {
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }
  # To the host's group, not a range: network.cidr does not describe an
  # existing network.
  egress {
    from_port       = 9443
    to_port         = 9443
    protocol        = "tcp"
    security_groups = [aws_security_group.host.id]
  }
}
resource "aws_vpc_security_group_ingress_rule" "from_load_balancer" {
  count                        = local.public ? 1 : 0
  security_group_id            = aws_security_group.host.id
  referenced_security_group_id = aws_security_group.load_balancer[0].id
  from_port                    = 9443
  to_port                      = 9443
  ip_protocol                  = "tcp"
  description                  = "Load balancer to the public-endpoint port of the host"
}
resource "aws_lb" "public" {
  count                      = local.public ? 1 : 0
  name                       = "hot-${local.config.name}"
  internal                   = false
  load_balancer_type         = "application"
  subnets                    = local.public_subnet_ids
  security_groups            = [aws_security_group.load_balancer[0].id]
  drop_invalid_header_fields = true
  lifecycle {
    precondition {
      condition     = var.public_certificate_arn != ""
      error_message = "A public endpoint needs public_certificate_arn."
    }
    # A load balancer sends nothing to a target in a zone it is not in.
    precondition {
      condition     = contains(local.public_zones, local.config.zone)
      error_message = "network.public_subnet_ids must include a subnet in the host's zone, or the load balancer never reaches the host."
    }
  }
}
resource "aws_lb_target_group" "public" {
  count       = local.public ? 1 : 0
  name        = "hot-${local.config.name}"
  port        = 9443
  protocol    = "HTTPS"
  vpc_id      = local.vpc_id
  target_type = "instance"
  health_check {
    protocol = "HTTPS"
    port     = "9443"
    path     = "/hot-lb-health"
    matcher  = "200"
  }
}
resource "aws_lb_target_group_attachment" "public" {
  count            = local.public ? 1 : 0
  target_group_arn = aws_lb_target_group.public[0].arn
  target_id        = aws_instance.host.id
  port             = 9443
}
resource "aws_lb_listener" "public" {
  count             = local.public ? 1 : 0
  load_balancer_arn = aws_lb.public[0].arn
  port              = 443
  protocol          = "HTTPS"
  ssl_policy        = "ELBSecurityPolicy-TLS13-1-2-2021-06"
  certificate_arn   = var.public_certificate_arn
  default_action {
    type = "fixed-response"
    fixed_response {
      content_type = "text/plain"
      status_code  = "404"
    }
  }
}
# Only the public hostnames are forwarded.
resource "aws_lb_listener_rule" "public" {
  count        = local.public ? 1 : 0
  listener_arn = aws_lb_listener.public[0].arn
  priority     = 10
  condition {
    host_header {
      values = local.public_hosts
    }
  }
  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.public[0].arn
  }
}
output "public_endpoint" {
  description = "Point each public hostname's DNS (CNAME or alias) here."
  value       = local.public ? aws_lb.public[0].dns_name : ""
}
