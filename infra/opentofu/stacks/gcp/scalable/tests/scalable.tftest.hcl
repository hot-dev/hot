mock_provider "google" {
  mock_resource "google_memorystore_instance" {
    defaults = {
      endpoints = [{ connections = [
        { psc_auto_connection = [{ connection_type = "CONNECTION_TYPE_DISCOVERY", ip_address = "10.68.0.39", port = 6379, forwarding_rule = "", network = "", project_id = "", psc_connection_id = "", service_attachment = "" }] },
        { psc_auto_connection = [{ connection_type = "CONNECTION_TYPE_PRIMARY", ip_address = "10.68.0.40", port = 6379, forwarding_rule = "", network = "", project_id = "", psc_connection_id = "", service_attachment = "" }] },
      ] }]
      managed_server_ca = [{ ca_certs = [{ certificates = ["-----BEGIN CERTIFICATE-----\nMIIC\n-----END CERTIFICATE-----\n"] }] }]
    }
  }
  mock_resource "google_compute_network" {
    defaults = { id = "projects/example-project/global/networks/hot-example" }
  }
  mock_resource "google_compute_global_address" {
    defaults = { address = "10.100.0.0" }
  }
  mock_resource "google_sql_database_instance" {
    defaults = {
      private_ip_address = "10.100.0.3"
      server_ca_cert     = [{ cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n", common_name = "C=US,O=Google\\, Inc,CN=Google Cloud SQL Server CA", create_time = "2026-10-29T00:00:00Z", expiration_time = "2036-10-27T00:00:00Z", sha1_fingerprint = "00" }]
    }
  }
}
variables {
  acknowledge_unqualified     = true
  state_encryption_passphrase = "test-only-encryption-passphrase-never-use-in-production"
  release_principal           = "hot-github-release@example-project.iam.gserviceaccount.com"
  installation = {
    "schema_version" : 1,
    "preset" : "scalable",
    "name" : "hot-example",
    "cloud" : "gcp",
    "region" : "us-central1",
    "zone" : "us-central1-a",
    "size" : "small",
    "host_image" : "projects/example-project/global/images/hot-host-amd64",
    "project" : "example-project",
    "app_hostname" : "app.hot.internal",
    "api_hostname" : "api.hot.internal",
    "api_exposure" : "public",
    "network" : { "client_cidrs" : ["10.10.0.0/16"] },
    "data" : { "postgres" : { "mode" : "managed", "high_availability" : true }, "valkey" : { "mode" : "managed", "high_availability" : true } },
    "storage" : {
      "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
      "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files", "provision" : true }
    },
    "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
  }
}

run "each_role_runs_in_its_own_group" {
  command = plan
  assert {
    condition     = keys(google_compute_region_instance_group_manager.role) == ["api", "app", "task-worker", "worker"] && google_compute_instance_group_manager.scheduler.target_size == 1
    error_message = "App, API, worker, and task-worker run in regional groups; the scheduler alone in one zone."
  }
  assert {
    condition     = google_compute_instance_group_manager.scheduler.update_policy[0].max_surge_fixed == 0 && google_compute_instance_group_manager.scheduler.update_policy[0].replacement_method == "RECREATE"
    error_message = "The scheduler is replaced stop-before-start, so two never run at once."
  }
  assert {
    condition     = google_compute_instance_template.role["task-worker"].advanced_machine_features[0].enable_nested_virtualization && !google_compute_instance_template.role["app"].advanced_machine_features[0].enable_nested_virtualization
    error_message = "Only task-workers run Kata VMs."
  }
  assert {
    condition     = google_compute_instance_template.role["task-worker"].machine_type == "n2-standard-2" && google_compute_instance_template.role["app"].machine_type == "e2-standard-2"
    error_message = "Task-workers use the size's nested-virtualization family; other roles general-purpose machines."
  }
  assert {
    condition     = alltrue([for t in google_compute_instance_template.role : length(t.network_interface[0].access_config) == 0])
    error_message = "No host has an external address."
  }
  assert {
    condition     = google_compute_router_nat.egress[0].enable_dynamic_port_allocation && !google_compute_router_nat.egress[0].enable_endpoint_independent_mapping && google_compute_router_nat.egress[0].min_ports_per_vm == 256 && google_compute_router_nat.egress[0].max_ports_per_vm == 8192
    error_message = "Hosts get NAT ports on demand beyond the default 64 per destination."
  }
}

run "release_principal_bindings_are_this_installations" {
  command = plan
  assert {
    condition     = keys(google_project_iam_member.release) == ["roles/compute.viewer", "roles/logging.viewer", "roles/monitoring.alertPolicyViewer", "roles/monitoring.snoozeEditor"] && alltrue([for m in google_project_iam_member.release : m.condition[0].title == "hot-hot-example-release"])
    error_message = "The release account's project roles are this installation's own bindings, titled as on the single machine, so a shared account keeps other installations' grants and a preset move keeps these."
  }
  assert {
    condition     = toset([for key, binding in google_secret_manager_secret_iam_member.release : key if binding.role == "roles/secretmanager.viewer"]) == toset([for role in concat(keys(local.roles), ["migrate"]) : "${role} roles/secretmanager.viewer"])
    error_message = "A re-run release reads when each pointer was written, which needs the viewer role on every pointer, under keys that an upgrade does not replace."
  }
  assert {
    condition     = alltrue([for role in concat(keys(local.roles), ["migrate"]) : alltrue([for grant in ["roles/secretmanager.secretVersionAdder", "roles/secretmanager.secretAccessor"] : contains(keys(google_secret_manager_secret_iam_member.release), "${role} ${grant}")])])
    error_message = "The release account adds and reads every pointer, under keys that an upgrade does not replace."
  }
}

run "no_release_account_no_pointer_grants" {
  command = plan
  variables {
    release_principal = ""
  }
  assert {
    condition     = length(google_secret_manager_secret_iam_member.release) == 0
    error_message = "Without a release account, no one is granted the release pointers."
  }
}

run "hosts_know_their_role_and_pointers" {
  command = plan
  assert {
    condition     = local.host_config["app"].roles == ["app"] && !local.host_config["app"].migrations && local.host_config["app"].migration_pointer == ""
    error_message = "An app host runs only the app and never migrates."
  }
  assert {
    condition     = local.host_config["scheduler"].migrations && local.host_config["scheduler"].migration_pointer == google_secret_manager_secret.pointer["migrate"].id
    error_message = "The scheduler's host migrates, from the migration pointer."
  }
  assert {
    condition     = local.host_config["worker"].release_pointer == google_secret_manager_secret.pointer["worker"].id && local.host_config["worker"].data_disk == "scratch" && local.host_config["worker"].db_pool == 20
    error_message = "Each role follows its own pointer, on a scratch disk, with a bounded database pool."
  }
  assert {
    condition     = length(google_secret_manager_secret.pointer) == 6 && length(google_service_account.role) == 5
    error_message = "A pointer per role plus the migration pointer, and an identity per role."
  }
  assert {
    condition     = google_service_account.role["app"].account_id == "hot-hot-example-app" && google_service_account.role["task-worker"].account_id == "hot-hot-example-tsk"
    error_message = "A name that fits keeps its whole tag in the account IDs."
  }
  assert {
    condition     = google_secret_manager_secret_iam_member.postgres_admin[0].member == "serviceAccount:${google_service_account.role["scheduler"].email}"
    error_message = "Only the migration host reads the database administrator's secret."
  }
  assert {
    condition     = keys(google_secret_manager_secret_iam_member.store_generated) == ["scheduler"] && google_secret_manager_secret_iam_member.store_generated["scheduler"].member == "serviceAccount:${google_service_account.role["scheduler"].email}"
    error_message = "Only the migration host may add a version of the generated secret; every other role only reads it."
  }
  assert {
    condition     = length(google_project_iam_member.valkey) == 5
    error_message = "Every role reaches Valkey."
  }
  assert {
    condition     = local.host_config["worker"].data.postgres.endpoint.deny_cidrs == tolist(["10.100.0.0/20"]) && alltrue([for config in values(local.host_config) : config.data.valkey.endpoint.deny_cidrs == tolist(["10.68.0.39/32", "10.68.0.40/32"]) && config.data.postgres.endpoint.deny_cidrs == tolist(["10.100.0.0/20"])])
    error_message = "User code is denied the private services range Cloud SQL's addresses come from, and every Private Service Connect address of the cache, on every role's host."
  }
}

run "provisioned_bucket_policy_is_the_stacks_own" {
  command = plan
  override_resource {
    target = google_service_account.role["app"]
    values = { email = "app@example-project.iam.gserviceaccount.com" }
  }
  override_resource {
    target = google_service_account.role["api"]
    values = { email = "api@example-project.iam.gserviceaccount.com" }
  }
  override_resource {
    target = google_service_account.role["worker"]
    values = { email = "worker@example-project.iam.gserviceaccount.com" }
  }
  override_resource {
    target = google_service_account.role["scheduler"]
    values = { email = "scheduler@example-project.iam.gserviceaccount.com" }
  }
  override_resource {
    target = google_service_account.role["task-worker"]
    values = { email = "task-worker@example-project.iam.gserviceaccount.com" }
  }
  assert {
    condition = (length(google_storage_bucket_iam_member.existing) == 0
      && [for b in data.google_iam_policy.storage["acme-hot"].binding : b.members if b.role == "roles/storage.objectUser"] == [for prefix in ["builds", "files"] : toset([for role in ["app", "api", "worker", "scheduler", "task-worker"] : "serviceAccount:${role}@example-project.iam.gserviceaccount.com"])]
    && strcontains(jsonencode(data.google_iam_policy.storage["acme-hot"].binding), "/objects/builds/") && strcontains(jsonencode(data.google_iam_policy.storage["acme-hot"].binding), "/objects/files/"))
    error_message = "Every role reaches both storage prefixes, through one conditional binding per prefix in the bucket's policy."
  }
  assert {
    condition     = google_storage_bucket_iam_policy.storage["acme-hot"].bucket == "acme-hot" && strcontains(file("${path.module}/storage.tf"), "policy_data = data.google_iam_policy.storage[each.key].policy_data")
    error_message = "The bucket's whole policy comes from the stack."
  }
  assert {
    condition     = !strcontains(jsonencode(data.google_iam_policy.storage["acme-hot"].binding), "projectViewer") && !strcontains(jsonencode(data.google_iam_policy.storage["acme-hot"].binding), "legacyObjectReader") && !strcontains(jsonencode(data.google_iam_policy.storage["acme-hot"].binding), "legacyBucketReader")
    error_message = "Project viewers, which a pull request's plan is, must not read a provisioned bucket's objects or list their names."
  }
  assert {
    condition = alltrue([for role in ["roles/storage.legacyBucketOwner", "roles/storage.legacyObjectOwner"] : one([
      for b in data.google_iam_policy.storage["acme-hot"].binding : b.members if b.role == role
    ]) == toset(["projectOwner:example-project", "projectEditor:example-project"])])
    error_message = "Project owners and editors keep the bucket, since an apply identity may manage it through them."
  }
  assert {
    condition     = strcontains(regex("(?s)resource \"google_storage_bucket_iam_policy\" \"storage\" \\{.*?\\n\\}", file("${path.module}/storage.tf")), "prevent_destroy = true") && strcontains(regex("(?s)resource \"google_storage_bucket_iam_policy\" \"storage\" \\{.*?\\n\\}", file("${path.module}/storage.tf")), "depends_on = [google_storage_bucket_iam_member.existing]")
    error_message = "Destroying the policy would empty the bucket's, even of its owners, so it is kept like the bucket; and an existing bucket's binding goes before the policy is set, not after."
  }
  assert {
    condition     = strcontains(file("${path.module}/storage.tf"), "removed {\n  from = google_storage_bucket_iam_member.storage\n  lifecycle { destroy = false }\n}")
    error_message = "Earlier versions' bindings are forgotten, not destroyed: destroying one after the policy is set would take the hosts' access out of it."
  }
}

run "existing_buckets_and_network_list_their_own" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "network" : {
        "mode" : "existing",
        "network_id" : "projects/example-project/global/networks/customer-private",
        "subnet_id" : "projects/example-project/regions/us-central1/subnetworks/customer-private"
      },
      "data" : {
        "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.20.0.0/20"] } },
        "valkey" : { "mode" : "managed" }
      },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-shared", "prefix" : "hot/builds" },
        "file" : { "backend" : "gcs", "bucket" : "acme-shared", "prefix" : "hot/files" }
      }
    }
  }
  assert {
    condition     = length(google_storage_bucket_iam_member.existing) == 10 && length(google_storage_bucket_iam_policy.storage) == 0 && strcontains(google_storage_bucket_iam_member.existing["app-file"].condition[0].expression, "buckets/acme-shared/objects/hot/files/")
    error_message = "An existing bucket's policy stays the operator's: every role gets a binding on each prefix, and nothing else changes."
  }
  assert {
    condition     = length(google_compute_global_address.services) == 0 && local.host_config["worker"].data.postgres.endpoint.deny_cidrs == tolist(["10.20.0.0/20"]) && local.host_config["worker"].data.valkey.endpoint.deny_cidrs == tolist(["10.68.0.39/32", "10.68.0.40/32"])
    error_message = "On an existing network the operator's entry names the private services range; the cache's addresses are the stack's to know."
  }
}

run "operator_entries_join_what_the_stack_knows" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : {
        "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.100.0.0/20", "10.20.0.0/20"] } },
        "valkey" : { "mode" : "existing", "endpoint" : { "deny_cidrs" : ["10.30.0.0/24"] } }
      },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds" },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files" }
      }
    }
  }
  assert {
    condition     = alltrue([for config in values(local.host_config) : config.data.postgres.endpoint.deny_cidrs == tolist(["10.100.0.0/20", "10.20.0.0/20"]) && config.data.postgres.endpoint.port == 5432])
    error_message = "A managed database's endpoint keeps the operator's entries, with the range the stack reserves added once."
  }
  assert {
    condition     = alltrue([for config in values(local.host_config) : config.data.valkey.endpoint.deny_cidrs == tolist(["10.30.0.0/24"]) && !can(config.data.valkey.endpoint.host)])
    error_message = "An existing service's entries reach every host; its connection settings stay in the operator secret."
  }
}

run "existing_services_without_entries_have_no_endpoint" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "existing" }, "valkey" : { "mode" : "existing" } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds" },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files" }
      }
    }
  }
  assert {
    condition     = alltrue([for config in values(local.host_config) : config.data.postgres.endpoint == null && config.data.valkey.endpoint == null])
    error_message = "With nothing to deny, the document stays as it was, so no host changes for it."
  }
}

run "endpoints_and_scaling" {
  command = plan
  assert {
    condition     = google_compute_region_backend_service.web["app"].load_balancing_scheme == "INTERNAL" && google_compute_region_backend_service.web["api"].load_balancing_scheme == "EXTERNAL"
    error_message = "Each endpoint's exposure is independent: the app private, the API public."
  }
  assert {
    condition     = google_compute_forwarding_rule.web["app"].ports == toset(["443"]) && google_compute_region_health_check.web.https_health_check[0].request_path == "/hot-lb-health"
    error_message = "Passthrough on 443 to the hosts' ingress, health-checked through it."
  }
  assert {
    condition     = google_compute_firewall.web["api"].source_ranges == toset(["0.0.0.0/0"]) && contains(google_compute_firewall.web["app"].source_ranges, "10.10.0.0/16") && !contains(google_compute_firewall.web["app"].source_ranges, "0.0.0.0/0")
    error_message = "A private endpoint admits its client ranges and health checks only."
  }
  assert {
    condition     = google_compute_region_autoscaler.role["app"].autoscaling_policy[0].min_replicas == 2 && google_compute_region_autoscaler.role["task-worker"].autoscaling_policy[0].mode == "ONLY_SCALE_OUT" && google_compute_region_autoscaler.role["worker"].autoscaling_policy[0].max_replicas == 4
    error_message = "App and API autoscale from two; task-workers only scale out."
  }
  assert {
    condition     = google_compute_region_autoscaler.role["worker"].autoscaling_policy[0].mode == "ONLY_SCALE_OUT" && google_compute_region_autoscaler.role["app"].autoscaling_policy[0].mode == "ON" && google_compute_region_autoscaler.role["api"].autoscaling_policy[0].mode == "ON"
    error_message = "Autoscaled workers only scale out, since nothing drains a host before the group removes it; app and API scale both ways."
  }
  assert {
    condition     = length(google_compute_region_autoscaler.role) == 4 && !contains(keys(google_compute_region_autoscaler.role), "scheduler")
    error_message = "Every role group but the scheduler's has an autoscaler, so a new count takes effect even when fixed."
  }
  assert {
    condition     = google_monitoring_alert_policy.scheduler.display_name == "hot-hot-example-scheduler-stopped" && length(google_monitoring_alert_policy.hot) == 12 && length(jsondecode(google_monitoring_dashboard.hot.dashboard_json).mosaicLayout.tiles) == 10 && one(google_monitoring_alert_policy.database_connections[0].conditions).condition_threshold[0].threshold_value == 320
    error_message = "The singleton scheduler and every per-host signal alert."
  }
  assert {
    condition     = alltrue([for tile in jsondecode(google_monitoring_dashboard.hot.dashboard_json).mosaicLayout.tiles : try(tile.xPos, 1) != 0 && try(tile.yPos, 1) != 0])
    error_message = "The API omits zero values from a dashboard it returns, so a tile that sends a zero position shows as a change in every plan."
  }
  assert {
    condition     = [for tile in jsondecode(google_monitoring_dashboard.hot.dashboard_json).mosaicLayout.tiles : [try(tile.xPos, 0), try(tile.yPos, 0)]] == [for index in range(10) : [(index % 2) * 6, floor(index / 2) * 4]]
    error_message = "Leaving out zero positions keeps the tiles on the same two-column grid."
  }
  assert {
    condition     = one(google_compute_region_autoscaler.role["worker"].autoscaling_policy[0].metric).single_instance_assignment == 50 && one(google_compute_region_autoscaler.role["worker"].autoscaling_policy[0].metric).filter == "resource.type = \"global\"" && endswith(one(google_compute_region_autoscaler.role["worker"].autoscaling_policy[0].metric).name, "/jobs_waiting")
    error_message = "Workers also scale on the jobs waiting per host, an installation-wide series."
  }
  assert {
    condition     = one(google_compute_region_autoscaler.role["task-worker"].autoscaling_policy[0].metric).target == 70 && endswith(one(google_compute_region_autoscaler.role["task-worker"].autoscaling_policy[0].metric).name, "/task_slots_used_percent") && length(google_compute_region_autoscaler.role["app"].autoscaling_policy[0].metric) == 0
    error_message = "Task-workers scale out on the share of their slots in use."
  }
  assert {
    condition     = alltrue([for policy in google_monitoring_alert_policy.hot : policy.alert_strategy[0].auto_close == "1800s"])
    error_message = "An incident on a host that scale-in or a rolling update removed closes after half an hour without data, not a week."
  }
  assert {
    condition     = one(google_sql_database_instance.postgres[0].settings[0].database_flags).name == "max_connections" && one(google_sql_database_instance.postgres[0].settings[0].database_flags).value == "640"
    error_message = "Cloud SQL accepts the connections the scaling maximums were checked against, and 20 for each of the 3 extra hosts a rolling update adds to each of the 4 regional groups."
  }
  assert {
    condition     = google_memorystore_instance.valkey[0].replica_count == 1 && one(google_memorystore_instance.valkey[0].zone_distribution_config).mode == "MULTI_ZONE"
    error_message = "High availability adds a replica; the zone distribution, which cannot change, does not depend on it."
  }
}

# IAM cannot limit a host to one instance group (see the autoscalers' comment),
# so task hosts get no access to any, even with the settings that retire them on
# AWS. These runs' defaults have a maximum above the minimum and the default
# retire_idle_minutes.
run "task_hosts_have_no_grant_on_instance_groups" {
  command = plan
  assert {
    condition     = local.roles["task-worker"].max > local.roles["task-worker"].min && local.config.scaling.task_worker.retire_idle_minutes > 0 && google_compute_region_autoscaler.role["task-worker"].autoscaling_policy[0].mode == "ONLY_SCALE_OUT"
    error_message = "With retirement asked for, the group still only scales out: scaling in could cut off running tasks."
  }
  assert {
    condition     = [for key, grant in google_project_iam_member.telemetry : grant.role if startswith(key, "task-worker ")] == ["roles/logging.logWriter", "roles/monitoring.metricWriter"] && google_project_iam_member.valkey["task-worker"].role == "roles/memorystore.dbConnectionUser"
    error_message = "A task host's project-wide roles are telemetry and the cache alone."
  }
  assert {
    condition     = alltrue([for grant in google_project_iam_member.release : grant.member == "serviceAccount:hot-github-release@example-project.iam.gserviceaccount.com"])
    error_message = "The project-wide compute viewer role is the release account's, not a host's."
  }
  assert {
    condition     = toset(flatten(regexall("resource \"google_project_iam_member\" \"(\\w+)\"", join("\n", [for name in fileset(path.module, "*.tf") : file("${path.module}/${name}")])))) == toset(["release", "telemetry", "valkey"])
    error_message = "A new project-wide grant must be checked here: it could give a task host, which runs untrusted boxes, power over other groups."
  }
  assert {
    condition     = !can(regex("instanceGroupManagers|google_project_iam_custom_role", join("\n", [for name in fileset(path.module, "*.tf") : file("${path.module}/${name}")])))
    error_message = "No role of this stack may list instance group permissions: nothing could limit them to one group."
  }
  assert {
    condition     = alltrue([for config in values(local.host_config) : !contains(keys(config), "instance_group")])
    error_message = "A host has no group to leave here, so its configuration names none."
  }
}

run "fixed_task_worker_count" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "medium",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "task_worker" : { "min" : 2, "max" : 2 } }
    }
  }
  assert {
    condition     = google_compute_region_autoscaler.role["task-worker"].autoscaling_policy[0].mode == "ON" && google_compute_region_autoscaler.role["task-worker"].autoscaling_policy[0].max_replicas == 2
    error_message = "A fixed task-worker count lets the autoscaler scale in, so a lower count takes effect."
  }
  assert {
    condition     = google_compute_region_autoscaler.role["worker"].autoscaling_policy[0].mode == "ON" && google_compute_region_autoscaler.role["worker"].autoscaling_policy[0].max_replicas == google_compute_region_autoscaler.role["worker"].autoscaling_policy[0].min_replicas
    error_message = "A fixed worker count lets the autoscaler scale in, so a lower count takes effect."
  }
  assert {
    condition     = one(google_sql_database_instance.postgres[0].settings[0].database_flags).value == "1040" && one(google_monitoring_alert_policy.database_connections[0].conditions).condition_threshold[0].threshold_value == 640
    error_message = "A 16 GB tier defaults to 500 connections; the database takes the medium budget of 800 and a rolling update's 240, and the alert is 80% of the budget."
  }
}

run "long_names_keep_distinct_accounts" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "northwind-production-eu",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files", "provision" : true }
      }
    }
  }
  assert {
    # northwind-production-us gets hot-northwind-produc-b9c85-app.
    condition     = google_service_account.role["app"].account_id == "hot-northwind-produc-15c99-app" && alltrue([for a in google_service_account.role : length(a.account_id) <= 30])
    error_message = "A name too long for the whole tag keeps a hash of itself, so names sharing their first 20 characters get different accounts."
  }
}

run "refuses_a_single_machine_installation" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "project" : "example-project",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_another_cloud" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_misspelled_data_service" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" }, "postgresql" : { "endpoint" : { "deny_cidrs" : ["10.20.0.0/20"] } } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_deny_list_outside_postgres_endpoint" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed", "deny_cidrs" : ["10.20.0.0/20"] }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_deny_list_outside_valkey_endpoint" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed", "deny_cidrs" : ["10.30.0.0/24"] } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_misspelled_postgres_endpoint_key" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidr" : ["10.20.0.0/20"] } }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_misspelled_valkey_endpoint_key" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed", "endpoint" : { "deny_cidr" : ["10.30.0.0/24"] } } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  expect_failures = [var.installation]
}

run "accepts_every_data_key_the_contract_knows" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : {
        "postgres" : { "mode" : "managed", "runtime_role" : "hot_runtime", "version" : "18", "tier" : "", "storage_gb" : 0, "high_availability" : true, "backup_retention_days" : 7, "deletion_protection" : true, "admin_password_version" : 1, "endpoint" : null },
        "valkey" : { "mode" : "managed", "node_type" : "", "high_availability" : true, "auth_token_version" : 1, "endpoint" : { "deny_cidrs" : ["10.30.0.0/24"] } }
      },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  assert {
    condition     = local.host_config["worker"].data.postgres.mode == "managed" && contains(local.host_config["worker"].data.valkey.endpoint.deny_cidrs, "10.30.0.0/24")
    error_message = "Every key in the contract's data object, and a null endpoint, must pass the unknown-key check."
  }
}

run "every_role_pulls_from_the_mirror" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "image_registry" : "us-docker.pkg.dev/acme-mirror/hot",
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files", "provision" : true }
      }
    }
  }
  assert {
    condition     = length(google_artifact_registry_repository_iam_member.mirror) == 5 && alltrue([for m in google_artifact_registry_repository_iam_member.mirror : m.repository == "hot" && m.role == "roles/artifactregistry.reader"])
    error_message = "Every role's account may read the mirror repository."
  }
}

# The order of an apply does not show in a plan, and prevent_destroy stops a
# test from applying and cleaning up. This reads the configuration instead.
run "compute_waits_for_its_api" {
  command = plan
  assert {
    condition = alltrue([for resource in [
      "google_compute_address\" \"egress",
      "google_compute_region_health_check\" \"web",
    ] : strcontains(regex("(?s)resource \"${resource}\" \\{.*?\\n\\}", "${file("${path.module}/main.tf")}\n${file("${path.module}/lb.tf")}"), "= [google_project_service.required]")])
    error_message = "A Compute resource that refers to nothing else must wait for the API, or the first apply on a project with it off fails."
  }
}

# expect_failures does not see a refusal's message, so this reads the configuration
# too. The last runs below show that this flag does get past both refusals.
run "each_storage_refusal_names_the_way_past" {
  command = plan
  assert {
    condition = alltrue([for record in ["storage_backends", "storage_locations"] :
      strcontains(regex("(?s)resource \"terraform_data\" \"${record}\" \\{.*?\\n\\}", file("${path.module}/storage.tf")), "tofu apply -replace=terraform_data.storage_backends.")
    ])
    error_message = "A first bucket that was wrong can never be put back, so each refusal must say how an installation that has stored nothing starts again."
  }
}

# The refusals need a state, so these last runs apply the records alone. Nothing
# with prevent_destroy joins them, so the test can still clean up. This first
# apply leaves the state of an installation from before the location record.
run "the_first_apply_records_each_storage_backend" {
  command = apply
  plan_options {
    target = [terraform_data.storage_backends]
  }
  assert {
    condition     = terraform_data.storage_backends.input.build == "gcs" && terraform_data.storage_backends.input.file == "gcs"
    error_message = "The record keeps the backend each storage role started on."
  }
}

run "a_state_with_no_location_record_is_not_refused" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed", "high_availability" : true }, "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-other", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-other", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  assert {
    condition     = terraform_data.storage_locations.input.build == "acme-other/builds" && terraform_data.storage_locations.input.file == "acme-other/files"
    error_message = "With nothing recorded to compare with, the first plan goes ahead and records where storage is then."
  }
  assert {
    condition     = terraform_data.storage_backends.input.build == "gcs" && terraform_data.storage_backends.input.file == "gcs"
    error_message = "Adding the location record leaves the backends as they were recorded."
  }
}

run "the_next_apply_records_each_storage_location" {
  command = apply
  plan_options {
    target = [terraform_data.storage_locations]
  }
  assert {
    condition     = terraform_data.storage_locations.input.build == "acme-hot/builds" && terraform_data.storage_locations.input.file == "acme-hot/files"
    error_message = "The record keeps the bucket and prefix of each storage role."
  }
}

run "refuses_another_storage_backend" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed", "high_availability" : true }, "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "s3", "bucket" : "acme-files", "region" : "us-east-1", "prefix" : "files" }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  expect_failures = [terraform_data.storage_backends]
}

run "refuses_both_roles_moving_together" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed", "high_availability" : true }, "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  expect_failures = [terraform_data.storage_backends]
}

run "the_record_outlasts_a_refused_change" {
  command = plan
  assert {
    condition     = terraform_data.storage_backends.input.build == "gcs" && terraform_data.storage_backends.input.file == "gcs"
    error_message = "With the backends put back the plan goes ahead, and the record still has what the installation started on."
  }
  assert {
    condition     = terraform_data.storage_locations.input.build == "acme-hot/builds" && terraform_data.storage_locations.input.file == "acme-hot/files"
    error_message = "With the locations put back the plan goes ahead, and the record still has where the installation started."
  }
}

# Records name their bucket, and the hosts keep access only to the new location.
run "refuses_another_bucket_of_the_same_backend" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed", "high_availability" : true }, "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-other", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-other", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  expect_failures = [terraform_data.storage_locations]
}

run "refuses_another_prefix_of_the_same_bucket" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed", "high_availability" : true }, "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "uploads", "provision" : true }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  expect_failures = [terraform_data.storage_locations]
}

run "the_same_location_is_not_a_move" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed", "high_availability" : true }, "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds/" },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files/" }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  assert {
    condition     = terraform_data.storage_locations.input.file == "acme-hot/files" && local.config.storage.file.prefix == "files/" && !local.config.storage.file.provision
    error_message = "A trailing slash on a prefix, or a change of whether the stack provisions the bucket, leaves storage where it is."
  }
}

run "an_installation_that_stored_nothing_can_start_again" {
  command = plan
  plan_options {
    replace = [terraform_data.storage_backends]
  }
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed", "high_availability" : true }, "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" },
        "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  assert {
    condition     = terraform_data.storage_backends.input.build == "s3" && terraform_data.storage_backends.input.file == "s3"
    error_message = "Replacing the record is the deliberate way past the refusal."
  }
}

run "the_same_replace_is_the_way_past_another_bucket" {
  command = plan
  plan_options {
    replace = [terraform_data.storage_backends]
  }
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "api_exposure" : "public",
      "network" : { "client_cidrs" : ["10.10.0.0/16"] },
      "data" : { "postgres" : { "mode" : "managed", "high_availability" : true }, "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-other", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-other", "prefix" : "files", "provision" : true }
      },
      "scaling" : { "worker" : { "min" : 2, "max" : 4 }, "task_worker" : { "min" : 1, "max" : 3 } }
    }
  }
  assert {
    condition     = terraform_data.storage_locations.input.build == "acme-other/builds" && terraform_data.storage_locations.input.file == "acme-other/files"
    error_message = "Replacing the backend record replaces the location record too, so one flag is the deliberate way past both refusals."
  }
}
