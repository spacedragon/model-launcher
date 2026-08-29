use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use rand::{RngCore, rngs::OsRng};

#[derive(Clone)]
pub struct TokenStore {
    hashes: Vec<String>,
    dummy_hash: String,
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
        }
    }
}

impl TokenStore {
    pub fn from_phc_hashes(hashes: Vec<String>) -> Result<Self, argon2::password_hash::Error> {
        for encoded in &hashes {
            PasswordHash::new(encoded)?;
        }
        Ok(Self {
            hashes,
            dummy_hash: hash("invalid-token")?,
        })
    }

    #[must_use]
    pub fn phc_hashes(&self) -> &[String] {
        &self.hashes
    }

    pub fn create(&mut self) -> Result<CreatedToken, argon2::password_hash::Error> {
        let mut bytes = [0_u8; 24];
        OsRng.fill_bytes(&mut bytes);
        let plaintext = format!("ml_{}", hex(&bytes));
        self.hashes.push(hash(&plaintext)?);
        Ok(CreatedToken { plaintext })
    }

    #[must_use]
    pub fn verify(&self, candidate: &str) -> bool {
        let candidates = if self.hashes.is_empty() {
            std::slice::from_ref(&self.dummy_hash)
        } else {
            &self.hashes
        };
        let mut valid = 0_u8;
        for encoded in candidates {
            let verified = PasswordHash::new(encoded).ok().is_some_and(|parsed| {
                Argon2::default()
                    .verify_password(candidate.as_bytes(), &parsed)
                    .is_ok()
            });
            valid |= u8::from(verified);
        }
        valid == 1
    }
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
