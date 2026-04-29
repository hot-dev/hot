// Context encryption module for per-org key derivation
// Master key: HOT_ENCRYPTION_KEY environment variable
// Org-specific key: HKDF(master_key, org_id)

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hkdf::Hkdf;
use sha2::Sha256;
use uuid::Uuid;

pub struct ContextEncryption {
    master_key: [u8; 32],
}

impl ContextEncryption {
    /// Load master key from environment variable HOT_ENCRYPTION_KEY
    pub fn from_env() -> Result<Self, EncryptionError> {
        let key_hex =
            std::env::var("HOT_ENCRYPTION_KEY").map_err(|_| EncryptionError::KeyNotConfigured)?;

        let master_key = hex::decode(&key_hex).map_err(|_| EncryptionError::InvalidKeyFormat)?;

        if master_key.len() != 32 {
            return Err(EncryptionError::InvalidKeyLength);
        }

        let mut key_bytes = [0u8; 32];
        key_bytes.copy_from_slice(&master_key);

        Ok(Self {
            master_key: key_bytes,
        })
    }

    /// Load encryption key from environment or generate for local development
    /// If profile is "local-dev" and HOT_ENCRYPTION_KEY is not set, auto-generate a key
    pub fn from_env_or_generate_for_dev(profile: &str) -> Result<Self, EncryptionError> {
        // Try environment variable first
        match Self::from_env() {
            Ok(encryption) => Ok(encryption),
            Err(EncryptionError::KeyNotConfigured) if profile == "local-dev" => {
                // Auto-generate for local dev
                Self::get_or_create_dev_key()
            }
            Err(e) => Err(e),
        }
    }

    /// Load encryption key from environment or existing dev key file
    /// Unlike from_env_or_generate_for_dev, this does NOT create a new dev key
    /// Use this for validation where we want to use an existing key but not generate one
    pub fn from_env_or_existing_dev_key() -> Result<Self, EncryptionError> {
        // Try environment variable first
        match Self::from_env() {
            Ok(encryption) => Ok(encryption),
            Err(EncryptionError::KeyNotConfigured) => {
                // Try loading existing dev key (but don't create one)
                Self::load_existing_dev_key()
            }
            Err(e) => Err(e),
        }
    }

    /// Load an existing development key without creating one
    fn load_existing_dev_key() -> Result<Self, EncryptionError> {
        let key_path = std::path::Path::new("hot/dev.key");

        if key_path.exists() {
            let key_hex = std::fs::read_to_string(key_path)
                .map_err(|e| EncryptionError::IoError(format!("Failed to read dev key: {}", e)))?;

            let master_key =
                hex::decode(key_hex.trim()).map_err(|_| EncryptionError::InvalidKeyFormat)?;

            if master_key.len() != 32 {
                return Err(EncryptionError::InvalidKeyLength);
            }

            let mut key_bytes = [0u8; 32];
            key_bytes.copy_from_slice(&master_key);

            Ok(Self {
                master_key: key_bytes,
            })
        } else {
            Err(EncryptionError::KeyNotConfigured)
        }
    }

    /// Get or create a development encryption key stored in hot/dev.key
    fn get_or_create_dev_key() -> Result<Self, EncryptionError> {
        let key_path = std::path::Path::new("hot/dev.key");

        if key_path.exists() {
            // Load existing dev key
            let key_hex = std::fs::read_to_string(key_path)
                .map_err(|e| EncryptionError::IoError(format!("Failed to read dev key: {}", e)))?;

            let master_key =
                hex::decode(key_hex.trim()).map_err(|_| EncryptionError::InvalidKeyFormat)?;

            if master_key.len() != 32 {
                return Err(EncryptionError::InvalidKeyLength);
            }

            let mut key_bytes = [0u8; 32];
            key_bytes.copy_from_slice(&master_key);

            Ok(Self {
                master_key: key_bytes,
            })
        } else {
            // Generate new dev key
            eprintln!("⚠️  Generating local development encryption key");
            eprintln!("⚠️  Stored in hot/dev.key (DO NOT use in production)");
            eprintln!("⚠️  Add hot/dev.key to .gitignore if sharing code");

            let key = Self::generate_key();

            // Create hot directory if it doesn't exist
            if let Some(parent) = key_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    EncryptionError::IoError(format!("Failed to create hot directory: {}", e))
                })?;
            }

            std::fs::write(key_path, &key)
                .map_err(|e| EncryptionError::IoError(format!("Failed to write dev key: {}", e)))?;

            // Parse the key we just generated
            let master_key =
                hex::decode(key.trim()).map_err(|_| EncryptionError::InvalidKeyFormat)?;

            let mut key_bytes = [0u8; 32];
            key_bytes.copy_from_slice(&master_key);

            Ok(Self {
                master_key: key_bytes,
            })
        }
    }

    /// Derive org-specific key from master key and org_id using HKDF-SHA256
    fn derive_org_key(&self, org_id: &Uuid) -> [u8; 32] {
        let hkdf = Hkdf::<Sha256>::new(None, &self.master_key);
        let info = format!("hot-context-v1-org-{}", org_id);

        let mut derived_key = [0u8; 32];
        hkdf.expand(info.as_bytes(), &mut derived_key)
            .expect("HKDF expand should never fail with valid length");

        derived_key
    }

    /// Encrypt plaintext using org-specific derived key
    /// Format: base64(nonce || ciphertext || auth_tag)
    pub fn encrypt(&self, plaintext: &str, org_id: &Uuid) -> Result<String, EncryptionError> {
        // Derive org-specific key
        let org_key = self.derive_org_key(org_id);
        let cipher = Aes256Gcm::new(&org_key.into());

        // Generate random nonce (96 bits for GCM)
        let mut nonce_bytes = [0u8; 12];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from(nonce_bytes);

        // Encrypt
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| EncryptionError::EncryptionFailed)?;

        // Format: nonce || ciphertext (includes auth tag)
        let mut result = Vec::with_capacity(12 + ciphertext.len());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&ciphertext);

        Ok(STANDARD.encode(&result))
    }

    /// Decrypt ciphertext using org-specific derived key
    pub fn decrypt(&self, ciphertext: &str, org_id: &Uuid) -> Result<String, EncryptionError> {
        // Derive org-specific key
        let org_key = self.derive_org_key(org_id);
        let cipher = Aes256Gcm::new(&org_key.into());

        // Decode base64
        let data = STANDARD
            .decode(ciphertext)
            .map_err(|_| EncryptionError::InvalidFormat)?;

        if data.len() < 12 {
            return Err(EncryptionError::InvalidFormat);
        }

        // Split nonce and ciphertext
        let (nonce_bytes, encrypted_data) = data.split_at(12);
        let nonce_array: [u8; 12] = nonce_bytes
            .try_into()
            .map_err(|_| EncryptionError::InvalidFormat)?;
        let nonce = Nonce::from(nonce_array);

        // Decrypt
        let plaintext = cipher
            .decrypt(&nonce, encrypted_data)
            .map_err(|_| EncryptionError::DecryptionFailed)?;

        String::from_utf8(plaintext).map_err(|_| EncryptionError::InvalidUtf8)
    }

    /// Generate a new random master key (for CLI command)
    pub fn generate_key() -> String {
        use rand::RngCore;
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        hex::encode(key)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EncryptionError {
    #[error("Encryption key not configured (set HOT_ENCRYPTION_KEY environment variable)")]
    KeyNotConfigured,
    #[error("Invalid key format (must be 64-character hex string)")]
    InvalidKeyFormat,
    #[error("Invalid key length (must be 32 bytes)")]
    InvalidKeyLength,
    #[error("Encryption failed")]
    EncryptionFailed,
    #[error("Decryption failed (wrong key or corrupted data)")]
    DecryptionFailed,
    #[error("Invalid encrypted data format")]
    InvalidFormat,
    #[error("Invalid UTF-8 in decrypted data")]
    InvalidUtf8,
    #[error("IO error: {0}")]
    IoError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_derivation_deterministic() {
        let master_key = [0u8; 32];
        let encryption = ContextEncryption { master_key };

        let org_id = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();

        let key1 = encryption.derive_org_key(&org_id);
        let key2 = encryption.derive_org_key(&org_id);

        assert_eq!(key1, key2, "Derived keys should be deterministic");
    }

    #[test]
    fn test_key_derivation_unique_per_org() {
        let master_key = [0u8; 32];
        let encryption = ContextEncryption { master_key };

        let org_id1 = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let org_id2 = Uuid::parse_str("fedcba98-7654-3210-fedc-ba9876543210").unwrap();

        let key1 = encryption.derive_org_key(&org_id1);
        let key2 = encryption.derive_org_key(&org_id2);

        assert_ne!(
            key1, key2,
            "Different orgs should have different derived keys"
        );
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let master_key = [1u8; 32];
        let encryption = ContextEncryption { master_key };

        let org_id = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let plaintext = "Hello, World!";

        let encrypted = encryption.encrypt(plaintext, &org_id).unwrap();
        let decrypted = encryption.decrypt(&encrypted, &org_id).unwrap();

        assert_eq!(plaintext, decrypted);
    }

    #[test]
    fn test_decrypt_with_wrong_org_fails() {
        let master_key = [1u8; 32];
        let encryption = ContextEncryption { master_key };

        let org_id1 = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let org_id2 = Uuid::parse_str("fedcba98-7654-3210-fedc-ba9876543210").unwrap();
        let plaintext = "Secret data";

        let encrypted = encryption.encrypt(plaintext, &org_id1).unwrap();
        let result = encryption.decrypt(&encrypted, &org_id2);

        assert!(result.is_err(), "Decryption with wrong org ID should fail");
    }

    #[test]
    fn test_generate_key_format() {
        let key = ContextEncryption::generate_key();
        assert_eq!(key.len(), 64, "Generated key should be 64 hex characters");
        assert!(
            hex::decode(&key).is_ok(),
            "Generated key should be valid hex"
        );
    }
}
