// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! 设备指纹生成器
//!

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

/// 兜底 machineId 缓存（按凭据 id 分桶，进程生命周期内稳定）
///
/// 未分配 id 的新账号不共享缓存；调用方应在首次使用前固定并持久化该值。
static FALLBACK_MACHINE_IDS: OnceLock<Mutex<HashMap<u64, String>>> = OnceLock::new();

/// 标准化 machineId 格式
///
/// 支持以下格式：
/// - 64 字符十六进制字符串（直接返回）
/// - UUID 格式（如 "2582956e-cc88-4669-b546-07adbffcb894"，移除连字符后补齐到 64 字符）
pub(crate) fn normalize_machine_id(machine_id: &str) -> Option<String> {
    let trimmed = machine_id.trim();

    // 如果已经是 64 字符，直接返回
    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(trimmed.to_string());
    }

    // 同时兼容带连字符和紧凑 UUID，拒绝连字符位置损坏的输入。
    if matches!(trimmed.len(), 32 | 36) && Uuid::parse_str(trimmed).is_ok() {
        let without_dashes: String = trimmed.chars().filter(|c| *c != '-').collect();
        // 补齐到 64 字符（重复一次）
        return Some(format!("{}{}", without_dashes, without_dashes));
    }

    // 无法识别的格式
    None
}

/// 固定账号自己的 machineId，并返回是否需要持久化。
///
/// 保留合法账号标识；缺失或损坏时只按该账号的派生材料补全，避免多个账号继承全局值。
pub fn ensure_fixed_machine_id(credentials: &mut KiroCredentials, config: &Config) -> bool {
    let fixed_id = generate_from_credentials(credentials, config);
    if credentials.machine_id.as_deref() == Some(fixed_id.as_str()) {
        return false;
    }
    credentials.machine_id = Some(fixed_id);
    true
}

/// 优先使用合法账号标识，否则按该账号 refreshToken 派生。
/// 全局 machineId 不再作为多账号共享身份的默认值。
pub fn generate_from_credentials(credentials: &KiroCredentials, _config: &Config) -> String {
    // 如果配置了账号级 machineId，优先使用
    if let Some(ref machine_id) = credentials.machine_id
        && let Some(normalized) = normalize_machine_id(machine_id)
    {
        return normalized;
    }

    // 使用 refreshToken 生成
    if let Some(ref refresh_token) = credentials.refresh_token
        && !refresh_token.is_empty()
    {
        return sha256_hex(&format!("KotlinNativeAPI/{}", refresh_token));
    }

    // 没有有效的派生材料：走兜底流程，避免整条请求硬失败
    fallback_machine_id(credentials)
}

/// 为缺失派生材料的凭据生成兜底 machineId
///
/// - 经 `sha256("KiroFallback/<uuid>")` 派生，输出格式与正常路径一致（64 字符十六进制）
/// - 已分配 id 时在进程内缓存；尚无 id 的新账号每次产生独立值
/// - 启动、导入和刷新前通过 ensure_fixed_machine_id 固定到账号字段并持久化
fn fallback_machine_id(credentials: &KiroCredentials) -> String {
    let Some(id) = credentials.id else {
        return random_machine_id();
    };
    let cache = FALLBACK_MACHINE_IDS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock();
    if let Some(existing) = map.get(&id) {
        return existing.clone();
    }

    let derived = random_machine_id();
    tracing::warn!(
        credential_id = ?credentials.id,
        "凭据缺少派生材料（machineId/refreshToken 均不可用），使用随机兜底 machineId（进程内稳定）"
    );
    map.insert(id, derived.clone());
    derived
}

fn random_machine_id() -> String {
    sha256_hex(&format!("KiroFallback/{}", Uuid::new_v4()))
}

/// SHA256 哈希实现（返回十六进制字符串）
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_hex() {
        let result = sha256_hex("test");
        assert_eq!(result.len(), 64);
        assert_eq!(
            result,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    #[test]
    fn test_generate_ignores_shared_global_machine_id() {
        let mut credentials = KiroCredentials::default();
        credentials.refresh_token = Some("account-refresh-token".to_string());
        let mut config = Config::default();
        config.machine_id = Some("a".repeat(64));

        let result = generate_from_credentials(&credentials, &config);
        assert_eq!(result, sha256_hex("KotlinNativeAPI/account-refresh-token"));
    }

    #[test]
    fn test_generate_with_credential_machine_id_overrides_config() {
        let mut credentials = KiroCredentials::default();
        credentials.machine_id = Some("b".repeat(64));

        let mut config = Config::default();
        config.machine_id = Some("a".repeat(64));

        let result = generate_from_credentials(&credentials, &config);
        assert_eq!(result, "b".repeat(64));
    }

    #[test]
    fn test_generate_with_refresh_token() {
        let mut credentials = KiroCredentials::default();
        credentials.refresh_token = Some("test_refresh_token".to_string());
        let config = Config::default();

        let result = generate_from_credentials(&credentials, &config);
        assert_eq!(result.len(), 64);
    }

    #[test]
    fn test_generate_without_credentials_uses_fallback() {
        let credentials = KiroCredentials::default();
        let config = Config::default();

        let result = generate_from_credentials(&credentials, &config);
        assert_eq!(result.len(), 64);
        assert!(result.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_fallback_is_stable_per_credential() {
        let mut credentials = KiroCredentials::default();
        credentials.id = Some(u64::MAX - 10);
        let config = Config::default();

        let first = generate_from_credentials(&credentials, &config);
        let second = generate_from_credentials(&credentials, &config);
        assert_eq!(first, second);
    }

    #[test]
    fn test_fallback_differs_across_credentials() {
        let mut cred_a = KiroCredentials::default();
        cred_a.id = Some(u64::MAX - 20);
        let mut cred_b = KiroCredentials::default();
        cred_b.id = Some(u64::MAX - 21);
        let config = Config::default();

        let id_a = generate_from_credentials(&cred_a, &config);
        let id_b = generate_from_credentials(&cred_b, &config);
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn test_normalize_uuid_format() {
        // UUID 格式应该被转换为 64 字符
        let uuid = "2582956e-cc88-4669-b546-07adbffcb894";
        let result = normalize_machine_id(uuid);
        assert!(result.is_some());
        let normalized = result.unwrap();
        assert_eq!(normalized.len(), 64);
        // UUID 去掉连字符后重复一次
        assert_eq!(
            normalized,
            "2582956ecc884669b54607adbffcb8942582956ecc884669b54607adbffcb894"
        );
    }

    #[test]
    fn test_normalize_64_char_hex() {
        // 64 字符十六进制应该直接返回
        let hex64 = "a".repeat(64);
        let result = normalize_machine_id(&hex64);
        assert_eq!(result, Some(hex64));
    }

    #[test]
    fn test_normalize_invalid_format() {
        // 无效格式应该返回 None
        assert!(normalize_machine_id("invalid").is_none());
        assert!(normalize_machine_id("too-short").is_none());
        assert!(normalize_machine_id(&"g".repeat(64)).is_none()); // 非十六进制
        assert!(normalize_machine_id(&format!("----{}", "a".repeat(32))).is_none());
    }

    #[test]
    fn test_generate_with_uuid_machine_id() {
        let mut credentials = KiroCredentials::default();
        credentials.machine_id = Some("2582956e-cc88-4669-b546-07adbffcb894".to_string());

        let config = Config::default();

        let result = generate_from_credentials(&credentials, &config);
        assert_eq!(result.len(), 64);
    }

    #[test]
    fn fixed_identity_survives_token_rotation_and_reload() {
        let config = Config::default();
        let mut credentials = KiroCredentials {
            refresh_token: Some("original-refresh-token".to_string()),
            ..Default::default()
        };
        assert!(ensure_fixed_machine_id(&mut credentials, &config));
        let original_id = credentials.machine_id.clone();
        credentials.refresh_token = Some("rotated-refresh-token".to_string());
        assert!(!ensure_fixed_machine_id(&mut credentials, &config));

        let serialized = serde_json::to_string(&credentials).unwrap();
        let mut restored: KiroCredentials = serde_json::from_str(&serialized).unwrap();
        assert!(!ensure_fixed_machine_id(&mut restored, &config));
        assert_eq!(restored.machine_id, original_id);
    }

    #[test]
    fn fixed_identity_repairs_invalid_account_value_without_global_sharing() {
        let mut config = Config::default();
        config.machine_id = Some("a".repeat(64));
        let mut first = KiroCredentials {
            machine_id: Some("invalid-machine-id".to_string()),
            refresh_token: Some("first-account-token".to_string()),
            ..Default::default()
        };
        let mut second = KiroCredentials {
            refresh_token: Some("second-account-token".to_string()),
            ..Default::default()
        };
        assert!(ensure_fixed_machine_id(&mut first, &config));
        assert!(ensure_fixed_machine_id(&mut second, &config));
        assert_eq!(
            first.machine_id,
            Some(sha256_hex("KotlinNativeAPI/first-account-token"))
        );
        assert_ne!(first.machine_id, second.machine_id);
        assert_ne!(first.machine_id, config.machine_id);
    }

    #[test]
    fn fixed_identity_preserves_valid_explicit_value_and_normalizes_uuid_once() {
        let config = Config::default();
        let explicit = "B".repeat(64);
        let mut credentials = KiroCredentials {
            machine_id: Some(explicit.clone()),
            ..Default::default()
        };
        assert!(!ensure_fixed_machine_id(&mut credentials, &config));
        assert_eq!(credentials.machine_id, Some(explicit));

        let uuid = "2582956e-cc88-4669-b546-07adbffcb894";
        credentials.machine_id = Some(format!(" {uuid} "));
        assert!(ensure_fixed_machine_id(&mut credentials, &config));
        assert_eq!(credentials.machine_id, normalize_machine_id(uuid));
        assert!(!ensure_fixed_machine_id(&mut credentials, &config));
    }

    #[test]
    fn fixed_identity_without_secret_is_unique_before_account_ids_are_assigned() {
        let config = Config::default();
        let mut first = KiroCredentials::default();
        let mut second = KiroCredentials::default();
        assert!(ensure_fixed_machine_id(&mut first, &config));
        assert!(ensure_fixed_machine_id(&mut second, &config));
        assert_ne!(first.machine_id, second.machine_id);
        assert!(!ensure_fixed_machine_id(&mut first, &config));
    }
}
