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
    condition     = google_secret_manager_secret_iam_member.postgres_admin[0].member == "serviceAccount:${google_service_account.role["scheduler"].email}"
    error_message = "Only the migration host reads the database administrator's secret."
  }
  assert {
    condition     = length(google_project_iam_member.valkey) == 5 && length(google_storage_bucket_iam_member.storage) == 10
    error_message = "Every role reaches Valkey and both storage prefixes."
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
    condition     = google_monitoring_alert_policy.scheduler.display_name == "hot-hot-example-scheduler-stopped" && length(google_monitoring_alert_policy.hot) == 7
    error_message = "The singleton scheduler and every per-host signal alert."
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
