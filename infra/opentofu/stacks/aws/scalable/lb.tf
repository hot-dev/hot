# App and API endpoints: one passthrough Network Load Balancer each, internal
# by default and internet-facing when the role's exposure is public. It keeps
# the client's address, and TLS ends on the hosts' ingress (port 443 there
# routes by Host header), so no certificate or key enters OpenTofu, and long
# streams see no proxy timeout.
resource "aws_lb" "web" {
  for_each                         = local.web_roles
  name                             = substr("hot-${local.config.name}-${each.key}", 0, 32)
  load_balancer_type               = "network"
  internal                         = local.exposure[each.key] != "public"
  subnets                          = local.exposure[each.key] == "public" ? local.public_subnet_ids : local.private_subnet_ids
  enable_cross_zone_load_balancing = true
  lifecycle {
    precondition {
      condition     = local.exposure[each.key] != "public" || length(local.public_subnet_ids) >= 2
      error_message = "A public endpoint on an existing network needs public_subnet_ids in two or more zones."
    }
  }
}
resource "aws_lb_target_group" "web" {
  for_each    = local.web_roles
  name        = substr("hot-${local.config.name}-${each.key}", 0, 32)
  port        = 443
  protocol    = "TCP"
  vpc_id      = local.vpc_id
  target_type = "instance"
  # In-flight uploads and streams finish before an instance leaves.
  deregistration_delay = 300
  preserve_client_ip   = true
  health_check {
    protocol            = "HTTPS"
    port                = "traffic-port"
    path                = "/hot-lb-health"
    healthy_threshold   = 2
    unhealthy_threshold = 3
    interval            = 10
  }
}
resource "aws_lb_listener" "web" {
  for_each          = local.web_roles
  load_balancer_arn = aws_lb.web[each.key].arn
  port              = 443
  protocol          = "TCP"
  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.web[each.key].arn
  }
}
output "endpoints" {
  description = "Point app_hostname and api_hostname at these names (CNAME or alias)."
  value       = { for role, lb in aws_lb.web : role => lb.dns_name }
}
