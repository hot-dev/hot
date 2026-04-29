use aws_lc_rs::{digest, pbkdf2};
use base64::{Engine as _, engine::general_purpose};
use serde_json::json;
use std::num::NonZeroU32;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AuthError {
    #[error("Password hashing failed: {0}")]
    PasswordHashing(String),
    #[error("Password verification failed")]
    PasswordVerification,
    #[error("Invalid password format")]
    InvalidPasswordFormat,
    #[error("JSON serialization error: {0}")]
    JsonSerialization(#[from] serde_json::Error),
    #[error("Random bytes generation failed: {0}")]
    RandomGeneration(String),
}

/// PBKDF2 algorithm for password hashing
static PBKDF2_ALG: pbkdf2::Algorithm = pbkdf2::PBKDF2_HMAC_SHA256;

/// Number of PBKDF2 iterations
const PBKDF2_ITERATIONS: u32 = 100_000;

/// Length of the derived password hash
const CREDENTIAL_LEN: usize = aws_lc_rs::digest::SHA256_OUTPUT_LEN;

/// Length of the random salt
const SALT_LEN: usize = 32;

/// Password credential type
pub type Credential = [u8; CREDENTIAL_LEN];

/// Generate a cryptographically secure random salt
pub fn generate_salt() -> Result<String, AuthError> {
    let mut salt_bytes = [0u8; SALT_LEN];

    // Generate random bytes using aws-lc-rs
    use aws_lc_rs::rand::SecureRandom;
    let rng = aws_lc_rs::rand::SystemRandom::new();
    rng.fill(&mut salt_bytes).map_err(|e| {
        AuthError::RandomGeneration(format!("Failed to generate random salt: {:?}", e))
    })?;

    // Encode as base64 for storage
    Ok(general_purpose::STANDARD.encode(salt_bytes))
}

// ============================================================================
// SHA-256 hashing — for high-entropy secrets (API keys, session tokens)
// ============================================================================

/// Hash a high-entropy secret using SHA-256.
///
/// Unlike PBKDF2 (which is designed for low-entropy passwords), a single SHA-256
/// pass is appropriate for random secrets with >= 128 bits of entropy. This is
/// ~100,000x faster than PBKDF2 with 100k iterations, improving API key
/// verification latency from ~100ms to ~1μs.
///
/// Returns a JSON string with `algorithm: "sha256"` for the `key_data` field.
pub fn hash_secret_sha256(secret: &str) -> Result<String, AuthError> {
    let hash = digest::digest(&digest::SHA256, secret.as_bytes());
    let auth_data = json!({
        "algorithm": "sha256",
        "hash": general_purpose::STANDARD.encode(hash.as_ref())
    });
    Ok(serde_json::to_string(&auth_data)?)
}

/// Verify a secret against a SHA-256 hash (constant-time comparison).
fn verify_secret_sha256(secret: &str, stored_hash_b64: &str) -> Result<bool, AuthError> {
    let stored_hash = general_purpose::STANDARD
        .decode(stored_hash_b64)
        .map_err(|_| AuthError::InvalidPasswordFormat)?;
    let computed = digest::digest(&digest::SHA256, secret.as_bytes());
    // Constant-time comparison to prevent timing attacks
    Ok(aws_lc_rs::constant_time::verify_slices_are_equal(computed.as_ref(), &stored_hash).is_ok())
}

// ============================================================================
// PBKDF2 hashing — for passwords and legacy API keys
// ============================================================================

/// Hash a password using PBKDF2 with a random salt
pub fn hash_password_with_random_salt(password: &str) -> Result<String, AuthError> {
    let salt = generate_salt()?;
    hash_password_with_salt(password, &salt)
}

/// Hash a password using PBKDF2 with the given salt
pub fn hash_password_with_salt(password: &str, salt: &str) -> Result<String, AuthError> {
    let iterations = NonZeroU32::new(PBKDF2_ITERATIONS)
        .ok_or_else(|| AuthError::PasswordHashing("Invalid iteration count".to_string()))?;

    let mut credential: Credential = [0u8; CREDENTIAL_LEN];

    pbkdf2::derive(
        PBKDF2_ALG,
        iterations,
        salt.as_bytes(),
        password.as_bytes(),
        &mut credential,
    );

    // Create auth_data JSON with salt and hash
    let auth_data = json!({
        "algorithm": "pbkdf2-hmac-sha256",
        "iterations": PBKDF2_ITERATIONS,
        "salt": salt,
        "hash": general_purpose::STANDARD.encode(credential)
    });

    // Use compact JSON serialization to avoid line breaks
    Ok(serde_json::to_string(&auth_data)?)
}

/// Hash a password using PBKDF2 with the given salt (backwards compatibility)
#[deprecated(note = "Use hash_password_with_random_salt() or hash_password_with_salt() instead")]
pub fn hash_password(password: &str, salt: &str) -> Result<String, AuthError> {
    hash_password_with_salt(password, salt)
}

/// Verify a secret/password against a stored credential.
///
/// Dispatches to the appropriate verification algorithm based on the `algorithm`
/// field in the stored auth data:
/// - `"sha256"` — single SHA-256 pass (for high-entropy API key secrets)
/// - `"pbkdf2-hmac-sha256"` or absent — PBKDF2 with 100k iterations (legacy)
pub fn verify_password(password: &str, stored_auth_data: &str) -> Result<bool, AuthError> {
    // Parse the stored auth data
    let auth_data: serde_json::Value = serde_json::from_str(stored_auth_data)?;

    let algorithm = auth_data["algorithm"]
        .as_str()
        .unwrap_or("pbkdf2-hmac-sha256");

    match algorithm {
        "sha256" => {
            let stored_hash = auth_data["hash"]
                .as_str()
                .ok_or(AuthError::InvalidPasswordFormat)?;
            verify_secret_sha256(password, stored_hash)
        }
        _ => {
            let salt = auth_data["salt"]
                .as_str()
                .ok_or(AuthError::InvalidPasswordFormat)?;

            let stored_hash = auth_data["hash"]
                .as_str()
                .ok_or(AuthError::InvalidPasswordFormat)?;

            let iterations = auth_data["iterations"]
                .as_u64()
                .ok_or(AuthError::InvalidPasswordFormat)? as u32;

            let iterations = NonZeroU32::new(iterations).ok_or(AuthError::InvalidPasswordFormat)?;

            // Decode the stored hash
            let stored_credential = general_purpose::STANDARD
                .decode(stored_hash)
                .map_err(|_| AuthError::InvalidPasswordFormat)?;

            // Verify the password
            match pbkdf2::verify(
                PBKDF2_ALG,
                iterations,
                salt.as_bytes(),
                password.as_bytes(),
                &stored_credential,
            ) {
                Ok(_) => Ok(true),
                Err(_) => Ok(false),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_password_hashing_and_verification() {
        let password = "hotdev";
        let salt = "hotdev-salt-dev";

        // Hash the password
        let auth_data = hash_password_with_salt(password, salt).unwrap();

        // Verify correct password
        assert!(verify_password(password, &auth_data).unwrap());

        // Verify incorrect password
        assert!(!verify_password("wrongpassword", &auth_data).unwrap());
    }

    #[test]
    fn test_consistent_hashing() {
        let password = "hotdev";
        let salt = "hotdev-salt-dev";

        let auth_data1 = hash_password_with_salt(password, salt).unwrap();
        let auth_data2 = hash_password_with_salt(password, salt).unwrap();

        // Should produce the same hash with the same salt
        assert_eq!(auth_data1, auth_data2);
    }

    #[test]
    fn test_random_salt_generation() {
        let password = "hotdev";

        let auth_data1 = hash_password_with_random_salt(password).unwrap();
        let auth_data2 = hash_password_with_random_salt(password).unwrap();

        // Should produce different hashes with different salts
        assert_ne!(auth_data1, auth_data2);

        // But both should verify correctly
        assert!(verify_password(password, &auth_data1).unwrap());
        assert!(verify_password(password, &auth_data2).unwrap());
    }

    #[test]
    fn test_sha256_hash_and_verify() {
        let secret = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";

        let auth_data = hash_secret_sha256(secret).unwrap();

        // Verify correct secret
        assert!(verify_password(secret, &auth_data).unwrap());

        // Verify incorrect secret
        assert!(!verify_password("wrong_secret", &auth_data).unwrap());

        // Verify algorithm field
        let parsed: serde_json::Value = serde_json::from_str(&auth_data).unwrap();
        assert_eq!(parsed["algorithm"], "sha256");
        // No salt or iterations for SHA-256
        assert!(parsed.get("salt").is_none());
        assert!(parsed.get("iterations").is_none());
    }

    #[test]
    fn test_pbkdf2_backward_compatibility() {
        // Generate a PBKDF2 hash (legacy path) and verify it still works
        let password = "hotdev-legacy-key";
        let auth_data = hash_password_with_random_salt(password).unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&auth_data).unwrap();
        assert_eq!(parsed["algorithm"], "pbkdf2-hmac-sha256");

        // PBKDF2 hashes must still verify through the dispatch
        assert!(verify_password(password, &auth_data).unwrap());
        assert!(!verify_password("wrong", &auth_data).unwrap());
    }

    #[test]
    fn test_salt_generation() {
        let salt1 = generate_salt().unwrap();
        let salt2 = generate_salt().unwrap();

        // Should generate different salts
        assert_ne!(salt1, salt2);

        // Should be base64 encoded
        assert!(general_purpose::STANDARD.decode(&salt1).is_ok());
        assert!(general_purpose::STANDARD.decode(&salt2).is_ok());
    }
}
