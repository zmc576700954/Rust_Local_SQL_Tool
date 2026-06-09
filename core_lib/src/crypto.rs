//! ConfigCrypto — 敏感配置字段静息加密
//!
//! 使用 AES-256-GCM 对 api_key、token_pool、db_url 中的密码等敏感字段加密，
//! 防止 config.json 在磁盘上明文暴露。
//!
//! 密钥管理策略：
//! - 密钥文件 ~/.local-ai-sql/.crypto_key（32 bytes）
//! - 首次运行自动生成随机密钥
//! - 加密后字段存储为 "enc::<base64_nonce+ciphertext>" 前缀标识
//! - 未加密的旧值自动兼容读取（迁移友好）

use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use aes_gcm::aead::rand_core::RngCore;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::config::ConfigError;

// ── 常量 ────────────────────────────────────────────────────

/// 加密值前缀标识 — 遇到此前缀的字符串会被解密
const ENC_PREFIX: &str = "enc::";

/// AES-256-GCM nonce 长度（12 bytes）
const NONCE_LEN: usize = 12;

// ── 密钥管理 ────────────────────────────────────────────────

/// 获取密钥文件路径
fn crypto_key_path() -> Result<PathBuf, ConfigError> {
    let home = dirs::home_dir().ok_or(ConfigError::NoHomeDir)?;
    Ok(home.join(".local-ai-sql").join(".crypto_key"))
}

/// 读取或生成加密密钥
///
/// 如果密钥文件不存在，自动生成 32 bytes 随机密钥并保存。
/// 密钥文件权限在 Unix 上设为 0600（仅当前用户可读写）。
pub async fn ensure_crypto_key() -> Result<[u8; 32], ConfigError> {
    let path = crypto_key_path()?;

    if path.exists() {
        let key_bytes = tokio::fs::read(&path).await?;
        if key_bytes.len() != 32 {
            return Err(ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Crypto key file must be exactly 32 bytes",
            )));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        Ok(key)
    } else {
        // 生成随机密钥（32 bytes）
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        tokio::fs::write(&path, &key).await?;

        // Unix-only: chmod 600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }

        Ok(key)
    }
}

/// 从给定字符串派生 32 bytes 密钥（fallback：无密钥文件时使用）
/// 使用 SHA-256 哈希确保固定长度
pub fn derive_key_from_password(password: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hasher.finalize().into()
}

// ── 加密/解密 ────────────────────────────────────────────────

/// 加密一个明文字符串，返回 "enc::<base64(nonce+ciphertext)>" 格式
///
/// 如果输入已经是 "enc::..." 格式，直接返回原值（避免重复加密）
pub fn encrypt_value(plaintext: &str, key: &[u8; 32]) -> Result<String, ConfigError> {
    // 已经是加密格式，不重复加密
    if plaintext.starts_with(ENC_PREFIX) {
        return Ok(plaintext.to_string());
    }

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| ConfigError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))?;

    // 生成随机 nonce（12 bytes）
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| ConfigError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))?;

    // nonce + ciphertext 合并后 base64 编码
    let combined: Vec<u8> = nonce_bytes.iter().chain(ciphertext.iter()).copied().collect();
    Ok(format!("{ENC_PREFIX}{}", BASE64.encode(&combined)))
}

/// 解密一个 "enc::..." 格式的加密字符串，返回明文
///
/// 非加密格式字符串直接返回原值（向后兼容）
pub fn decrypt_value(enc_text: &str, key: &[u8; 32]) -> Result<String, ConfigError> {
    if !enc_text.starts_with(ENC_PREFIX) {
        // 非加密格式，直接返回（兼容旧 config.json）
        return Ok(enc_text.to_string());
    }

    let b64_part = &enc_text[ENC_PREFIX.len()..];
    let combined = BASE64.decode(b64_part)
        .map_err(|e| ConfigError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))?;

    if combined.len() < NONCE_LEN {
        return Err(ConfigError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Encrypted value too short — missing nonce",
        )));
    }

    let (nonce_bytes, ciphertext) = combined.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| ConfigError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))?;

    let plaintext_bytes = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| ConfigError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Decryption failed: {}", e))))?;

    String::from_utf8(plaintext_bytes)
        .map_err(|e| ConfigError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))
}

/// 判断一个值是否已加密
pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(ENC_PREFIX)
}

// ── 批量加密/解密 AppConfig ──────────────────────────────────

/// 加密 AppConfig 中的敏感字段（保存前调用）
///
/// 逐个加密 api_key、token_pool、db_url 密码、ai_profiles[].api_key 等字段
/// 已加密字段（以 "enc::" 开头）不会重复加密
pub fn encrypt_config_secrets(
    config_json: &mut serde_json::Value,
    key: &[u8; 32],
) -> Result<(), ConfigError> {
    if let serde_json::Value::Object(map) = config_json {
        // api_key
        if let Some(v) = map.get_mut("api_key") {
            encrypt_json_string_value(v, key)?;
        }

        // token_pool
        if let Some(serde_json::Value::Array(tokens)) = map.get_mut("token_pool") {
            for token in tokens.iter_mut() {
                if let serde_json::Value::String(s) = token {
                    if !is_encrypted(s) {
                        *s = encrypt_value(s, key)?;
                    }
                }
            }
        }

        // db_url — 加密 URL 中的密码部分
        if let Some(v) = map.get_mut("db_url") {
            encrypt_json_string_value(v, key)?;
        }

        // ai_profiles
        if let Some(serde_json::Value::Array(profiles)) = map.get_mut("ai_profiles") {
            for profile in profiles.iter_mut() {
                if let serde_json::Value::Object(p) = profile {
                    if let Some(v) = p.get_mut("api_key") {
                        encrypt_json_string_value(v, key)?;
                    }
                    if let Some(serde_json::Value::Object(pool)) = p.get_mut("pool") {
                        if let Some(serde_json::Value::Array(tokens)) = pool.get_mut("tokens") {
                            for token in tokens.iter_mut() {
                                if let serde_json::Value::String(s) = token {
                                    if !is_encrypted(s) {
                                        *s = encrypt_value(s, key)?;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // db_connections
        if let Some(serde_json::Value::Array(conns)) = map.get_mut("db_connections") {
            for conn in conns.iter_mut() {
                if let serde_json::Value::Object(c) = conn {
                    if let Some(v) = c.get_mut("url") {
                        encrypt_json_string_value(v, key)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// 解密 AppConfig 中的敏感字段（加载后调用）
///
/// 逐个解密以 "enc::" 开头的字段
/// 非加密字段直接保留
pub fn decrypt_config_secrets(
    config_json: &mut serde_json::Value,
    key: &[u8; 32],
) -> Result<(), ConfigError> {
    if let serde_json::Value::Object(map) = config_json {
        // api_key
        if let Some(v) = map.get_mut("api_key") {
            decrypt_json_string_value(v, key)?;
        }

        // token_pool
        if let Some(serde_json::Value::Array(tokens)) = map.get_mut("token_pool") {
            for token in tokens.iter_mut() {
                if let serde_json::Value::String(s) = token {
                    *s = decrypt_value(s, key)?;
                }
            }
        }

        // db_url
        if let Some(v) = map.get_mut("db_url") {
            decrypt_json_string_value(v, key)?;
        }

        // ai_profiles
        if let Some(serde_json::Value::Array(profiles)) = map.get_mut("ai_profiles") {
            for profile in profiles.iter_mut() {
                if let serde_json::Value::Object(p) = profile {
                    if let Some(v) = p.get_mut("api_key") {
                        decrypt_json_string_value(v, key)?;
                    }
                    if let Some(serde_json::Value::Object(pool)) = p.get_mut("pool") {
                        if let Some(serde_json::Value::Array(tokens)) = pool.get_mut("tokens") {
                            for token in tokens.iter_mut() {
                                if let serde_json::Value::String(s) = token {
                                    *s = decrypt_value(s, key)?;
                                }
                            }
                        }
                    }
                }
            }
        }

        // db_connections
        if let Some(serde_json::Value::Array(conns)) = map.get_mut("db_connections") {
            for conn in conns.iter_mut() {
                if let serde_json::Value::Object(c) = conn {
                    if let Some(v) = c.get_mut("url") {
                        decrypt_json_string_value(v, key)?;
                    }
                }
            }
        }
    }
    Ok(())
}

// ── JSON 辅助 ────────────────────────────────────────────────

fn encrypt_json_string_value(v: &mut serde_json::Value, key: &[u8; 32]) -> Result<(), ConfigError> {
    if let serde_json::Value::String(s) = v {
        if !s.is_empty() && !is_encrypted(s) {
            *s = encrypt_value(s, key)?;
        }
    }
    Ok(())
}

fn decrypt_json_string_value(v: &mut serde_json::Value, key: &[u8; 32]) -> Result<(), ConfigError> {
    if let serde_json::Value::String(s) = v {
        if is_encrypted(s) {
            *s = decrypt_value(s, key)?;
        }
    }
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::rand_core::RngCore;

    fn random_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        key
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = random_key();
        let plaintext = "sk-1234567890abcdef";
        let encrypted = encrypt_value(plaintext, &key).unwrap();
        assert!(encrypted.starts_with(ENC_PREFIX));
        assert_ne!(encrypted, plaintext);

        let decrypted = decrypt_value(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_plain_value_passthrough() {
        let key = random_key();
        let plain = "not-encrypted";
        let result = decrypt_value(plain, &key).unwrap();
        assert_eq!(result, plain);
    }

    #[test]
    fn double_encrypt_no_reencrypt() {
        let key = random_key();
        let plaintext = "my-api-key";
        let encrypted = encrypt_value(plaintext, &key).unwrap();
        // 第二次加密已加密值应不改变
        let re_encrypted = encrypt_value(&encrypted, &key).unwrap();
        assert_eq!(re_encrypted, encrypted);
    }

    #[test]
    fn is_encrypted_check() {
        assert!(is_encrypted("enc::dGVzdA=="));
        assert!(!is_encrypted("sk-plain-key"));
        assert!(!is_encrypted(""));
    }

    #[test]
    fn derive_key_deterministic() {
        let key1 = derive_key_from_password("my-password");
        let key2 = derive_key_from_password("my-password");
        assert_eq!(key1, key2);
    }

    #[test]
    fn encrypt_decrypt_url() {
        let key = random_key();
        let url = "mysql://user:secret@127.0.0.1:3306/mydb";
        let encrypted = encrypt_value(url, &key).unwrap();
        let decrypted = decrypt_value(&encrypted, &key).unwrap();
        assert_eq!(decrypted, url);
    }

    #[test]
    fn config_json_batch_encrypt_decrypt() {
        let key = random_key();
        let mut config = serde_json::json!({
            "api_key": "sk-test-123",
            "token_pool": ["token-a", "token-b"],
            "db_url": "mysql://root:password@localhost/db",
            "ai_profiles": [
                {
                    "api_key": "sk-profile-key",
                    "pool": { "tokens": ["p-token-1"] }
                }
            ],
            "db_connections": [
                { "url": "postgres://user:pass@host/app" }
            ],
            "active_db_id": "default"
        });

        encrypt_config_secrets(&mut config, &key).unwrap();

        // 所有敏感字段应已加密
        let api_key = config["api_key"].as_str().unwrap();
        assert!(api_key.starts_with(ENC_PREFIX));

        // 非敏感字段应不变
        assert_eq!(config["active_db_id"].as_str().unwrap(), "default");

        // 解密后应恢复原值
        decrypt_config_secrets(&mut config, &key).unwrap();
        assert_eq!(config["api_key"].as_str().unwrap(), "sk-test-123");
        assert_eq!(config["db_url"].as_str().unwrap(), "mysql://root:password@localhost/db");
    }
}