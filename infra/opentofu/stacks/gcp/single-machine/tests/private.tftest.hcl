mock_provider "google" {}
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
    condition     = google_compute_instance.host.metadata["block-project-ssh-keys"] == "TRUE" && google_compute_instance.host.metadata["serial-port-enable"] == "FALSE"
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
