use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use rand::{RngCore, rngs::OsRng};
use std::sync::Arc;
use tokio::sync::Semaphore;

const MAX_TOKENS: usize = 16;
const AUTH_JOBS: usize = 4;

#[derive(Clone)]
pub struct TokenStore {
    hashes: Vec<String>,
    dummy_hash: String,
    admission: Arc<Semaphore>,
}

impl std::fmt::Debug for TokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenStore")
            .field("token_count", &self.hashes.len())
            .finish()
    }
}

pub struct CreatedToken {
    pub plaintext: String,
}

impl Default for TokenStore {
    fn default() -> Self {
        Self {
            hashes: Vec::new(),
            dummy_hash: hash("invalid-token").expect("static token hashes"),
            admission: Arc::new(Semaphore::new(AUTH_JOBS)),
        }
    }
}

impl TokenStore {
    pub fn from_phc_hashes(hashes: Vec<String>) -> Result<Self, argon2::password_hash::Error> {
        if hashes.len() > MAX_TOKENS {
            return Err(argon2::password_hash::Error::ParamsMaxExceeded);
        }
        for encoded in &hashes {
            validate_phc(encoded)?;
        }
        Ok(Self {
            hashes,
            dummy_hash: hash("invalid-token")?,
            admission: Arc::new(Semaphore::new(AUTH_JOBS)),
        })
    }

    #[must_use]
    pub fn phc_hashes(&self) -> &[String] {
        &self.hashes
    }

    pub fn create(&mut self) -> Result<CreatedToken, argon2::password_hash::Error> {
        if self.hashes.len() >= MAX_TOKENS {
            return Err(argon2::password_hash::Error::ParamsMaxExceeded);
        }
        let mut bytes = [0_u8; 24];
        OsRng.fill_bytes(&mut bytes);
        let plaintext = format!("ml_{}", hex(&bytes));
        self.hashes.push(hash(&plaintext)?);
        Ok(CreatedToken { plaintext })
    }

    pub async fn verify(&self, candidate: &str) -> bool {
        let Ok(_permit) = self.admission.clone().acquire_owned().await else {
            return false;
        };
        let hashes = if self.hashes.is_empty() {
            vec![self.dummy_hash.clone()]
        } else {
            self.hashes.clone()
        };
        let candidate = candidate.to_owned();
        tokio::task::spawn_blocking(move || {
            hashes.iter().fold(0_u8, |valid, encoded| {
                let verified = PasswordHash::new(encoded).ok().is_some_and(|parsed| {
                    Argon2::default()
                        .verify_password(candidate.as_bytes(), &parsed)
                        .is_ok()
                });
                valid | u8::from(verified)
            }) == 1
        })
        .await
        .unwrap_or(false)
    }
}

fn validate_phc(encoded: &str) -> Result<(), argon2::password_hash::Error> {
    if encoded.len() > 256 {
        return Err(argon2::password_hash::Error::PhcStringField);
    }
    let parsed = PasswordHash::new(encoded)?;
    if parsed.algorithm.as_str() != "argon2id" {
        return Err(argon2::password_hash::Error::Algorithm);
    }
    if parsed.version != Some(19) {
        return Err(argon2::password_hash::Error::Version);
    }
    let m = parsed
        .params
        .get_decimal("m")
        .ok_or(argon2::password_hash::Error::ParamNameInvalid)?;
    let t = parsed
        .params
        .get_decimal("t")
        .ok_or(argon2::password_hash::Error::ParamNameInvalid)?;
    let p = parsed
        .params
        .get_decimal("p")
        .ok_or(argon2::password_hash::Error::ParamNameInvalid)?;
    if !(8192..=65536).contains(&m) || !(1..=10).contains(&t) || !(1..=8).contains(&p) {
        return Err(argon2::password_hash::Error::ParamsMaxExceeded);
    }
    let salt = parsed
        .salt
        .ok_or(argon2::password_hash::Error::PhcStringField)?;
    let output = parsed
        .hash
        .ok_or(argon2::password_hash::Error::PhcStringField)?;
    if !(8..=64).contains(&salt.len()) || !(16..=64).contains(&output.len()) {
        return Err(argon2::password_hash::Error::PhcStringField);
    }
    Ok(())
}

fn hash(value: &str) -> Result<String, argon2::password_hash::Error> {
    Ok(Argon2::default()
        .hash_password(value.as_bytes(), &SaltString::generate(&mut OsRng))?
        .to_string())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 15) as usize] as char,
            ]
        })
        .collect()
}
