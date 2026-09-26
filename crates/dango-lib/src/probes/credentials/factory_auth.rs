use std::fs;

use base64::prelude::*;

use super::gcm::decrypt_factory_payload;
use super::get_home_dir;
use super::keyring::load_factory_key;
use super::FactoryAuth;

/// Decrypt Factory credentials from ~/.factory/auth.v2.loginkeychain or ~/.factory/auth.v2.keyring
pub fn load_factory_credentials() -> Result<FactoryAuth, String> {
    let home = get_home_dir().ok_or("Cannot locate user home directory")?;
    let candidates = [
        home.join(".factory").join("auth.v2.loginkeychain"),
        home.join(".factory").join("auth.v2.keyring"),
    ];

    let path = candidates
        .iter()
        .find(|p| p.exists())
        .ok_or_else(|| "Factory auth file not found in ~/.factory/".to_string())?;

    let file_content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read Factory keyring file: {}", e))?;

    let parts: Vec<&str> = file_content.trim().split(':').collect();
    if parts.len() != 3 {
        return Err("Invalid Factory credential format; expected iv:authTag:ciphertext".into());
    }

    let iv = BASE64_STANDARD
        .decode(parts[0].as_bytes())
        .map_err(|e| format!("Invalid base64 iv: {}", e))?;
    let tag = BASE64_STANDARD
        .decode(parts[1].as_bytes())
        .map_err(|e| format!("Invalid base64 tag: {}", e))?;
    let ciphertext = BASE64_STANDARD
        .decode(parts[2].as_bytes())
        .map_err(|e| format!("Invalid base64 ciphertext: {}", e))?;

    let key_bytes = load_factory_key()?;
    if key_bytes.len() != 32 {
        return Err("Factory AES key must be 32 bytes".into());
    }

    let plaintext = decrypt_factory_payload(&key_bytes, &iv, &tag, &ciphertext)?;

    let auth: FactoryAuth = serde_json::from_slice(&plaintext)
        .map_err(|e| format!("Failed to parse decrypted Factory credentials JSON: {}", e))?;

    Ok(auth)
}
