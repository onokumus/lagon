use aes_gcm::{aead::Aead, KeyInit, Nonce};
use anyhow::{anyhow, Result};

use crate::{Aes256Gcm, Algorithm};

pub fn decrypt(algorithm: Algorithm, key_value: Vec<u8>, data: Vec<u8>) -> Result<Vec<u8>> {
    match algorithm {
        Algorithm::AesGcm(iv) => {
            let cipher = Aes256Gcm::new_from_slice(&key_value)?;
            let nonce = Nonce::from_slice(&iv);

            match cipher.decrypt(nonce, data.as_ref()) {
                Ok(result) => Ok(result),
                Err(_) => Err(anyhow!("Decryption failed")),
            }
        }
        _ => Err(anyhow!("Algorithm not supported")),
    }
}
