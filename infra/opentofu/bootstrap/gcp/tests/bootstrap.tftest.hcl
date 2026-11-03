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
  mock_resource "google_project_iam_custom_role" {
    defaults = { id = "projects/example-project/roles/hot_github_plan_read_abc123" }
  }
  mock_data "google_storage_project_service_account" {
    defaults = { email_address = "service-123456789@gs-project-accounts.iam.gserviceaccount.com" }
  }
}
mock_provider "random" {
  mock_resource "random_id" {
    defaults = { hex = "abc123" }
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
  assert {
    condition     = length(google_project_iam_custom_role.plan_read) == 0 && length(google_project_iam_member.plan_policies) == 0
    error_message = "Without a plan identity, nothing gets the plan's read grants."
  }
  assert {
    condition     = length(google_project_service.identity) == 0
    error_message = "State alone enables no APIs."
  }
  assert {
    condition = (one(one(google_storage_bucket.state.lifecycle_rule).condition).num_newer_versions == 20
    && one(one(google_storage_bucket.state.lifecycle_rule).condition).days_since_noncurrent_time == 90)
    error_message = "Old state versions are kept for 90 days, as on AWS, however many newer ones exist."
  }
  assert {
    condition     = strcontains(output.backend_override, "bucket = \"example-hot-tofu-state\"")
    error_message = "The suggested backend names the bucket."
  }
}

run "customer_managed_state_keys" {
  command = plan
  variables {
    kms_key = "projects/example-keys/locations/us/keyRings/hot/cryptoKeys/state"
  }
  assert {
    condition = (google_kms_crypto_key_iam_member.state[0].member == "serviceAccount:service-123456789@gs-project-accounts.iam.gserviceaccount.com"
    && google_kms_crypto_key_iam_member.state[0].role == "roles/cloudkms.cryptoKeyEncrypterDecrypter")
    error_message = "Cloud Storage's service agent may use the key before the bucket needs it."
  }
}

run "github_identities" {
  command = plan
  variables {
    github_repository          = "example/hot-infra"
    github_repository_id       = "456789"
    github_repository_owner_id = "123456"
    apply_roles                = ["roles/compute.admin"]
  }
  assert {
    condition = (strcontains(google_iam_workload_identity_pool_provider.github[0].attribute_condition, "assertion.repository_id == '456789'")
    && strcontains(google_iam_workload_identity_pool_provider.github[0].attribute_condition, "assertion.repository_owner_id == '123456'"))
    error_message = "Only the operator's repository may authenticate, by its immutable IDs."
  }
  assert {
    condition     = strcontains(google_iam_workload_identity_pool_provider.github[0].attribute_condition, "(!assertion.sub.contains(':environment:') || assertion.ref == 'refs/heads/main')")
    error_message = "Apply and release jobs are accepted only from main."
  }
  assert {
    condition     = !contains(keys(google_iam_workload_identity_pool_provider.github[0].attribute_mapping), "attribute.environment")
    error_message = "Nothing maps the environment claim, which jobs without an environment lack."
  }
  assert {
    condition = (toset(keys(google_service_account_iam_member.github)) == toset([
      "apply repo:example/hot-infra:environment:production", "apply repo:example@123456/hot-infra@456789:environment:production",
      "plan repo:example/hot-infra:pull_request", "plan repo:example/hot-infra:ref:refs/heads/main",
      "plan repo:example@123456/hot-infra@456789:pull_request", "plan repo:example@123456/hot-infra@456789:ref:refs/heads/main",
      "release repo:example/hot-infra:environment:production", "release repo:example@123456/hot-infra@456789:environment:production",
    ]))
    error_message = "Plans run for pull requests and main; only the approved environment may apply and release."
  }
  assert {
    condition     = google_service_account_iam_member.github["apply repo:example/hot-infra:environment:production"].member == "principal://iam.googleapis.com/projects/123456789/locations/global/workloadIdentityPools/hot-github/subject/repo:example/hot-infra:environment:production"
    error_message = "Each identity trusts exact subjects, like the AWS roles."
  }
  assert {
    condition = (google_storage_bucket_iam_member.state["plan"].role == "roles/storage.objectViewer" && google_storage_bucket_iam_member.state["apply"].role == "roles/storage.objectUser"
    && google_storage_bucket_iam_member.plan_lock[0].condition[0].expression == "resource.name.endsWith('.tflock')")
    error_message = "Plans read state and take the lock; only apply changes state."
  }
  assert {
    condition = (google_storage_bucket_iam_member.plan_new_state[0].role == "roles/storage.objectCreator"
      && google_storage_bucket_iam_member.plan_new_state[0].member == "serviceAccount:hot-github@example-project.iam.gserviceaccount.com"
    && google_storage_bucket_iam_member.plan_new_state[0].condition[0].expression == "resource.name.endsWith('.tfstate')")
    error_message = "A new installation's first plan can create its empty state, and no plan can overwrite one."
  }
  assert {
    condition     = keys(google_project_service.identity) == ["cloudresourcemanager.googleapis.com", "iam.googleapis.com", "iamcredentials.googleapis.com", "pubsub.googleapis.com", "sts.googleapis.com"] && !google_project_service.identity["iam.googleapis.com"].disable_on_destroy
    error_message = "The identity APIs are enabled on a new project, and left on; the plan identity's custom role also needs Pub/Sub's."
  }
  assert {
    condition = (google_project_iam_member.plan_read[0].role == "roles/viewer"
      && google_project_iam_member.plan_policies[0].role == "projects/example-project/roles/hot_github_plan_read_abc123"
    && google_project_iam_member.plan_policies[0].member == "serviceAccount:hot-github@example-project.iam.gserviceaccount.com")
    error_message = "The plan identity has viewer, and the custom role beside it, so a plan can refresh after the first apply."
  }
  assert {
    condition = toset(google_project_iam_custom_role.plan_read[0].permissions) == toset([
      "pubsub.subscriptions.getIamPolicy", "pubsub.topics.getIamPolicy", "storage.buckets.get", "storage.buckets.getIamPolicy",
    ])
    error_message = "Refresh reads these IAM policies and bucket settings, which viewer lacks, and nothing here reads data."
  }
  assert {
    condition     = google_project_iam_custom_role.plan_read[0].role_id == "hot_github_plan_read_abc123"
    error_message = "The role's ID carries a suffix, since IAM never reuses a deleted role's ID."
  }
}

run "a_hyphenated_prefix_makes_a_valid_role_id" {
  command = plan
  variables {
    operators   = { plan = ["group:ops@example.com"] }
    name_prefix = "hot-prod"
  }
  assert {
    condition     = can(regex("^[a-zA-Z0-9_.]{3,64}$", google_project_iam_custom_role.plan_read[0].role_id)) && startswith(google_project_iam_custom_role.plan_read[0].role_id, "hot_prod_")
    error_message = "IAM role IDs take no hyphens."
  }
  assert {
    condition     = length(google_project_iam_member.plan_policies) == 1
    error_message = "A plan identity for operators alone gets the same read."
  }
  assert {
    condition     = contains(keys(google_project_service.identity), "pubsub.googleapis.com")
    error_message = "A plan identity for operators alone needs the Pub/Sub API for its custom role too."
  }
}

run "github_needs_the_repository_ids" {
  command = plan
  variables {
    github_repository = "example/hot-infra"
    apply_roles       = ["roles/compute.admin"]
  }
  expect_failures = [google_iam_workload_identity_pool.github]
}

run "apply_needs_chosen_roles" {
  command = plan
  variables {
    github_repository          = "example/hot-infra"
    github_repository_id       = "456789"
    github_repository_owner_id = "123456"
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
  assert {
    condition     = length(google_storage_bucket_iam_member.plan_lock) == 0 && length(google_storage_bucket_iam_member.plan_new_state) == 0
    error_message = "Without a plan identity, nothing gets the plan's narrow state grants."
  }
  assert {
    condition     = length(google_project_iam_custom_role.plan_read) == 0 && length(random_id.plan_role) == 0
    error_message = "Without a plan identity, no custom role is created."
  }
  assert {
    condition     = keys(google_project_service.identity) == ["cloudresourcemanager.googleapis.com", "iam.googleapis.com", "iamcredentials.googleapis.com", "sts.googleapis.com"]
    error_message = "Without a plan identity, nothing needs the Pub/Sub API from the bootstrap."
  }
}

run "operators_must_be_iam_members" {
  command = plan
  variables {
    operators = { apply = ["ada@example.com"] }
  }
  expect_failures = [var.operators]
}
