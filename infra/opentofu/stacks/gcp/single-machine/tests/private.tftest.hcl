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
  installation = {
    "schema_version" : 1,
    "name" : "hot-example",
    "cloud" : "gcp",
    "region" : "us-central1",
    "zone" : "us-central1-a",
    "size" : "small",
    "host_image" : "projects/example-project/global/images/hot-host-amd64",
    "project" : "example-project",
    "release_pointer" : "projects/example-project/locations/global/parameters/hot-example-release",
    "secret_references" : {
      "app" : "projects/example-project/secrets/hot-example-app"
    },
    "app_hostname" : "app.hot.internal",
    "api_hostname" : "api.hot.internal",
    "storage" : {
      "build" : {
        "backend" : "local"
      },
      "file" : {
        "backend" : "local"
      }
    }
  }
}

run "private_local" {
  command = plan
  assert {
    condition     = length(google_compute_instance.host.network_interface[0].access_config) == 0
    error_message = "The VM must have no external address."
  }
  assert {
    condition     = google_compute_instance.host.advanced_machine_features[0].enable_nested_virtualization && google_compute_instance.host.deletion_protection
    error_message = "Nested virtualization and deletion protection must be enabled."
  }
  assert {
    condition     = google_compute_instance.host.metadata["block-project-ssh-keys"] == "TRUE" && google_compute_instance.host.metadata["serial-port-enable"] == "FALSE" && jsondecode(base64decode(google_compute_instance.host.metadata["hot-installation"])).name == "hot-example" && google_compute_instance.host.allow_stopping_for_update
    error_message = "Project SSH keys and the serial console must be disabled."
  }
  assert {
    condition     = tolist(google_compute_firewall.https.allow)[0].ports == tolist(["443"])
    error_message = "Ingress must be HTTPS only, never SSH or data ports."
  }
}

run "private_existing_s3" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "small",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "release_pointer" : "projects/example-project/locations/global/parameters/hot-example-release",
      "secret_references" : {
        "app" : "projects/example-project/secrets/hot-example-app"
      },
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "storage" : {
        "build" : {
          "backend" : "s3",
          "bucket" : "customer-hot-storage",
          "region" : "us-east-1",
          "prefix" : "hot-example/builds"
        },
        "file" : {
          "backend" : "s3",
          "bucket" : "customer-hot-storage",
          "region" : "us-east-1",
          "prefix" : "hot-example/files"
        }
      }
    }
  }
  assert {
    condition     = output.configuration.storage.file.backend == "s3" && output.configuration.storage.file.bucket == "customer-hot-storage"
    error_message = "S3 selection must remain independent of compute cloud."
  }
}

run "requires_development_acknowledgment" {
  command = plan
  variables { acknowledge_unqualified = false }
  expect_failures = [var.acknowledge_unqualified]
}

run "existing_network_ownership" {
  command = plan
  variables { installation = {
    "schema_version" : 1,
    "name" : "hot-example",
    "cloud" : "gcp",
    "region" : "us-central1",
    "zone" : "us-central1-a",
    "size" : "small",
    "host_image" : "projects/example-project/global/images/hot-host-amd64",
    "project" : "example-project",
    "release_pointer" : "projects/example-project/locations/global/parameters/hot-example-release",
    "secret_references" : {
      "app" : "projects/example-project/secrets/hot-example-app"
    },
    "app_hostname" : "app.hot.internal",
    "api_hostname" : "api.hot.internal",
    "storage" : {
      "build" : {
        "backend" : "local"
      },
      "file" : {
        "backend" : "local"
      }
    },
    "network" : {
      "mode" : "existing",
      "network_id" : "projects/example-project/global/networks/customer-private",
      "subnet_id" : "projects/example-project/regions/us-central1/subnetworks/customer-private"
    }
  } }
  assert {
    condition     = length(google_compute_network.hot) == 0 && length(google_compute_router_nat.egress) == 0 && length(google_compute_subnetwork.hot) == 0
    error_message = "Existing networking must not be recreated or re-routed."
  }
}

run "agent_channel" {
  command = plan
  assert {
    condition     = google_pubsub_subscription_iam_member.agent.role == "roles/pubsub.subscriber" && google_pubsub_topic.commands.name == "hot-hot-example-commands"
    error_message = "Host commands arrive on the installation's own Pub/Sub subscription."
  }
  assert {
    condition     = google_secret_manager_secret_iam_member.store_generated.role == "roles/secretmanager.secretVersionAdder"
    error_message = "The host may add versions only to its generated secrets."
  }
  assert {
    condition     = length(google_secret_manager_secret_iam_member.read) == 3
    error_message = "The host reads the release pointer, generated secrets, and operator secrets."
  }
}

run "release_principal_is_scoped" {
  command = plan
  variables {
    release_principal = "hot-github-release@example-project.iam.gserviceaccount.com"
  }
  assert {
    condition     = google_secret_manager_secret_iam_member.release[0].role == "roles/secretmanager.secretVersionAdder" && google_pubsub_topic_iam_member.release[0].role == "roles/pubsub.publisher"
    error_message = "The release account may add pointer versions and publish commands only."
  }
}

run "existing_gcs_is_prefix_scoped" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "project" : "example-project",
      "host_image" : "projects/example-project/global/images/hot-host",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds" },
        "file" : { "backend" : "local" }
      }
    }
  }
  assert {
    condition     = length(google_storage_bucket_iam_member.storage) == 1 && strcontains(google_storage_bucket_iam_member.storage["build"].condition[0].expression, "objects/builds/")
    error_message = "GCS access is limited to the installation's prefix."
  }
}

run "public_app_only" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "project" : "example-project",
      "host_image" : "projects/example-project/global/images/hot-host",
      "app_hostname" : "app.hot.example",
      "api_hostname" : "api.hot.example",
      "app_exposure" : "public",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = toset(google_compute_managed_ssl_certificate.public[0].managed[0].domains) == toset(["app.hot.example"])
    error_message = "Only the public endpoint gets a public certificate."
  }
  assert {
    condition     = one(google_compute_firewall.public[0].allow).ports == tolist(["9443"]) && length(google_compute_instance.host.network_interface[0].access_config) == 0
    error_message = "The load balancer reaches only the host's public-endpoint port; the VM has no external IP."
  }
}

run "managed_postgres" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed", "high_availability" : true } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = google_sql_database_instance.postgres[0].root_password == null && tostring(google_sql_database_instance.postgres[0].root_password_wo_version) == "1" && google_secret_manager_secret_version.postgres_admin[0].secret_data == null
    error_message = "The administrator password reaches Cloud SQL and Secret Manager only through write-only attributes."
  }
  assert {
    condition     = one(google_sql_database_instance.postgres[0].settings[0].ip_configuration).ipv4_enabled == false && one(google_sql_database_instance.postgres[0].settings[0].ip_configuration).ssl_mode == "ENCRYPTED_ONLY"
    error_message = "Cloud SQL has no public address and refuses connections without TLS."
  }
  assert {
    condition     = google_sql_database_instance.postgres[0].settings[0].availability_type == "REGIONAL" && google_sql_database_instance.postgres[0].settings[0].tier == "db-custom-2-8192" && google_sql_database_instance.postgres[0].deletion_protection
    error_message = "High availability is regional; the tier follows the size; the instance is protected from deletion."
  }
  assert {
    condition     = one(google_sql_database_instance.postgres[0].settings[0].backup_configuration).point_in_time_recovery_enabled && one(one(google_sql_database_instance.postgres[0].settings[0].backup_configuration).backup_retention_settings).retained_backups == 7
    error_message = "Backups with point-in-time recovery are kept for the configured days."
  }
  assert {
    condition     = length(google_service_networking_connection.services) == 1 && google_compute_global_address.services[0].purpose == "VPC_PEERING"
    error_message = "A new network gets private services access for Cloud SQL."
  }
  assert {
    condition     = output.configuration.data.postgres.endpoint.admin_user == "postgres" && output.configuration.data.postgres.endpoint.port == 5432
    error_message = "The agent learns where the database and its administrator secret are."
  }
}

run "backups_and_alerts" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "monitoring" : { "alert_emails" : ["ops@example.com"] },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = one(one(google_compute_resource_policy.snapshots.snapshot_schedule_policy).retention_policy).max_retention_days == 7 && one(one(google_compute_resource_policy.snapshots.snapshot_schedule_policy).snapshot_properties).labels["hot-installation"] == "hot-example"
    error_message = "Snapshots are kept for the default week and labeled for the agent."
  }
  assert {
    condition     = google_compute_disk_resource_policy_attachment.snapshots.disk == google_compute_disk.data.name
    error_message = "The schedule snapshots the retained data disk."
  }
  assert {
    condition     = google_project_iam_custom_role.snapshots.permissions == toset(["compute.snapshots.list"])
    error_message = "The host may only list snapshots."
  }
  assert {
    condition     = google_monitoring_metric_descriptor.hot["agent_heartbeat"].type == "custom.googleapis.com/hot/hot-example/agent_heartbeat" && length(google_monitoring_alert_policy.hot) == 14
    error_message = "Each installation owns its metric types, and every critical signal has an alert."
  }
  assert {
    condition     = startswith(one(google_monitoring_alert_policy.hot["job-wait"].conditions).condition_threshold[0].filter, "resource.type = \"global\"") && startswith(one(google_monitoring_alert_policy.hot["disk-pressure"].conditions).condition_threshold[0].filter, "resource.type = \"gce_instance\"")
    error_message = "Queue alerts read the installation-wide series; host alerts the instance's."
  }
  assert {
    condition     = length(jsondecode(google_monitoring_dashboard.hot.dashboard_json).mosaicLayout.tiles) == 11 && strcontains(jsondecode(google_monitoring_dashboard.hot.dashboard_json).mosaicLayout.tiles[3].widget.xyChart.dataSets[0].timeSeriesQuery.timeSeriesFilter.filter, "resource.type = \"global\"")
    error_message = "A dashboard of the installation's metrics, reading queue metrics for the project."
  }
  assert {
    condition     = length(google_monitoring_alert_policy.hot["backup-stale"].conditions) == 2 && length(google_monitoring_notification_channel.email) == 1
    error_message = "A missing backup status alerts too, by email."
  }
}

run "restore_from_a_snapshot" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "backup" : { "restore_snapshot" : "hot-example-data-20261029" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = google_compute_disk.data.snapshot == "hot-example-data-20261029" && google_compute_disk.data.name != "hot-hot-example-data" && startswith(google_compute_disk.data.name, "hot-hot-example-data-")
    error_message = "A restore creates the data disk from the chosen snapshot, under a name that does not collide with the kept disk."
  }
}

run "managed_valkey" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "valkey" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = google_memorystore_instance.valkey[0].mode == "CLUSTER_DISABLED" && google_memorystore_instance.valkey[0].shard_count == 1 && google_memorystore_instance.valkey[0].node_type == "STANDARD_SMALL"
    error_message = "One standalone shard, sized from the installation, so Hot's standalone client applies."
  }
  assert {
    condition     = google_memorystore_instance.valkey[0].authorization_mode == "IAM_AUTH" && google_memorystore_instance.valkey[0].transit_encryption_mode == "SERVER_AUTHENTICATION"
    error_message = "Clients authenticate with IAM tokens over TLS; there is no password."
  }
  assert {
    condition     = google_memorystore_instance.valkey[0].engine_configs["maxmemory-policy"] == "noeviction" && one(google_memorystore_instance.valkey[0].persistence_config).mode == "AOF" && google_memorystore_instance.valkey[0].deletion_protection_enabled
    error_message = "Queued work is never evicted, is persisted, and the instance is protected from deletion."
  }
  assert {
    condition     = google_network_connectivity_service_connection_policy.valkey[0].service_class == "gcp-memorystore"
    error_message = "The new network gets the Private Service Connect policy Memorystore needs."
  }
  assert {
    condition     = google_project_iam_member.valkey[0].role == "roles/memorystore.dbConnectionUser" && strcontains(google_project_iam_member.valkey[0].condition[0].expression, "/instances/hot-hot-example-valkey")
    error_message = "The host may connect to this instance only."
  }
  assert {
    condition     = local.host_config.data.valkey.endpoint.host == "10.68.0.40" && local.host_config.data.valkey.endpoint.auth == "gcp-iam" && startswith(local.host_config.data.valkey.endpoint.ca_certificate, "-----BEGIN CERTIFICATE-----")
    error_message = "The host connects to the primary endpoint with IAM tokens and the instance's CA."
  }
}

run "provisioned_buckets" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot-builds", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "local" }
      }
    }
  }
  assert {
    condition     = google_storage_bucket.storage["acme-hot-builds"].public_access_prevention == "enforced" && google_storage_bucket.storage["acme-hot-builds"].uniform_bucket_level_access && !google_storage_bucket.storage["acme-hot-builds"].force_destroy
    error_message = "Provisioned buckets are private, IAM-only, and never emptied on destroy."
  }
  assert {
    condition     = google_storage_bucket.storage["acme-hot-builds"].versioning[0].enabled && length(google_storage_bucket.storage["acme-hot-builds"].lifecycle_rule) == 2
    error_message = "Versions are kept for a while and abandoned uploads are cleaned up."
  }
  assert {
    condition     = google_storage_bucket_iam_member.storage["build"].bucket == "acme-hot-builds" && strcontains(google_storage_bucket_iam_member.storage["build"].condition[0].expression, "/objects/builds/")
    error_message = "The host reaches the role's prefix only."
  }
}

run "mirrors_queues_and_budgets" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "image_registry" : "us-docker.pkg.dev/acme-mirror/hot",
      "monitoring" : { "alert_emails" : ["ops@example.com"], "queue_backlog_alert" : 250, "monthly_budget" : 400, "billing_account" : "0A1B2C-3D4E5F-6A7B8C" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = google_artifact_registry_repository_iam_member.mirror[0].project == "acme-mirror" && google_artifact_registry_repository_iam_member.mirror[0].location == "us" && google_artifact_registry_repository_iam_member.mirror[0].repository == "hot" && google_artifact_registry_repository_iam_member.mirror[0].role == "roles/artifactregistry.reader"
    error_message = "The host account may read the mirror repository and nothing more."
  }
  assert {
    condition     = google_monitoring_alert_policy.hot["queue-backlog"].conditions[0].condition_threshold[0].threshold_value == 250 && contains(keys(google_monitoring_metric_descriptor.hot), "new_dead_letters")
    error_message = "Queue alerts use the chosen backlog threshold and alert on new dead letters."
  }
  assert {
    condition     = google_billing_budget.monthly[0].billing_account == "0A1B2C-3D4E5F-6A7B8C" && google_billing_budget.monthly[0].amount[0].specified_amount[0].units == "400" && contains(keys(google_project_service.required), "billingbudgets.googleapis.com")
    error_message = "The budget covers the installation's project on its billing account."
  }
}

run "no_mirror_or_budget_by_default" {
  command = plan
  assert {
    condition     = length(google_billing_budget.monthly) == 0 && length(google_artifact_registry_repository_iam_member.mirror) == 0 && !contains(keys(google_project_service.required), "billingbudgets.googleapis.com")
    error_message = "Without a mirror or budget, there is no registry access and no budget."
  }
}
