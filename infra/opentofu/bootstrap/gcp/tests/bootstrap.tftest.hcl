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
  expect_failures = [google_project_iam_member.plan_read]
}
