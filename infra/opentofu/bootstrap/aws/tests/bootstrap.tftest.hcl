mock_provider "aws" {
  mock_data "aws_caller_identity" {
    defaults = { account_id = "123456789012" }
  }
}
variables {
  region       = "us-east-1"
  state_bucket = "example-hot-tofu-state"
}

run "state_only" {
  command = plan
  assert {
    condition     = aws_s3_bucket_versioning.state.versioning_configuration[0].status == "Enabled"
    error_message = "State must be versioned."
  }
  assert {
    condition     = aws_s3_bucket_public_access_block.state.block_public_acls && aws_s3_bucket_public_access_block.state.restrict_public_buckets
    error_message = "State must never be public."
  }
  assert {
    condition     = strcontains(aws_s3_bucket_policy.state.policy, "aws:SecureTransport")
    error_message = "State must be reached over TLS only."
  }
  assert {
    condition     = length(aws_iam_role.github) == 0
    error_message = "No GitHub identities without a repository."
  }
}

run "github_identities" {
  command = plan
  variables {
    github_repository = "example/hot-infra"
    apply_policy_arns = ["arn:aws:iam::123456789012:policy/hot-apply"]
  }
  assert {
    condition     = length(aws_iam_role.github) == 3
    error_message = "Plan, apply, and release each get their own role."
  }
  assert {
    condition     = strcontains(aws_iam_role.github["apply"].assume_role_policy, "environment:production") && !strcontains(aws_iam_role.github["apply"].assume_role_policy, "pull_request")
    error_message = "Only the approved environment may apply."
  }
  assert {
    condition     = strcontains(aws_iam_role.github["plan"].assume_role_policy, "repo:example/hot-infra:pull_request")
    error_message = "Plans run for the repository's pull requests."
  }
}

run "apply_needs_chosen_policies" {
  command = plan
  variables {
    github_repository = "example/hot-infra"
  }
  expect_failures = [aws_iam_role.github]
}

run "operators_assume_a_named_role_without_github" {
  command = plan
  variables {
    operators         = { apply = ["arn:aws:iam::123456789012:role/platform-admins"] }
    names             = { apply = "infra-provisioner" }
    apply_policy_arns = ["arn:aws:iam::aws:policy/PowerUserAccess"]
  }
  assert {
    condition     = keys(aws_iam_role.github) == ["apply"] && aws_iam_role.github["apply"].name == "infra-provisioner"
    error_message = "Only the assumed role exists, under the chosen name."
  }
  assert {
    condition     = jsondecode(aws_iam_role.github["apply"].assume_role_policy).Statement[0].Principal.AWS == ["arn:aws:iam::123456789012:role/platform-admins"] && !strcontains(aws_iam_role.github["apply"].assume_role_policy, "Federated")
    error_message = "Only the named operators may assume the role; no GitHub federation without a repository."
  }
  assert {
    condition     = keys(aws_iam_role_policy.state) == ["apply"] && length(aws_iam_openid_connect_provider.github) == 0
    error_message = "The role reads and locks state."
  }
}

run "github_and_operators_share_a_role" {
  command = plan
  variables {
    github_repository = "example/hot-infra"
    operators         = { release = ["arn:aws:iam::123456789012:user/ada"] }
    apply_policy_arns = ["arn:aws:iam::aws:policy/PowerUserAccess"]
  }
  assert {
    condition     = length(jsondecode(aws_iam_role.github["release"].assume_role_policy).Statement) == 2 && length(jsondecode(aws_iam_role.github["apply"].assume_role_policy).Statement) == 1
    error_message = "Operators are added beside GitHub only where they are named."
  }
}

run "operators_must_be_iam_arns" {
  command = plan
  variables {
    operators = { apply = ["ada"] }
  }
  expect_failures = [var.operators]
}
