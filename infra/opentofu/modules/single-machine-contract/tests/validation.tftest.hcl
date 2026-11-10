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
      "storage" : { "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds" }, "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files" } },
      "scaling" : { "app" : { "min" : 2, "max" : 20 }, "api" : { "min" : 2, "max" : 20 } }
    }
  }
  expect_failures = [var.installation]
}

# Each private subnet of a new /20 AWS network holds 11 addresses. The default
# maximums (4, 4, 2, and 1), the scheduler, and two internal load balancers
# need 14 while a new host image rolls.
run "scalable_maximums_fit_the_private_subnets_of_a_new_aws_network" {
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
      "network" : { "cidr" : "10.68.0.0/20" },
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" }, "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" } }
    }
  }
  expect_failures = [var.installation]
}

run "scalable_maximums_a_small_aws_network_holds_are_accepted" {
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
      "app_exposure" : "public",
      "network" : { "cidr" : "10.68.0.0/20" },
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" }, "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" } },
      "scaling" : { "app" : { "min" : 2, "max" : 3 }, "api" : { "min" : 2, "max" : 3 } }
    }
  }
  assert {
    condition     = output.roles.groups.app.max + output.roles.groups.api.max + output.roles.groups.worker.max + output.roles.groups["task-worker"].max == 9
    error_message = "Maximums of 3, 3, 2, and 1, the scheduler, and one internal load balancer are the 11 addresses a /20 network's private subnet holds."
  }
}

run "default_scalable_maximums_fit_a_slash_19_aws_network" {
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
      "network" : { "cidr" : "10.68.0.0/19" },
      "data" : { "postgres" : { "mode" : "managed" }, "valkey" : { "mode" : "managed" } },
      "storage" : { "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" }, "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" } }
    }
  }
  assert {
    condition     = output.configuration.network.cidr == "10.68.0.0/19"
    error_message = "The default maximums need 14 addresses of the 27 a /19 network's private subnet holds."
  }
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
      "storage" : { "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" }, "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" } }
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
      "storage" : { "build" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "builds" }, "file" : { "backend" : "s3", "bucket" : "acme-hot", "region" : "us-east-1", "prefix" : "files" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_synthetic_event_is_not_a_reserved_type" {
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
      "monitoring" : { "synthetic_event" : "hot:rerun" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
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

run "settings_the_bundle_sets_are_refused" {
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
      "settings" : { "HOT_LOG_LEVEL" : "debug" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "storage_settings_are_refused" {
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
      "settings" : { "HOT_FILE_STORAGE_TYPE" : "local" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "the_models_path_setting_is_refused" {
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
      "settings" : { "HOT_STORE_MODELS_PATH" : "/tmp" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_setting_naming_a_file_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY_FILE" : "/run/secrets/proxy" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_with_credentials_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://hot:example@proxy.internal:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_without_credentials_is_accepted" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://proxy.internal:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_NETWORK_EGRESS_PROXY"] == "http://proxy.internal:3128"
    error_message = "A proxy with no credentials is an ordinary setting."
  }
}

run "settings_the_bundle_leaves_open_are_accepted" {
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
      "settings" : { "HOT_APP_SIGNUP" : "open", "HOT_EMAIL_PROVIDER" : "smtp", "HOT_WORKER_DB_MAX_CONNECTIONS" : "10" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_APP_SIGNUP"] == "open"
    error_message = "Sign-up, email, and a single machine's pool size stay the operator's to set."
  }
}

run "a_name_ending_in_a_hyphen_is_refused" {
  command = plan
  variables {
    installation = {
      "name" : "abc-",
      "cloud" : "aws",
      "region" : "us-east-1",
      "zone" : "us-east-1a",
      "host_image" : "ami-0123456789abcdef0",
      "app_hostname" : "app.hot.internal",
      "api_hostname" : "api.hot.internal",
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_network_with_host_bits_is_refused" {
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
      "network" : { "mode" : "existing", "network_id" : "hot", "subnet_id" : "hot", "cidr" : "10.20.0.1/16" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_client_range_with_host_bits_is_refused" {
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
      "network" : { "client_cidrs" : ["203.0.113.5/24"] },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_ipv6_network_is_refused" {
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
      "network" : { "cidr" : "fd00::/48" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_new_aws_network_too_small_for_its_subnets_is_refused" {
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
      "network" : { "cidr" : "10.68.0.0/22" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_new_aws_network_larger_than_a_vpc_is_refused" {
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
      "network" : { "cidr" : "10.0.0.0/8" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_public_aws_endpoint_needs_load_balancer_subnets" {
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
      "app_exposure" : "public",
      "network" : { "cidr" : "10.68.0.0/20" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "network_ranges_the_clouds_build_are_accepted" {
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
      "network" : { "cidr" : "10.68.0.0/20", "client_cidrs" : ["203.0.113.0/24", "198.51.100.7/32"] },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.network.cidr == "10.68.0.0/20"
    error_message = "A private AWS installation may use a /20 network."
  }
}

run "a_wildcard_prefix_is_refused" {
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
        "build" : { "backend" : "s3", "bucket" : "customer-hot-storage", "region" : "us-east-1", "prefix" : "hot-example/builds" },
        "file" : { "backend" : "s3", "bucket" : "customer-hot-storage", "region" : "us-east-1", "prefix" : "*" }
      }
    }
  }
  expect_failures = [var.installation]
}

run "a_quoted_prefix_is_refused" {
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
        "build" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "builds" },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "prefix" : "files\") || true || (\"" }
      }
    }
  }
  expect_failures = [var.installation]
}

run "a_wildcard_bucket_is_refused" {
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
        "build" : { "backend" : "local" },
        "file" : { "backend" : "s3", "bucket" : "*", "region" : "us-east-1", "prefix" : "files" }
      }
    }
  }
  expect_failures = [var.installation]
}

# The renderer refuses these on the host, so the contract refuses them at plan.
run "an_s3_region_with_a_dollar_is_refused" {
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
        "build" : { "backend" : "local" },
        "file" : { "backend" : "s3", "bucket" : "customer-hot-storage", "region" : "us$east", "prefix" : "files" }
      }
    }
  }
  expect_failures = [var.installation]
}

run "an_uppercase_s3_region_is_refused" {
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
        "build" : { "backend" : "local" },
        "file" : { "backend" : "s3", "bucket" : "customer-hot-storage", "region" : "US-EAST-1", "prefix" : "files" }
      }
    }
  }
  expect_failures = [var.installation]
}

# Only an S3 backend reads a storage region, as in the renderer.
run "a_region_on_other_backends_is_not_checked" {
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
        "build" : { "backend" : "local", "region" : "us$east" },
        "file" : { "backend" : "gcs", "bucket" : "acme-hot", "region" : "us$east", "prefix" : "files" }
      }
    }
  }
}

run "a_local_database_refuses_another_runtime_role" {
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
      "data" : { "postgres" : { "mode" : "local", "runtime_role" : "hot_app" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "the_default_local_database_refuses_another_runtime_role" {
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
      "data" : { "postgres" : { "runtime_role" : "hot_app" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_local_database_accepts_hot_runtime" {
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
      "data" : { "postgres" : { "mode" : "local", "runtime_role" : "hot_runtime" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.data.postgres.runtime_role == "hot_runtime"
    error_message = "A local database keeps the hot_runtime login."
  }
}

run "a_managed_database_keeps_its_runtime_role" {
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
      "data" : { "postgres" : { "mode" : "managed", "runtime_role" : "hot_app" } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.data.postgres.runtime_role == "hot_app"
    error_message = "A managed database's runtime role stays the operator's to name."
  }
}

run "a_deny_entry_that_is_a_hostname_is_refused" {
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
      "settings" : { "HOT_NETWORK_DENY_CIDRS" : "10.50.0.10/32,vault.internal" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_deny_entry_with_a_bad_prefix_is_refused" {
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
      "settings" : { "HOT_NETWORK_DENY_CIDRS" : "10.50.0.0/33" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "deny_entries_the_renderer_parses_are_accepted" {
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
      "settings" : { "HOT_NETWORK_DENY_CIDRS" : "10.20.0.5, 10.50.0.10/32,10.20.5.1/24, 2600:1f18::/64,::ffff:10.20.0.5" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = strcontains(output.configuration.settings["HOT_NETWORK_DENY_CIDRS"], "2600:1f18::/64")
    error_message = "Addresses and networks of either family are valid deny entries."
  }
}

run "a_deny_entry_with_a_leading_zero_in_an_address_is_refused" {
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
      "settings" : { "HOT_NETWORK_DENY_CIDRS" : "010.0.0.1" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_deny_entry_with_a_leading_zero_in_a_prefix_is_refused" {
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
      "settings" : { "HOT_NETWORK_DENY_CIDRS" : "10.0.0.0/08" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_deny_entry_with_a_leading_zero_after_ipv6_is_refused" {
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
      "settings" : { "HOT_NETWORK_DENY_CIDRS" : "::ffff:010.0.0.1" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_deny_entry_with_a_long_ipv6_group_is_refused" {
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
      "settings" : { "HOT_NETWORK_DENY_CIDRS" : "::00001/128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_deny_entry_with_a_long_leading_ipv6_group_is_refused" {
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
      "settings" : { "HOT_NETWORK_DENY_CIDRS" : "02600::1/64" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "ipv6_groups_with_leading_zeros_are_accepted_as_deny_entries" {
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
      "settings" : { "HOT_NETWORK_DENY_CIDRS" : "::0001,::0001:10.0.0.1/128,0.0.0.0/0" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = strcontains(output.configuration.settings["HOT_NETWORK_DENY_CIDRS"], "::0001:10.0.0.1/128")
    error_message = "The renderer takes leading zeros in an IPv6 group."
  }
}

run "an_allow_entry_with_a_netmask_is_refused" {
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
      "settings" : { "HOT_NETWORK_ALLOW_CIDRS" : "10.50.0.0/16,10.0.0.0/255.0.0.0" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_allow_entry_with_a_typo_is_refused" {
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
      "settings" : { "HOT_NETWORK_ALLOW_CIDRS" : "10.50.0.0/16,10.50.0.0./16" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_allow_entry_with_a_long_ipv6_group_is_refused" {
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
      "settings" : { "HOT_NETWORK_ALLOW_CIDRS" : "::00001/128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_allow_entry_with_a_long_leading_ipv6_group_is_refused" {
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
      "settings" : { "HOT_NETWORK_ALLOW_CIDRS" : "02600::1/64" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "allow_entries_the_renderer_parses_are_accepted" {
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
      "settings" : { "HOT_NETWORK_ALLOW_CIDRS" : "10.50.0.10, 10.50.0.11/24,2600::1,," },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = strcontains(output.configuration.settings["HOT_NETWORK_ALLOW_CIDRS"], "2600::1")
    error_message = "A bare address, host bits, IPv6, spaces, and empty entries are as Hot reads them."
  }
}

run "a_call_retention_with_a_unit_is_refused" {
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
      "settings" : { "HOT_DB_RETENTION_CALL_DAYS" : "30d" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_call_retention_of_zero_is_refused" {
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
      "settings" : { "HOT_DB_RETENTION_CALL_DAYS" : "0" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_call_retention_with_a_leading_zero_is_refused" {
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
      "settings" : { "HOT_DB_RETENTION_CALL_DAYS" : "030" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_call_retention_over_the_limit_is_refused" {
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
      "settings" : { "HOT_DB_RETENTION_CALL_DAYS" : "36501" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "keeping_calls_forever_is_accepted" {
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
      "settings" : { "HOT_DB_RETENTION_CALL_DAYS" : "-1" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_DB_RETENTION_CALL_DAYS"] == "-1"
    error_message = "The renderer accepts this call retention."
  }
}

run "the_shortest_call_retention_is_accepted" {
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
      "settings" : { "HOT_DB_RETENTION_CALL_DAYS" : "1" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_DB_RETENTION_CALL_DAYS"] == "1"
    error_message = "The renderer accepts this call retention."
  }
}

run "the_longest_call_retention_is_accepted" {
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
      "settings" : { "HOT_DB_RETENTION_CALL_DAYS" : "36500" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_DB_RETENTION_CALL_DAYS"] == "36500"
    error_message = "The renderer accepts this call retention."
  }
}

run "the_known_hosts_setting_is_refused" {
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
      "settings" : { "HOT_API_KNOWN_HOSTS" : "evil.example" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "the_app_port_setting_is_refused" {
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
      "settings" : { "HOT_APP_PORT" : "8080" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "the_api_port_setting_is_refused" {
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
      "settings" : { "HOT_API_PORT" : "8081" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "the_database_schema_setting_is_refused" {
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
      "settings" : { "HOT_DB_SCHEMA" : "other" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "turning_off_row_level_security_is_refused" {
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
      "settings" : { "HOT_DB_RLS_ENFORCE" : "false" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_without_a_scheme_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "proxy.internal:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_with_another_scheme_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "socks5://proxy.internal:1080" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_with_a_path_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://proxy.internal:3128/path" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_without_a_host_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_with_a_bad_port_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://proxy.internal:port" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_https_proxy_on_an_ipv6_address_is_accepted" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "https://[2600:1f18::1]:3128/" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_NETWORK_EGRESS_PROXY"] == "https://[2600:1f18::1]:3128/"
    error_message = "Hot reads this as a proxy address."
  }
}

run "a_blank_proxy_is_accepted" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : " " },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_NETWORK_EGRESS_PROXY"] == " "
    error_message = "Hot reads a blank proxy as none."
  }
}

run "an_allow_host_with_a_scheme_and_port_is_refused" {
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
      "settings" : { "HOT_NETWORK_ALLOW_HOSTS" : "https://x.internal:443" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_allow_host_with_a_port_is_refused" {
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
      "settings" : { "HOT_NETWORK_ALLOW_HOSTS" : "x.internal:443" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_bare_wildcard_allow_host_is_refused" {
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
      "settings" : { "HOT_NETWORK_ALLOW_HOSTS" : "*" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_allow_host_with_an_inner_wildcard_is_refused" {
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
      "settings" : { "HOT_NETWORK_ALLOW_HOSTS" : "a.*.internal" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_allow_host_after_a_good_one_is_still_checked" {
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
      "settings" : { "HOT_NETWORK_ALLOW_HOSTS" : "api.corp.example,https://x.internal" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "allow_hosts_hot_reads_are_accepted" {
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
      "settings" : { "HOT_NETWORK_ALLOW_HOSTS" : "api.corp.example, *.svc.internal,,My_Host" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_NETWORK_ALLOW_HOSTS"] == "api.corp.example, *.svc.internal,,My_Host"
    error_message = "Exact names, wildcard subdomains, spaces, and empty entries are as Hot reads them."
  }
}

run "a_guest_resolver_that_is_a_name_is_refused" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "dns.example" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_ipv6_guest_resolver_is_refused" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "2001:4860:4860::8888" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_guest_resolver_with_a_leading_zero_is_refused" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "010.0.0.1" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_guest_resolver_out_of_range_is_refused" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "10.0.0.256" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_link_local_guest_resolver_is_refused" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "169.254.169.253" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_loopback_guest_resolver_is_refused" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "127.0.0.53" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_zero_network_guest_resolver_is_refused" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "0.0.0.0" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_guest_network_resolver_is_refused" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "10.88.0.1" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_multicast_guest_resolver_is_refused" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "224.0.0.251" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_reserved_guest_resolver_is_refused" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "255.255.255.255" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_guest_resolver_after_a_good_one_is_still_checked" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "10.68.0.2,dns.example" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "guest_resolvers_hot_reads_are_accepted" {
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
      "settings" : { "HOT_BOX_GUEST_DNS" : "10.68.0.2, 1.1.1.1,,223.255.255.255" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_BOX_GUEST_DNS"] == "10.68.0.2, 1.1.1.1,,223.255.255.255"
    error_message = "Reachable IPv4 resolvers, spaces, and empty entries are as Hot reads them."
  }
}

run "an_unknown_signup_policy_is_refused" {
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
      "settings" : { "HOT_APP_SIGNUP" : "closed" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "invite_only_signup_is_accepted" {
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
      "settings" : { "HOT_APP_SIGNUP" : "invite" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_APP_SIGNUP"] == "invite"
    error_message = "The app reads this sign-up policy."
  }
}

run "an_unknown_redis_auth_is_refused" {
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
      "settings" : { "HOT_REDIS_AUTH" : "iam" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "password_redis_auth_is_accepted" {
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
      "settings" : { "HOT_REDIS_AUTH" : "password" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_REDIS_AUTH"] == "password"
    error_message = "Hot reads this Valkey login."
  }
}

run "gcp_iam_redis_auth_on_aws_is_refused" {
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
      "data" : { "valkey" : { "mode" : "existing" } },
      "settings" : { "HOT_REDIS_AUTH" : "gcp-iam" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "gcp_iam_redis_auth_on_a_local_valkey_is_refused" {
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
      "settings" : { "HOT_REDIS_AUTH" : "gcp-iam" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "gcp_iam_redis_auth_on_an_existing_valkey_is_accepted" {
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
      "data" : { "valkey" : { "mode" : "existing" } },
      "settings" : { "HOT_REDIS_AUTH" : "gcp-iam" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_REDIS_AUTH"] == "gcp-iam"
    error_message = "An operator's own Memorystore can use the VM's identity."
  }
}

run "redis_auth_on_managed_valkey_on_gcp_is_refused" {
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
      "data" : { "valkey" : { "mode" : "managed" } },
      "settings" : { "HOT_REDIS_AUTH" : "password" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_redis_ca_on_managed_valkey_on_gcp_is_refused" {
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
      "data" : { "valkey" : { "mode" : "managed" } },
      "settings" : { "HOT_REDIS_CA_CERT" : "-----BEGIN CERTIFICATE-----\nexample\n-----END CERTIFICATE-----\n" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_redis_ca_on_managed_valkey_on_aws_is_accepted" {
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
      "data" : { "valkey" : { "mode" : "managed" } },
      "settings" : { "HOT_REDIS_CA_CERT" : "-----BEGIN CERTIFICATE-----\nexample\n-----END CERTIFICATE-----\n" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_REDIS_CA_CERT"] == "-----BEGIN CERTIFICATE-----\nexample\n-----END CERTIFICATE-----\n"
    error_message = "Only GCP's bundle mounts the instance's CA."
  }
}

run "a_redis_ca_on_an_existing_valkey_is_accepted" {
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
      "data" : { "valkey" : { "mode" : "existing" } },
      "settings" : { "HOT_REDIS_CA_CERT" : "-----BEGIN CERTIFICATE-----\nexample\n-----END CERTIFICATE-----\n" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_REDIS_CA_CERT"] == "-----BEGIN CERTIFICATE-----\nexample\n-----END CERTIFICATE-----\n"
    error_message = "An operator's own Valkey can name its CA."
  }
}

run "a_redis_ca_that_is_not_pem_is_refused" {
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
      "data" : { "valkey" : { "mode" : "existing" } },
      "settings" : { "HOT_REDIS_CA_CERT" : "/etc/ssl/valkey.pem" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "the_short_row_level_security_setting_is_refused" {
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
      "settings" : { "HOT_RLS_ENFORCE" : "false" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "replacing_the_database_settings_is_refused" {
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
      "settings" : { "HOT_DB" : "other" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "replacing_the_app_settings_is_refused" {
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
      "settings" : { "HOT_APP" : "{\"port\":9000}" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "replacing_the_api_settings_is_refused" {
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
      "settings" : { "HOT_API" : "{\"port\":9000}" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "choosing_the_box_data_directory_is_refused" {
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
      "settings" : { "HOT_TASK_DATA_VOLUME_DIR" : "/tmp/box-data" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "changing_the_reserved_task_worker_memory_is_refused" {
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
      "settings" : { "HOT_TASK_CONTAINER_RESERVED_MEMORY_MB" : "0" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "changing_the_code_memory_share_is_refused" {
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
      "settings" : { "HOT_TASK_CODE_MEMORY_PERCENT" : "90" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "choosing_the_store_backend_is_refused" {
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
      "settings" : { "HOT_STORE_TYPE" : "sqlite" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_with_a_port_out_of_range_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://proxy.internal:65536" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_name_with_other_characters_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://pro%xy.internal:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_name_ending_in_a_number_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://proxy.1:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_name_ending_in_a_hex_number_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://proxy.0x1f:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_on_an_invalid_ipv4_address_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://10.0.0.256:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_on_an_invalid_ipv6_address_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://[2600::1::2]:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_on_a_bracketed_ipv4_address_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://[10.0.0.1]:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_on_a_long_ipv6_group_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://[::00001]:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_on_ipv6_with_a_leading_zero_after_it_is_refused" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://[::ffff:010.0.0.1]:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_proxy_on_an_ipv4_address_is_accepted" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://10.30.0.7:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_NETWORK_EGRESS_PROXY"] == "http://10.30.0.7:3128"
    error_message = "An IPv4 address is a host."
  }
}

run "a_proxy_on_the_highest_port_is_accepted" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://proxy.internal:65535/" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_NETWORK_EGRESS_PROXY"] == "http://proxy.internal:65535/"
    error_message = "The highest port and a bare slash are a proxy address."
  }
}

run "a_proxy_on_ipv6_with_leading_zeros_in_a_group_is_accepted" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://[::0001]:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_NETWORK_EGRESS_PROXY"] == "http://[::0001]:3128"
    error_message = "An IPv6 group can have leading zeros."
  }
}

run "a_proxy_name_with_a_trailing_dot_is_accepted" {
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
      "settings" : { "HOT_NETWORK_EGRESS_PROXY" : "http://proxy.internal.:3128" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_NETWORK_EGRESS_PROXY"] == "http://proxy.internal.:3128"
    error_message = "A trailing dot is a fully qualified name."
  }
}

run "a_redis_cluster_with_gcp_iam_is_refused" {
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
      "data" : { "valkey" : { "mode" : "existing" } },
      "settings" : { "HOT_REDIS_CLUSTER" : "true", "HOT_REDIS_AUTH" : "gcp-iam" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_redis_cluster_with_a_ca_is_refused" {
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
      "data" : { "valkey" : { "mode" : "existing" } },
      "settings" : { "HOT_REDIS_CLUSTER" : "1", "HOT_REDIS_CA_CERT" : "-----BEGIN CERTIFICATE-----\nexample\n-----END CERTIFICATE-----\n" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_redis_cluster_on_managed_valkey_on_gcp_is_refused" {
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
      "data" : { "valkey" : { "mode" : "managed" } },
      "settings" : { "HOT_REDIS_CLUSTER" : "yes" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_redis_cluster_on_an_existing_valkey_is_accepted" {
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
      "data" : { "valkey" : { "mode" : "existing" } },
      "settings" : { "HOT_REDIS_CLUSTER" : "true" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_REDIS_CLUSTER"] == "true"
    error_message = "A cluster with a password in its URI is as Hot reads it."
  }
}

run "a_redis_cluster_turned_off_is_not_checked" {
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
      "data" : { "valkey" : { "mode" : "existing" } },
      "settings" : { "HOT_REDIS_CLUSTER" : "false", "HOT_REDIS_AUTH" : "gcp-iam" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_REDIS_CLUSTER"] == "false"
    error_message = "Only a cluster that is on conflicts with gcp-iam."
  }
}

run "a_null_setting_is_refused" {
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
      "settings" : { "HOT_EMAIL_FROM" : null },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_null_email_provider_is_refused" {
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
      "settings" : { "HOT_EMAIL_PROVIDER" : null },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_null_email_setting_with_smtp_is_refused" {
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
      "settings" : { "HOT_EMAIL_PROVIDER" : "smtp", "HOT_EMAIL_SMTP_SECURITY" : null },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_null_storage_move_skip_is_refused" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP" : null },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_smtp_ca_that_is_not_pem_is_refused" {
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
      "settings" : { "HOT_EMAIL_PROVIDER" : "smtp", "HOT_EMAIL_SMTP_CA_CERT" : "abc" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_smtp_ca_that_is_not_pem_is_refused_for_alerts_alone" {
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
      "settings" : { "HOT_EMAIL_ALERTS_PROVIDER" : " SMTP ", "HOT_EMAIL_SMTP_CA_CERT" : "/etc/ssl/relay.pem" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_smtp_ca_without_tls_is_refused" {
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
      "settings" : { "HOT_EMAIL_PROVIDER" : "smtp", "HOT_EMAIL_SMTP_SECURITY" : "none", "HOT_EMAIL_SMTP_CA_CERT" : "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_smtp_ca_without_tls_is_refused_however_it_is_written" {
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
      "settings" : { "HOT_EMAIL_PROVIDER" : "smtp", "HOT_EMAIL_SMTP_SECURITY" : " NONE ", "HOT_EMAIL_SMTP_CA_CERT" : "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_smtp_username_without_tls_is_refused" {
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
      "settings" : { "HOT_EMAIL_PROVIDER" : "smtp", "HOT_EMAIL_SMTP_SECURITY" : "none", "HOT_EMAIL_SMTP_USERNAME" : "hot" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "an_smtp_ca_with_tls_is_accepted" {
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
      "settings" : { "HOT_EMAIL_PROVIDER" : "smtp", "HOT_EMAIL_SMTP_SECURITY" : "tls", "HOT_EMAIL_SMTP_CA_CERT" : "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_EMAIL_SMTP_SECURITY"] == "tls"
    error_message = "A private relay's CA needs TLS, and has it."
  }
}

run "a_blank_smtp_ca_without_tls_is_accepted" {
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
      "settings" : { "HOT_EMAIL_PROVIDER" : "smtp", "HOT_EMAIL_SMTP_SECURITY" : "none", "HOT_EMAIL_SMTP_CA_CERT" : "  " },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_EMAIL_SMTP_SECURITY"] == "none"
    error_message = "A blank certificate is no certificate, as the agent and Hot read it."
  }
}

run "smtp_without_tls_and_without_a_login_is_accepted" {
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
      "settings" : { "HOT_EMAIL_PROVIDER" : "smtp", "HOT_EMAIL_SMTP_SECURITY" : "none" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_EMAIL_SMTP_SECURITY"] == "none"
    error_message = "A relay on a trusted network needs neither TLS nor a login."
  }
}

run "smtp_settings_are_not_checked_without_smtp" {
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
      "settings" : { "HOT_EMAIL_PROVIDER" : "none", "HOT_EMAIL_SMTP_SECURITY" : "none", "HOT_EMAIL_SMTP_USERNAME" : "hot", "HOT_EMAIL_SMTP_CA_CERT" : "abc" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_EMAIL_SMTP_CA_CERT"] == "abc"
    error_message = "The agent reads these only when email goes through SMTP."
  }
}

run "a_storage_move_skip_that_is_not_an_id_is_refused" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP" : "abc" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_storage_move_skip_with_one_bad_id_is_refused" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP" : "0192d4f0-7a3b-7c21-9f10-3b5e8a6c4d21,0192d4f0-7a3b-7c21-9f10-3b5e8a6c4d210" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_storage_move_skip_of_ids_is_accepted" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP" : " 0192d4f0-7a3b-7c21-9f10-3b5e8a6c4d21, 0192D4F0-7A3B-7C21-9F10-3B5E8A6C4D21 ," },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_MOVE_STORAGE_SKIP"] == " 0192d4f0-7a3b-7c21-9f10-3b5e8a6c4d21, 0192D4F0-7A3B-7C21-9F10-3B5E8A6C4D21 ,"
    error_message = "Ids may have spaces around them, either case, and a trailing comma, as the agent reads them."
  }
}

run "a_storage_move_skip_failed_count_of_one_is_accepted" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP_FAILED" : "1" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_MOVE_STORAGE_SKIP_FAILED"] == "1"
    error_message = "One failed object is the smallest count to leave behind."
  }
}

run "a_storage_move_skip_failed_count_past_what_the_id_list_holds_is_accepted" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP_FAILED" : "120000" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.settings["HOT_MOVE_STORAGE_SKIP_FAILED"] == "120000"
    error_message = "The count has no upper limit: the agent checks it against the last report."
  }
}

run "a_storage_move_skip_failed_count_of_zero_is_refused" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP_FAILED" : "0" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_negative_storage_move_skip_failed_count_is_refused" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP_FAILED" : "-1" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_fractional_storage_move_skip_failed_count_is_refused" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP_FAILED" : "1.5" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_storage_move_skip_failed_count_with_a_leading_zero_is_refused" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP_FAILED" : "012" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_storage_move_skip_failed_that_is_not_a_number_is_refused" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP_FAILED" : "0192d4f0-7a3b-7c21-9f10-3b5e8a6c4d21" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_blank_storage_move_skip_failed_is_refused" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP_FAILED" : "" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_null_storage_move_skip_failed_is_refused" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP_FAILED" : null },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

# The agent writes the skip file and names it for the job; settings name no files.
run "a_storage_move_skip_file_setting_is_refused" {
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
      "settings" : { "HOT_MOVE_STORAGE_SKIP_FILE" : "/run/skip" },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_null_signup_setting_is_refused" {
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
      "settings" : { "HOT_APP_SIGNUP" : null },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "data_deny_cidrs_default_to_none" {
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
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = length(output.configuration.data.postgres.endpoint.deny_cidrs) == 0 && length(output.configuration.data.valkey.endpoint.deny_cidrs) == 0
    error_message = "Without any, a service denies only its own address, and a local one takes the default."
  }
}

run "data_deny_cidrs_of_managed_and_existing_services_are_accepted" {
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
      "data" : {
        "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.1.2.0/24", "10.1.3.0/24", "10.1.4.5/32"] } },
        "valkey" : { "mode" : "existing", "endpoint" : { "deny_cidrs" : ["fd00:1::/48", "2600:1f18:0:1::1/128"] } }
      },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.data.postgres.endpoint.deny_cidrs == tolist(["10.1.2.0/24", "10.1.3.0/24", "10.1.4.5/32"]) && output.configuration.data.valkey.endpoint.deny_cidrs == tolist(["fd00:1::/48", "2600:1f18:0:1::1/128"])
    error_message = "IPv4 and IPv6 networks reach the stacks, which pass them to the host."
  }
}

run "data_deny_cidrs_beside_the_prefixes_that_carry_ipv4_are_accepted" {
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
      "data" : {
        "valkey" : { "mode" : "existing", "endpoint" : { "deny_cidrs" : ["::/95", "::1:0:0/96", "64:ff9b:1::/96", "fd00::a00:0/104"] } }
      },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  assert {
    condition     = output.configuration.data.valkey.endpoint.deny_cidrs == tolist(["::/95", "::1:0:0/96", "64:ff9b:1::/96", "fd00::a00:0/104"])
    error_message = "Hot reads these as IPv6, so they cut no IPv4 egress and stay allowed."
  }
}

run "a_data_deny_cidr_that_is_not_an_address_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["db.internal/24"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_without_a_prefix_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.1.2.10"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_with_host_bits_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.1.2.10/24"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_of_everything_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["0.0.0.0/0"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_of_every_ipv6_address_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["::/0"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_of_the_ipv4_compatible_prefix_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["::/96"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_of_the_nat64_prefix_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["64:ff9b::/96"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_of_half_of_ipv4_in_the_ipv4_compatible_prefix_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["::/97"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_of_ipv4_in_the_ipv4_compatible_prefix_is_refused" {
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
      "data" : { "valkey" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.1.2.0/24", "::a00:0/104"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_of_a_host_in_the_nat64_prefix_is_refused" {
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
      "data" : { "valkey" : { "mode" : "existing", "endpoint" : { "deny_cidrs" : ["64:FF9B::a14:105/128"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_of_ipv4_mapped_into_ipv6_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["::ffff:a00:0/104"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_with_too_long_a_prefix_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.1.2.0/33"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_with_a_leading_zero_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["010.1.2.0/24"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_data_deny_cidr_with_a_netmask_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.1.2.0/255.255.255.0"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_bad_valkey_deny_cidr_is_refused" {
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
      "data" : { "valkey" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : ["10.1.2.0/24", "not-a-network"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_bad_deny_cidr_of_an_existing_service_is_refused" {
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
      "data" : { "valkey" : { "mode" : "existing", "endpoint" : { "deny_cidrs" : ["10.1.2.10"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "a_null_data_deny_cidr_is_refused" {
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
      "data" : { "postgres" : { "mode" : "managed", "endpoint" : { "deny_cidrs" : [null] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "data_deny_cidrs_of_a_local_service_are_refused" {
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
      "data" : { "postgres" : { "mode" : "local", "endpoint" : { "deny_cidrs" : ["10.1.2.0/24"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}

run "data_deny_cidrs_of_a_default_local_service_are_refused" {
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
      "data" : { "valkey" : { "endpoint" : { "deny_cidrs" : ["10.1.2.0/24"] } } },
      "storage" : { "build" : { "backend" : "local" }, "file" : { "backend" : "local" } }
    }
  }
  expect_failures = [var.installation]
}
