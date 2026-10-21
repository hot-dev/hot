//! Building the GCS client must not rely on another subsystem having already
//! installed a rustls crypto provider. Hot enables both rustls backends, so
//! the SDK's gRPC transport used to panic here. This lives in its own test
//! binary so no other test can install a provider first.
#![cfg(feature = "gcs-storage")]

#[tokio::test]
async fn gcs_client_builds_without_a_preinstalled_crypto_provider() {
    let conf = hot::val!({"file": {"gcs": {"bucket": "hot-test-bucket"}}});
    let config = hot::gcs::GcsConfig::resolve(&conf, "file", |_| None).unwrap();
    let credentials = google_cloud_auth::credentials::anonymous::Builder::new().build();
    hot::gcs::GcsClient::connect_with_credentials(&config, credentials)
        .await
        .expect("GCS client builds without network access");
}
