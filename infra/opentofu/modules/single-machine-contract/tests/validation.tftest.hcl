variables {
  installation = {
    "schema_version" : 1,
    "name" : "hot-example",
    "cloud" : "aws",
    "region" : "us-east-1",
    "zone" : "us-east-1a",
    "size" : "small",
    "host_image" : "ami-0123456789abcdef0",
    "project" : "",
    "release_pointer" : "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
    "secret_references" : {
      "app" : "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/app-example"
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

run "unknown_backend" {
  command = plan
  variables { installation = {
    "schema_version" : 1,
    "name" : "hot-example",
    "cloud" : "aws",
    "region" : "us-east-1",
    "zone" : "us-east-1a",
    "size" : "small",
    "host_image" : "ami-0123456789abcdef0",
    "project" : "",
    "release_pointer" : "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
    "secret_references" : {
      "app" : "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/app-example"
    },
    "app_hostname" : "app.hot.internal",
    "api_hostname" : "api.hot.internal",
    "storage" : {
      "build" : {
        "backend" : "local"
      },
      "file" : {
        "backend" : "azure"
      }
    }
  } }
  expect_failures = [var.installation]
}

run "s3_missing_bucket" {
  command = plan
  variables { installation = {
    "schema_version" : 1,
    "name" : "hot-example",
    "cloud" : "aws",
    "region" : "us-east-1",
    "zone" : "us-east-1a",
    "size" : "small",
    "host_image" : "ami-0123456789abcdef0",
    "project" : "",
    "release_pointer" : "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
    "secret_references" : {
      "app" : "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/app-example"
    },
    "app_hostname" : "app.hot.internal",
    "api_hostname" : "api.hot.internal",
    "storage" : {
      "build" : {
        "backend" : "local"
      },
      "file" : {
        "backend" : "s3"
      }
    }
  } }
  expect_failures = [var.installation]
}

run "public_on_existing_network_needs_two_public_subnets" {
  command = plan
  variables { installation = {
    "schema_version" : 1,
    "name" : "hot-example",
    "cloud" : "aws",
    "region" : "us-east-1",
    "zone" : "us-east-1a",
    "size" : "small",
    "host_image" : "ami-0123456789abcdef0",
    "project" : "",
    "release_pointer" : "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
    "secret_references" : {
      "app" : "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/app-example"
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
    "api_exposure" : "public",
    "network" : { "mode" : "existing", "network_id" : "vpc-0123", "subnet_id" : "subnet-0123" }
  } }
  expect_failures = [var.installation]
}

run "open_client_range" {
  command = plan
  variables { installation = {
    "schema_version" : 1,
    "name" : "hot-example",
    "cloud" : "aws",
    "region" : "us-east-1",
    "zone" : "us-east-1a",
    "size" : "small",
    "host_image" : "ami-0123456789abcdef0",
    "project" : "",
    "release_pointer" : "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
    "secret_references" : {
      "app" : "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/app-example"
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
      "client_cidrs" : [
        "0.0.0.0/0"
      ]
    }
  } }
  expect_failures = [var.installation]
}

run "existing_network_missing_ids" {
  command = plan
  variables { installation = {
    "schema_version" : 1,
    "name" : "hot-example",
    "cloud" : "aws",
    "region" : "us-east-1",
    "zone" : "us-east-1a",
    "size" : "small",
    "host_image" : "ami-0123456789abcdef0",
    "project" : "",
    "release_pointer" : "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
    "secret_references" : {
      "app" : "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/app-example"
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
      "mode" : "existing"
    }
  } }
  expect_failures = [var.installation]
}

run "traversal_prefix" {
  command = plan
  variables { installation = {
    "schema_version" : 1,
    "name" : "hot-example",
    "cloud" : "aws",
    "region" : "us-east-1",
    "zone" : "us-east-1a",
    "size" : "small",
    "host_image" : "ami-0123456789abcdef0",
    "project" : "",
    "release_pointer" : "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
    "secret_references" : {
      "app" : "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/app-example"
    },
    "app_hostname" : "app.hot.internal",
    "api_hostname" : "api.hot.internal",
    "storage" : {
      "build" : {
        "backend" : "local"
      },
      "file" : {
        "backend" : "local",
        "prefix" : "../other-installation"
      }
    }
  } }
  expect_failures = [var.installation]
}

run "existing_postgres" {
  command = plan
  variables {
    installation = {
      "schema_version" : 1,
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "size" : "small",
      "host_image" : "ami-0123456789abcdef0",
      "project" : "",
      "release_pointer" : "arn:aws:ssm:us-east-1:123456789012:parameter/hot/hot-example/release",
      "secret_references" : {
        "app" : "arn:aws:secretsmanager:us-east-1:123456789012:secret:hot/hot-example/app-example"
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
      "data" : { "postgres" : { "mode" : "existing", "runtime_role" : "hot_app" } }
    }
  }
  assert {
    condition     = output.configuration.data.postgres.mode == "existing" && output.configuration.data.valkey.mode == "local"
    error_message = "Each data service selects its mode independently."
  }
}

run "managed_valkey_on_gcp_follows_the_size" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "medium",
      "host_image" : "projects/example/global/images/hot",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "valkey" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.managed_valkey.node_type == "HIGHMEM_MEDIUM"
    error_message = "Memorystore's node type follows the size."
  }
}

run "managed_postgres_sizes_follow_the_installation" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "gcp",
      "project" : "example-project",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "size" : "medium",
      "host_image" : "projects/example/global/images/hot",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed", "storage_gb" : 250 } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.managed_postgres.tier == "db-custom-4-16384" && output.managed_postgres.storage_gb == 250
    error_message = "The tier follows the size unless set; explicit storage wins."
  }
}

run "managed_postgres_on_an_existing_aws_network_needs_data_subnets" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed" } },
      "network" : { "mode" : "existing", "network_id" : "vpc-0123", "subnet_id" : "subnet-0123" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "managed_postgres_rejects_unsupported_versions" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed", "version" : "14" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "unknown_data_mode" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "clustered" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "managed_valkey_node_follows_the_size" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "size" : "large",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "valkey" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.managed_valkey.node_type == "cache.m7g.2xlarge" && output.managed_postgres == null
    error_message = "The node type follows the size; PostgreSQL stays local."
  }
}

run "log_retention_must_be_a_cloudwatch_value" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "logging" : { "retention_days" : 45 },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "restore_snapshots_name_the_clouds_snapshot" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "backup" : { "restore_snapshot" : "projects/example/global/snapshots/hot-data" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "provisioned_buckets_belong_to_the_installation_cloud" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example/global/images/hot",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot-builds", "region" : "us-east-1", "provision" : true },
        "file" : { "backend" : "local" }
      }
    }
  }
  expect_failures = [var.installation]
}

run "a_shared_bucket_needs_one_policy" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "storage" : {
        "build" : { "backend" : "s3", "bucket" : "acme-hot-data", "region" : "us-east-1", "provision" : true },
        "file" : { "backend" : "s3", "bucket" : "acme-hot-data", "region" : "us-east-1", "provision" : false }
      }
    }
  }
  expect_failures = [var.installation]
}

run "scalable_maximums_fit_the_connection_budget" {
  command = plan
  variables {
    installation = {
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example/global/images/hot",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "gcs", "bucket" : "b", "prefix" : "builds" }, "file" : { "backend" : "gcs", "bucket" : "b", "prefix" : "files" } },
      "scaling" : { "app" : { "min" : 2, "max" : 20 }, "api" : { "min" : 2, "max" : 20 } }
    }
  }
  expect_failures = [var.installation]
}

run "scalable_installations_need_shared_data_and_storage" {
  command = plan
  variables {
    installation = {
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example/global/images/hot",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "scalable_roles_follow_the_size" {
  command = plan
  variables {
    installation = {
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "size" : "medium",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "s3", "bucket" : "b", "region" : "us-east-1", "prefix" : "builds" }, "file" : { "backend" : "s3", "bucket" : "b", "region" : "us-east-1", "prefix" : "files" } }
    }
  }
  assert {
    condition     = output.roles.groups["app"].aws == "m7i.xlarge" && output.roles.groups["task-worker"].aws == "m8i.xlarge" && output.roles.groups["scheduler"].max == 1 && output.roles.groups["app"].min == 2
    error_message = "Roles use general-purpose machines, task-workers the nested family, and the scheduler exactly one instance."
  }
  assert {
    condition = (output.roles.groups["worker"].waiting_per_host == 50 && output.roles.groups["task-worker"].slots_target == 70
    && output.roles.groups["task-worker"].retire_idle_minutes == 30 && output.configuration.monitoring.queue_wait_alert == 900)
    error_message = "Scaling and wait alerts have defaults."
  }
}

run "scaling_targets_are_bounded" {
  command = plan
  variables {
    installation = {
      "preset" : "scalable",
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "scaling" : { "task_worker" : { "min" : 1, "max" : 3, "slots_target" : 5, "retire_idle_minutes" : 2 } },
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "s3", "bucket" : "b", "region" : "us-east-1", "prefix" : "builds" }, "file" : { "backend" : "s3", "bucket" : "b", "region" : "us-east-1", "prefix" : "files" } }
    }
  }
  expect_failures = [var.installation]
}

run "the_queue_wait_alert_is_at_least_a_minute" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "monitoring" : { "queue_wait_alert" : 30 },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_ecr_mirror_is_parsed_for_the_host_role" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "image_registry" : "123456789012.dkr.ecr.eu-west-1.amazonaws.com/hot-mirror",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.mirror.kind == "ecr" && output.mirror.account == "123456789012" && output.mirror.region == "eu-west-1" && output.mirror.repositories == "hot-mirror/*"
    error_message = "An ECR mirror names its account, region and repositories."
  }
}

run "an_artifact_registry_mirror_is_parsed_for_the_service_accounts" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example/global/images/hot",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "image_registry" : "us-docker.pkg.dev/acme-mirror/hot",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.mirror.kind == "artifact-registry" && output.mirror.location == "us" && output.mirror.project == "acme-mirror" && output.mirror.repository == "hot"
    error_message = "An Artifact Registry mirror names its location, project and repository."
  }
}

run "other_mirrors_are_pulled_anonymously" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "image_registry" : "mirror.example.com:5000/hot",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.mirror.kind == ""
    error_message = "Only ECR and Artifact Registry mirrors use the host identity."
  }
}

run "an_artifact_registry_mirror_names_its_repository" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example/global/images/hot",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "image_registry" : "us-docker.pkg.dev/acme-mirror",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_gcp_budget_needs_a_billing_account" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example/global/images/hot",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "monitoring" : { "monthly_budget" : 500 },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "the_queue_alert_needs_a_positive_threshold" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "monitoring" : { "queue_backlog_alert" : 0 },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_mirror_belongs_to_the_installation_cloud" {
  command = plan
  variables {
    installation = {
      "name" : "hot-example",
      "cloud" : "gcp",
      "region" : "us-central1",
      "zone" : "us-central1-a",
      "host_image" : "projects/example/global/images/hot",
      "project" : "example-project",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "image_registry" : "123456789012.dkr.ecr.us-east-1.amazonaws.com/hot-mirror",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}
