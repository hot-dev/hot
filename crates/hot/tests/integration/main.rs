// Consolidated integration test binary to avoid linking dozens of
// separate test executables in CI and local builds.

mod activation_event_postgres;
mod file_upload_postgres;
mod json_bytes_memory;
mod metadata_scope;
mod oversized_records;
mod oversized_values;
mod parallel_resource_scope;
mod postgres_advisory_lock;
mod postgres_queue_correctness;
mod postgres_rls;
mod postgres_upgrade;
mod queue_conformance;
mod schema_parity;
mod sqlite_upgrade;
mod ssh_sftp;
mod task_integration;
mod tool_schema_extraction;
mod type_checking_errors;
