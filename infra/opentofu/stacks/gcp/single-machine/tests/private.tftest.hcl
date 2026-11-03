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
    condition     = length(output.configuration.load_balancer_addresses) == 0
    error_message = "Without a public endpoint there is no load balancer address to trust."
  }
  assert {
    condition     = output.configuration.data.postgres.endpoint == null && output.configuration.data.valkey.endpoint == null
    error_message = "A local database and cache have no endpoint: they are on the bundle's own network, which user code is always denied."
  }
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
  assert {
    condition     = google_compute_router_nat.egress[0].enable_dynamic_port_allocation && !google_compute_router_nat.egress[0].enable_endpoint_independent_mapping && google_compute_router_nat.egress[0].min_ports_per_vm == 4096 && google_compute_router_nat.egress[0].max_ports_per_vm == 65536
    error_message = "The one host gets enough NAT ports for busy outbound calls to one destination, not the default 64."
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

run "refuses_a_scalable_installation" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "project" : "example-project",
      "host_image" : "projects/example-project/global/images/hot-host",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : {
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds" },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files" }
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
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgresql" : { "endpoint" : { "deny_cidrs" : ["10.20.0.0/20"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_deny_list_outside_postgres_endpoint" {
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
      "data" : { "postgres" : { "mode" : "existing", "deny_cidrs" : ["10.20.0.0/20"] } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_deny_list_outside_valkey_endpoint" {
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
      "data" : { "valkey" : { "mode" : "existing", "deny_cidrs" : ["10.30.0.0/24"] } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_misspelled_postgres_endpoint_key" {
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
      "data" : { "postgres" : { "mode" : "existing", "endpoint" : { "deny_cidr" : ["10.20.0.0/20"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "refuses_a_misspelled_valkey_endpoint_key" {
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
      "data" : { "valkey" : { "mode" : "existing", "endpoint" : { "deny_cidr" : ["10.30.0.0/24"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "accepts_every_data_key_the_contract_knows" {
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
      "data" : {
        "postgres" : { "mode" : "managed", "runtime_role" : "hot_runtime", "version" : "18", "tier" : "", "storage_gb" : 0, "high_availability" : false, "backup_retention_days" : 7, "deletion_protection" : true, "admin_password_version" : 1, "endpoint" : null },
        "valkey" : { "mode" : "managed", "node_type" : "", "high_availability" : false, "auth_token_version" : 1, "endpoint" : { "deny_cidrs" : ["10.30.0.0/24"] } }
      },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.data.postgres.mode == "managed" && contains(output.configuration.data.valkey.endpoint.deny_cidrs, "10.30.0.0/24")
    error_message = "Every key in the contract's data object, and a null endpoint, must pass the unknown-key check."
  }
}

run "accepts_null_data" {
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
      "data" : null,
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.data.postgres.endpoint == null && output.configuration.data.valkey.endpoint == null
    error_message = "Null data follows the contract's defaults; keys(null) must not fail the unknown-key check."
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
  assert {
    condition     = keys(google_project_iam_member.release) == ["roles/logging.viewer", "roles/monitoring.alertPolicyViewer", "roles/monitoring.snoozeEditor"] && alltrue([for m in google_project_iam_member.release : m.condition[0].title == "hot-hot-example-release" && m.member == "serviceAccount:hot-github-release@example-project.iam.gserviceaccount.com"])
    error_message = "The release account's project roles are this installation's own bindings, so another installation sharing the account cannot remove them."
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
    condition     = length(google_storage_bucket_iam_member.existing) == 1 && strcontains(google_storage_bucket_iam_member.existing["build"].condition[0].expression, "objects/builds/")
    error_message = "GCS access is limited to the installation's prefix."
  }
  assert {
    condition     = length(google_storage_bucket_iam_policy.storage) == 0 && length(data.google_iam_policy.storage) == 0
    error_message = "An existing bucket's policy stays the operator's: the stack only adds its binding."
  }
}

run "public_app_only" {
  command = plan
  override_resource {
    target = google_compute_global_address.public
    values = { address = "34.120.5.9" }
  }
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
    condition     = keys(google_compute_managed_ssl_certificate.public) == ["app"] && google_compute_managed_ssl_certificate.public["app"].managed[0].domains == tolist(["app.hot.example"])
    error_message = "Only the public endpoint gets a public certificate."
  }
  assert {
    condition     = google_compute_managed_ssl_certificate.public["app"].name == "hot-hot-example-app-${substr(sha1("app.hot.example"), 0, 8)}"
    error_message = "A certificate is named after its hostname, so a new hostname's certificate can exist beside the old one."
  }
  assert {
    condition     = one(google_compute_firewall.public[0].allow).ports == tolist(["9443"]) && length(google_compute_instance.host.network_interface[0].access_config) == 0
    error_message = "The load balancer reaches only the host's public-endpoint port; the VM has no external IP."
  }
  assert {
    condition     = google_compute_instance_group_membership.public[0].instance == google_compute_instance.host.self_link && google_compute_instance_group_membership.public[0].instance_group == "hot-hot-example-public"
    error_message = "The host joins the load balancer's group through a membership, which a replaced VM replaces."
  }
  assert {
    condition     = google_compute_instance_group_membership.public[0].deletion_policy == "ABANDON"
    error_message = "Removing the membership must not take the VM out of the group: a VM that then refuses its delete, under deletion protection, keeps serving. The group drops a VM that does go."
  }
  assert {
    condition     = google_compute_instance_group.public[0].network == "projects/example-project/global/networks/hot-example"
    error_message = "The group names its network, so a backend service accepts it while it is empty: before the host joins, and while a replaced host is gone."
  }
  assert {
    condition     = tolist(output.configuration.load_balancer_addresses) == tolist(["34.120.5.9"]) && output.public_endpoint == "34.120.5.9"
    error_message = "The ingress must trust the load balancer's own address, which it appends to X-Forwarded-For, to reach the client's."
  }
}

run "public_group_expands_a_bare_network_name" {
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
      "network" : { "mode" : "existing", "network_id" : "customer-private", "subnet_id" : "customer-private" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = google_compute_instance_group.public[0].network == "projects/example-project/global/networks/customer-private"
    error_message = "The provider sends a group's network to the API as given, so a bare name must be spelled out."
  }
}

run "public_group_expands_a_project_relative_network" {
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
      "network" : { "mode" : "existing", "network_id" : "global/networks/customer-private", "subnet_id" : "customer-private" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = google_compute_instance_group.public[0].network == "projects/example-project/global/networks/customer-private"
    error_message = "A path with no project would not match an existing group's self link, and would replace the group."
  }
}

run "public_group_keeps_a_shared_vpc_network" {
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
      "network" : {
        "mode" : "existing",
        "network_id" : "https://www.googleapis.com/compute/v1/projects/network-host/global/networks/shared",
        "subnet_id" : "projects/network-host/regions/us-central1/subnetworks/shared"
      },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = google_compute_instance_group.public[0].network == "https://www.googleapis.com/compute/v1/projects/network-host/global/networks/shared"
    error_message = "A network that names its project stays as given, or a Shared VPC host's network would become one in the service project."
  }
}

run "public_app_and_api" {
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
      "api_exposure" : "public",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = keys(google_compute_managed_ssl_certificate.public) == ["api", "app"] && google_compute_managed_ssl_certificate.public["api"].managed[0].domains == tolist(["api.hot.example"]) && google_compute_managed_ssl_certificate.public["app"].name == "hot-hot-example-app-${substr(sha1("app.hot.example"), 0, 8)}"
    error_message = "Each public hostname has its own certificate, so making the API public leaves the app's certificate as it was."
  }
  assert {
    condition     = length(google_compute_target_https_proxy.public[0].ssl_certificates) == 2 && toset(one(google_compute_url_map.public[0].host_rule).hosts) == toset(["app.hot.example", "api.hot.example"])
    error_message = "The proxy serves both certificates and both hostnames."
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
  assert {
    condition     = output.configuration.data.postgres.endpoint.deny_cidrs == tolist(["10.100.0.0/20"])
    error_message = "User code is denied the whole private services range the stack reserves, which the network's own range does not cover."
  }
  assert {
    condition     = one(google_sql_database_instance.postgres[0].settings[0].database_flags).name == "max_connections" && one(google_sql_database_instance.postgres[0].settings[0].database_flags).value == "400" && google_sql_database_instance.postgres[0].settings[0].disk_autoresize
    error_message = "Cloud SQL accepts the size's planned connections, and grows its disk."
  }
}

run "managed_postgres_medium" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "medium",
      "host_image" : "projects/example-project/global/images/hot-host-amd64",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = google_sql_database_instance.postgres[0].settings[0].tier == "db-custom-4-16384" && one(google_sql_database_instance.postgres[0].settings[0].database_flags).value == "800"
    error_message = "A 16 GB tier defaults to 500 connections; the size plans 800, so the flag raises it."
  }
  assert {
    condition     = one(google_monitoring_alert_policy.database_connections[0].conditions).condition_threshold[0].threshold_value == 640
    error_message = "The connections alert is 80% of a limit the database actually has."
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
  override_resource {
    target = random_id.roles
    values = { hex = "a1b2c3" }
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
    condition     = google_project_iam_custom_role.snapshots.role_id == "hot_hot_example_snapshots_a1b2c3"
    error_message = "The snapshot role's ID carries the installation's own suffix, so a reinstall under the same name never needs a deleted role's ID."
  }
  assert {
    condition     = one(google_monitoring_alert_policy.host_down.conditions).condition_absent[0].aggregations[0].cross_series_reducer == "REDUCE_COUNT" && one([for c in google_monitoring_alert_policy.hot["backup-stale"].conditions : c if length(c.condition_absent) > 0]).condition_absent[0].aggregations[0].cross_series_reducer == "REDUCE_COUNT"
    error_message = "Absence alerts count every instance's series, so a replaced VM's old series does not read as missing."
  }
  assert {
    condition     = one(google_monitoring_alert_policy.hot["disk-pressure"].conditions).condition_threshold[0].aggregations[0].cross_series_reducer == "REDUCE_MAX" && one(google_monitoring_alert_policy.hot["certificate-expiring"].conditions).condition_threshold[0].aggregations[0].cross_series_reducer == "REDUCE_MIN"
    error_message = "Threshold alerts take the worst value across instances, so an incident follows the host across a VM replacement and can resolve."
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
    condition     = alltrue([for tile in jsondecode(google_monitoring_dashboard.hot.dashboard_json).mosaicLayout.tiles : try(tile.xPos, 1) != 0 && try(tile.yPos, 1) != 0])
    error_message = "The API omits zero values from a dashboard it returns, so a tile that sends a zero position shows as a change in every plan."
  }
  assert {
    condition     = [for tile in jsondecode(google_monitoring_dashboard.hot.dashboard_json).mosaicLayout.tiles : [try(tile.xPos, 0), try(tile.yPos, 0)]] == [for index in range(11) : [(index % 2) * 6, floor(index / 2) * 4]]
    error_message = "Leaving out zero positions keeps the tiles on the same two-column grid."
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
    condition     = google_memorystore_instance.valkey[0].replica_count == 0 && one(google_memorystore_instance.valkey[0].zone_distribution_config).mode == "MULTI_ZONE"
    error_message = "Without high availability there is no replica, but the instance is still placed across zones, which cannot change later."
  }
  assert {
    condition     = local.host_config.data.valkey.endpoint.host == "10.68.0.40" && local.host_config.data.valkey.endpoint.auth == "gcp-iam" && startswith(local.host_config.data.valkey.endpoint.ca_certificate, "-----BEGIN CERTIFICATE-----")
    error_message = "The host connects to the primary endpoint with IAM tokens and the instance's CA."
  }
  assert {
    condition     = local.host_config.data.valkey.endpoint.deny_cidrs == tolist(["10.68.0.39/32", "10.68.0.40/32"])
    error_message = "User code is denied every address Private Service Connect gives the instance, not only the primary's."
  }
}

run "managed_valkey_high_availability" {
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
      "data" : { "valkey" : { "mode" : "managed", "high_availability" : true } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = google_memorystore_instance.valkey[0].replica_count == 1 && one(google_memorystore_instance.valkey[0].zone_distribution_config).mode == "MULTI_ZONE" && one(google_memorystore_instance.valkey[0].zone_distribution_config).zone == null
    error_message = "High availability only adds a replica, in another zone; the zone distribution is the same either way, so turning it on later changes the instance in place."
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
    condition     = google_storage_bucket.storage["acme-hot-builds"].versioning[0].enabled && length(google_storage_bucket.storage["acme-hot-builds"].lifecycle_rule) == 3 && contains(flatten([for rule in google_storage_bucket.storage["acme-hot-builds"].lifecycle_rule : [for c in rule.condition : coalesce(c.matches_prefix, [])]]), "builds/.uploads/")
    error_message = "Versions are kept for a while, and abandoned uploads, including Hot's staged parts, are cleaned up."
  }
  assert {
    condition     = length(google_storage_bucket_iam_member.existing) == 0 && google_storage_bucket_iam_policy.storage["acme-hot-builds"].bucket == "acme-hot-builds" && strcontains(file("${path.module}/storage.tf"), "policy_data = data.google_iam_policy.storage[each.key].policy_data")
    error_message = "A provisioned bucket's whole policy is the stack's, with no additive binding that it would remove."
  }
  assert {
    condition     = !strcontains(jsonencode(data.google_iam_policy.storage["acme-hot-builds"].binding), "projectViewer") && !strcontains(jsonencode(data.google_iam_policy.storage["acme-hot-builds"].binding), "legacyObjectReader") && !strcontains(jsonencode(data.google_iam_policy.storage["acme-hot-builds"].binding), "legacyBucketReader")
    error_message = "Project viewers, which a pull request's plan is, must not read a provisioned bucket's objects or list their names."
  }
  assert {
    condition = alltrue([for role in ["roles/storage.legacyBucketOwner", "roles/storage.legacyObjectOwner"] : one([
      for b in data.google_iam_policy.storage["acme-hot-builds"].binding : b.members if b.role == role
    ]) == toset(["projectOwner:example-project", "projectEditor:example-project"])])
    error_message = "Project owners and editors keep the bucket, since an apply identity may manage it through them."
  }
  assert {
    condition = one([
      for b in data.google_iam_policy.storage["acme-hot-builds"].binding : b.members if b.role == "roles/storage.objectUser"
    ]) == toset(["serviceAccount:${google_service_account.host.email}"])
    error_message = "The host is the only other principal on the bucket."
  }
  assert {
    condition = strcontains(one(flatten([
      for b in data.google_iam_policy.storage["acme-hot-builds"].binding : b.condition[*].expression if b.role == "roles/storage.objectUser"
    ])), "/objects/builds/")
    error_message = "The host reaches the role's prefix only."
  }
  assert {
    condition     = strcontains(regex("(?s)resource \"google_storage_bucket_iam_policy\" \"storage\" \\{.*?\\n\\}", file("${path.module}/storage.tf")), "prevent_destroy = true") && strcontains(regex("(?s)resource \"google_storage_bucket_iam_policy\" \"storage\" \\{.*?\\n\\}", file("${path.module}/storage.tf")), "depends_on = [google_storage_bucket_iam_member.existing]")
    error_message = "Destroying the policy would empty the bucket's, even of its owners, so it is kept like the bucket; and an existing bucket's binding goes before the policy is set, not after."
  }
  assert {
    condition     = strcontains(file("${path.module}/main.tf"), "removed {\n  from = google_storage_bucket_iam_member.storage\n  lifecycle { destroy = false }\n}")
    error_message = "Earlier versions' bindings are forgotten, not destroyed: destroying one after the policy is set would take the host's access out of it."
  }
}

run "provisioned_and_existing_buckets_side_by_side" {
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
        "file" : { "backend" : "gcs", "bucket" : "acme-shared", "prefix" : "hot/files" }
      }
    }
  }
  assert {
    condition     = keys(google_storage_bucket_iam_policy.storage) == ["acme-hot-builds"] && keys(google_storage_bucket_iam_member.existing) == ["file"]
    error_message = "Only the provisioned bucket's whole policy is the stack's; the existing bucket gets a binding."
  }
  assert {
    condition     = !strcontains(jsonencode(data.google_iam_policy.storage["acme-hot-builds"].binding), "acme-shared") && strcontains(google_storage_bucket_iam_member.existing["file"].condition[0].expression, "buckets/acme-shared/objects/hot/files/")
    error_message = "Each bucket's grant names its own bucket and prefix."
  }
}

run "shared_provisioned_bucket_grants_each_prefix" {
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
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds", "provision" : true },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files", "provision" : true }
      }
    }
  }
  assert {
    condition = (keys(google_storage_bucket_iam_policy.storage) == ["acme-hot"]
      && toset(flatten([for b in data.google_iam_policy.storage["acme-hot"].binding : [for c in b.condition : c.title]])) == toset(["hot-hot-example-build", "hot-hot-example-file"])
      && length([for b in data.google_iam_policy.storage["acme-hot"].binding : b if b.role == "roles/storage.objectUser"]) == 2
    && strcontains(jsonencode(data.google_iam_policy.storage["acme-hot"].binding), "/objects/builds/") && strcontains(jsonencode(data.google_iam_policy.storage["acme-hot"].binding), "/objects/files/"))
    error_message = "One bucket holding both prefixes gets one policy, with a conditional binding per prefix."
  }
}

run "existing_network_and_existing_services_list_their_ranges" {
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
      "network" : {
        "mode" : "existing",
        "network_id" : "projects/example-project/global/networks/customer-private",
        "subnet_id" : "projects/example-project/regions/us-central1/subnetworks/customer-private"
      },
      "data" : {
        "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.20.0.0/20"] } },
        "valkey" : { "mode" : "existing", "endpoint" : { "deny_cidrs" : ["10.30.0.0/24"] } }
      },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = length(google_compute_global_address.services) == 0 && output.configuration.data.postgres.endpoint.deny_cidrs == tolist(["10.20.0.0/20"]) && output.configuration.data.postgres.endpoint.port == 5432
    error_message = "On an existing network the stack reserves no range, so the operator's entry names it, beside the endpoint."
  }
  assert {
    condition     = output.configuration.data.valkey.endpoint.deny_cidrs == tolist(["10.30.0.0/24"]) && !can(output.configuration.data.valkey.endpoint.host)
    error_message = "An existing service's entries reach the host; its connection settings stay in the operator secret."
  }
}

run "existing_services_without_entries_have_no_endpoint" {
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
      "data" : { "postgres" : { "mode" : "existing" }, "valkey" : { "mode" : "existing" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.data.postgres.endpoint == null && output.configuration.data.valkey.endpoint == null
    error_message = "With nothing to deny, the document stays as it was, so no host changes for it."
  }
}

run "a_managed_database_on_an_existing_network_needs_no_entries" {
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
      "network" : {
        "mode" : "existing",
        "network_id" : "projects/example-project/global/networks/customer-private",
        "subnet_id" : "projects/example-project/regions/us-central1/subnetworks/customer-private"
      },
      "data" : { "postgres" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = length(output.configuration.data.postgres.endpoint.deny_cidrs) == 0 && output.configuration.data.postgres.endpoint.host == "10.100.0.3"
    error_message = "The range is optional: the host still gets its endpoint, whose address it resolves and denies."
  }
}

run "operator_entries_join_what_the_stack_knows" {
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
      "data" : {
        "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.100.0.0/20", "10.20.0.0/20"] } },
        "valkey" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.30.0.0/24"] } }
      },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.data.postgres.endpoint.deny_cidrs == tolist(["10.100.0.0/20", "10.20.0.0/20"]) && output.configuration.data.valkey.endpoint.deny_cidrs == tolist(["10.30.0.0/24", "10.68.0.39/32", "10.68.0.40/32"])
    error_message = "The operator's entries stay, with the stack's added once each."
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

# The order of an apply, and what an upgrade does to a VM that exists, do not
# show in a plan from empty state, and prevent_destroy stops a test from
# applying and cleaning up. These read the configuration instead.
run "api_order_and_address_upgrade" {
  command = plan
  assert {
    condition = alltrue([for resource in [
      "google_compute_address\" \"egress",
      "google_compute_global_address\" \"public",
      "google_compute_managed_ssl_certificate\" \"public",
      "google_compute_health_check\" \"public",
      "google_compute_ssl_policy\" \"public",
    ] : strcontains(regex("(?s)resource \"${resource}\" \\{.*?\\n\\}", "${file("${path.module}/main.tf")}\n${file("${path.module}/public.tf")}"), "= [google_project_service.required]")])
    error_message = "A Compute resource that refers to nothing else must wait for the API, or the first apply on a project with it off fails."
  }
  assert {
    condition     = strcontains(regex("(?s)resource \"google_compute_instance\" \"host\" \\{.*?\\n\\}", file("${path.module}/main.tf")), "replace_triggered_by = [google_compute_address.host.address]")
    error_message = "A new address must replace the VM in the plan; otherwise the plan shows an update and the apply fails on an inconsistent final plan."
  }
  assert {
    condition     = strcontains(regex("(?s)resource \"google_project_iam_custom_role\" \"snapshots\" \\{.*?\\n\\}", file("${path.module}/monitoring.tf")), "depends_on = [google_project_service.required]")
    error_message = "A custom role cannot list a permission of a service whose API is off, so it must wait for the API, or the first apply on a project with it off fails."
  }
  assert {
    condition = alltrue([for grant in [
      "google_storage_bucket_iam_policy.storage",
      "google_storage_bucket_iam_member.existing",
      "google_artifact_registry_repository_iam_member.mirror",
      "google_secret_manager_secret_iam_member.store_generated",
      "google_project_iam_member.snapshots",
    ] : strcontains(regex("(?s)depends_on = \\[(.*?)\\]", regex("(?s)resource \"google_compute_instance\" \"host\" \\{.*?\\n\\}", file("${path.module}/main.tf")))[0], grant)])
    error_message = "The agent acts on new metadata within a cycle, so the VM must change only once the host holds the grants its configuration relies on: otherwise an apply that sets storage can start a move the host cannot write."
  }
}
