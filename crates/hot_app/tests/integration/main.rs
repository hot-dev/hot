// Consolidated integration test binary to avoid linking dozens of
// separate test executables in CI and local builds.

#[allow(dead_code, unused_macros)]
#[path = "../../../hot/tests/support/privileged_fixture.rs"]
pub mod privileged_fixture;

mod email_case;
mod http_api_keys;
mod http_auth_redirects;
mod http_blob_rehydration;
mod http_call_retention;
mod http_isolation_review;
mod http_link_crawl;
mod http_new_org;
mod http_run_rerun;
mod http_run_stream_graph;
mod http_script_safety;
mod http_security_review;
mod http_signup_flow;
mod http_signup_policy;
mod http_stores;
mod http_sub_orgs;
mod postgres_scope;
mod postgres_signup_policy;
mod signup_flow;
