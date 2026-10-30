mock_provider "google" {
  mock_resource "google_service_account" {
    defaults = {
      name  = "projects/example-project/serviceAccounts/hot-github@example-project.iam.gserviceaccount.com"
      email = "hot-github@example-project.iam.gserviceaccount.com"
    }
  }
  mock_resource "google_iam_workload_identity_pool" {
    defaults = { name = "projects/123456789/locations/global/workloadIdentityPools/hot-github" }
  }
}
variables {
  project      = "example-project"
  state_bucket = "example-hot-tofu-state"
}

run "state_only" {
  command = plan
  assert {
    condition     = google_storage_bucket.state.versioning[0].enabled && google_storage_bucket.state.public_access_prevention == "enforced"
    error_message = "State must be versioned and never public."
  }
  assert {
    condition     = google_storage_bucket.state.uniform_bucket_level_access
    error_message = "State access is controlled by IAM only."
  }
  assert {
    condition     = length(google_service_account.github) == 0
    error_message = "No GitHub identities without a repository."
  }
}

run "github_identities" {
  command = plan
  variables {
    github_repository = "example/hot-infra"
    apply_roles       = ["roles/compute.admin"]
  }
  assert {
    condition     = google_iam_workload_identity_pool_provider.github[0].attribute_condition == "assertion.repository == 'example/hot-infra'"
    error_message = "Only the operator's repository may authenticate."
  }
  assert {
    condition     = strcontains(google_service_account_iam_member.github["apply"].member, "attribute.environment/production")
    error_message = "Only the approved environment may apply."
  }
}

run "apply_needs_chosen_roles" {
  command = plan
  variables {
    github_repository = "example/hot-infra"
  }
  expect_failures = [google_service_account.github]
}

run "operators_impersonate_a_named_identity_without_github" {
  command = plan
  variables {
    operators   = { apply = ["user:ada@example.com", "group:ops@example.com"] }
    names       = { apply = "infra-provisioner" }
    apply_roles = ["roles/compute.admin"]
  }
  assert {
    condition     = keys(google_service_account.github) == ["apply"] && google_service_account.github["apply"].account_id == "infra-provisioner"
    error_message = "Only the impersonated identity exists, under the chosen name."
  }
  assert {
    condition     = length(google_service_account_iam_member.github) == 0 && length(google_iam_workload_identity_pool.github) == 0
    error_message = "No GitHub federation without a repository."
  }
  assert {
    condition     = length(google_service_account_iam_member.operators) == 2 && google_service_account_iam_member.operators["apply user:ada@example.com"].role == "roles/iam.serviceAccountTokenCreator"
    error_message = "Each operator may mint short-lived tokens for the identity, and nothing more."
  }
  assert {
    condition     = keys(google_storage_bucket_iam_member.state) == ["apply"] && length(google_project_iam_member.apply) == 1
    error_message = "The identity reads and locks state and has the chosen roles."
  }
}

run "operators_must_be_iam_members" {
  command = plan
  variables {
    operators = { apply = ["ada@example.com"] }
  }
  expect_failures = [var.operators]
}
