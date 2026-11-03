mock_provider "aws" {
  mock_data "aws_caller_identity" {
    defaults = { account_id = "123456789012" }
  }
  mock_resource "aws_s3_bucket" {
    defaults = { id = "example-hot-tofu-state", arn = "arn:aws:s3:::example-hot-tofu-state" }
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
    github_repository          = "example/hot-infra"
    github_repository_id       = "456789"
    github_repository_owner_id = "123456"
    apply_policy_arns          = ["arn:aws:iam::123456789012:policy/hot-apply"]
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
    condition = (jsondecode(aws_iam_role.github["plan"].assume_role_policy).Statement[0].Condition.StringEquals["token.actions.githubusercontent.com:sub"] == [
      "repo:example/hot-infra:pull_request", "repo:example/hot-infra:ref:refs/heads/main",
      "repo:example@123456/hot-infra@456789:pull_request", "repo:example@123456/hot-infra@456789:ref:refs/heads/main",
    ])
    error_message = "Plans run for the repository's pull requests and main, in either subject format."
  }
  assert {
    condition = alltrue([for role in ["plan", "apply", "release"] :
      jsondecode(aws_iam_role.github[role].assume_role_policy).Statement[0].Condition.StringEquals["token.actions.githubusercontent.com:repository_id"] == "456789"
    && jsondecode(aws_iam_role.github[role].assume_role_policy).Statement[0].Condition.StringEquals["token.actions.githubusercontent.com:repository_owner_id"] == "123456"])
    error_message = "Trust is pinned to the repository's immutable IDs, not only its name."
  }
  assert {
    condition = (jsondecode(aws_iam_role.github["apply"].assume_role_policy).Statement[0].Condition.StringEquals["token.actions.githubusercontent.com:ref"] == "refs/heads/main"
      && jsondecode(aws_iam_role.github["release"].assume_role_policy).Statement[0].Condition.StringEquals["token.actions.githubusercontent.com:ref"] == "refs/heads/main"
    && !can(jsondecode(aws_iam_role.github["plan"].assume_role_policy).Statement[0].Condition.StringEquals["token.actions.githubusercontent.com:ref"]))
    error_message = "Apply and release run only from main; plans also run for pull requests."
  }
  assert {
    condition     = jsondecode(aws_iam_role_policy.state["plan"].policy).Statement[2].Resource == "arn:aws:s3:::example-hot-tofu-state/*.tflock" && jsondecode(aws_iam_role_policy.state["apply"].policy).Statement[2].Resource == "arn:aws:s3:::example-hot-tofu-state/*"
    error_message = "Plans read state and take the lock; only apply writes state."
  }
  assert {
    condition     = jsondecode(aws_iam_role_policy.plan_no_data[0].policy).Statement[0].NotResource == "arn:aws:s3:::example-hot-tofu-state/*" && contains(jsondecode(aws_iam_role_policy.plan_no_data[0].policy).Statement[1].Action, "logs:FilterLogEvents")
    error_message = "Plans read no objects but state, and no logs."
  }
  assert {
    condition     = aws_iam_role.github["apply"].max_session_duration == 14400 && aws_iam_role.github["release"].max_session_duration == 14400
    error_message = "A long apply, release, or command can ask for four hours."
  }
  assert {
    condition     = aws_iam_role.github["plan"].max_session_duration == 3600
    error_message = "Plans keep the default hour."
  }
}

run "the_backend_takes_the_lock" {
  command = plan
  assert {
    condition     = strcontains(output.backend_override, "use_lockfile = true") && strcontains(output.backend_override, "bucket       = \"example-hot-tofu-state\"") && !strcontains(output.backend_override, "kms_key_id")
    error_message = "The suggested backend locks state."
  }
}

run "a_kms_backend_names_the_key" {
  command = plan
  variables {
    kms_key_arn = "arn:aws:kms:us-east-1:123456789012:key/example"
  }
  assert {
    condition     = strcontains(output.backend_override, "kms_key_id   = \"arn:aws:kms:us-east-1:123456789012:key/example\"\n")
    error_message = "State written through the backend uses the bucket's key."
  }
}

run "github_needs_the_repository_ids" {
  command = plan
  variables {
    github_repository = "example/hot-infra"
    apply_policy_arns = ["arn:aws:iam::123456789012:policy/hot-apply"]
  }
  expect_failures = [aws_iam_role.github]
}

run "apply_needs_chosen_policies" {
  command = plan
  variables {
    github_repository          = "example/hot-infra"
    github_repository_id       = "456789"
    github_repository_owner_id = "123456"
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
    condition     = aws_iam_role.github["apply"].max_session_duration == 14400
    error_message = "An operator's role allows the same long session as GitHub's."
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
    github_repository          = "example/hot-infra"
    github_repository_id       = "456789"
    github_repository_owner_id = "123456"
    operators                  = { release = ["arn:aws:iam::123456789012:user/ada"] }
    apply_policy_arns          = ["arn:aws:iam::aws:policy/PowerUserAccess"]
  }
  assert {
    condition     = length(jsondecode(aws_iam_role.github["release"].assume_role_policy).Statement) == 2 && length(jsondecode(aws_iam_role.github["apply"].assume_role_policy).Statement) == 1
    error_message = "Operators are added beside GitHub only where they are named."
  }
  assert {
    condition     = jsondecode(aws_iam_role.github["release"].assume_role_policy).Statement[1].Condition.Bool["aws:MultiFactorAuthPresent"] == "true"
    error_message = "An IAM user assumes a deploy role only with MFA."
  }
}

run "account_roots_are_not_operators" {
  command = plan
  variables {
    operators = { apply = ["arn:aws:iam::123456789012:root"] }
  }
  expect_failures = [var.operators]
}

run "operators_must_be_iam_arns" {
  command = plan
  variables {
    operators = { apply = ["ada"] }
  }
  expect_failures = [var.operators]
}
