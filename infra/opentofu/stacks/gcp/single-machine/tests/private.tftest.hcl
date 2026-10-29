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
