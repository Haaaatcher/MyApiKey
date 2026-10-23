//! vault 文件的加解密。
//!
//! ## 文件格式（版本 1）
//!
//! ```text
//! 偏移   长度   字段
//! 0      4      magic：b"MAKV"
//! 4      2      version：u16 小端（当前为 1）
//! 6      4      Argon2id 内存参数 m_kib：u32 小端
//! 10     4      Argon2id 迭代参数 t：u32 小端
//! 14     4      Argon2id 并行参数 p：u32 小端
//! 18     16     盐（每次加密重新随机）
//! 34     12     AES-GCM nonce（每次加密重新随机）
//! 46     …      AES-256-GCM 密文（明文 = VaultData 的 JSON；附认证标签）
//! ```
//!
//! 头部全部 46 字节同时作为 AES-GCM 的 **AAD（附加认证数据）**：
//! 任何对头部（包括版本号、KDF 参数）的篡改都会导致解密失败，
//! 从根本上防止“降级攻击”——攻击者不能把高版本/强参数文件改成弱参数再交给我们解。
//!
//! ## 加密流程
//!
//! 1. 主密码 + 盐 → Argon2id（m=64 MiB, t=3, p=1）→ 32 字节密钥；
//! 2. 密钥 + nonce 对 JSON 明文做 AES-256-GCM 加密（AAD = 头部）；
//! 3. 输出 头部 || 密文。
//!
//! 每次保存全量重加密（文件只有几 KB，代价可忽略），盐与 nonce 都重新随机，
//! 同一份数据两次加密的输出不同（语义安全）。
//!
//! ## 防御性校验
//!
//! KDF 参数从文件中读入后先做上限检查（见 [`KDF_PARAM_LIMITS`]）：
//! 恶意构造的 vault 不能诱使我们对超大参数做无谓的内存/时间消耗（DoS）。

use crate::error::{Error, Result, SUPPORTED_VAULT_VERSION};
use crate::model::VaultData;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::Aes256Gcm;
use argon2::Algorithm::Argon2id;
use argon2::{Argon2, Params, Version};
use rand::RngCore;
use zeroize::Zeroizing;

/// 文件魔数。读取时第一个不匹配即判定“这不是 MyApiKey vault”。
const MAGIC: &[u8; 4] = b"MAKV";

/// 头部长度（不含密文）：magic(4) + version(2) + m(4) + t(4) + p(4) + salt(16) + nonce(12)。
const HEADER_LEN: usize = 4 + 2 + 4 + 4 + 4 + 16 + 12;

/// 盐长度（OWASP 建议不低于 16 字节）。
const SALT_LEN: usize = 16;

/// AES-GCM 标准 nonce 长度（96 bit）。
const NONCE_LEN: usize = 12;

/// 派生密钥长度（AES-256 需要 32 字节）。
const KEY_LEN: usize = 32;

/// 本程序加密时使用的默认 Argon2id 参数（OWASP 2024 对交互式登录场景的推荐下限之上）。
///
/// 解密时以文件头部记录的参数为准（向后兼容旧文件），
/// 但任何参数都会先经过 [`KDF_PARAM_LIMITS`] 的上限校验。
pub const DEFAULT_KDF_PARAMS: KdfParams = KdfParams {
    m_kib: 65536,
    t: 3,
    p: 1,
};

/// Argon2id 参数三元组（序列化进 vault 头部）。
///
/// `m_kib` 单位是 KiB：65536 KiB = 64 MiB。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    /// 内存开销（KiB）。
    pub m_kib: u32,
    /// 时间开销（迭代次数）。
    pub t: u32,
    /// 并行度。
    pub p: u32,
}

impl KdfParams {
    /// 构造并校验参数在 Argon2 库允许的范围内。
    ///
    /// # Errors
    /// - [`Error::Format`]：参数超出 Argon2 库本身允许的范围（如 m 最小 8 KiB）。
    fn validate(self) -> Result<Params> {
        Params::new(self.m_kib, self.t, self.p, Some(KEY_LEN)).map_err(|e| {
            Error::Format(format!(
                "invalid Argon2id parameters (m={}, t={}, p={}): {}",
                self.m_kib, self.t, self.p, e
            ))
        })
    }
}

/// 解密时接受的 KDF 参数上限。
///
/// 这些值远高于我们自己的默认参数，但足以挡住恶意构造的
/// “m=1TB / t=100000” 文件把解密过程变成资源耗尽攻击。
const KDF_PARAM_LIMITS: KdfParams = KdfParams {
    m_kib: 1_048_576,
    t: 32,
    p: 16,
};

/// 用主密码加密一份数据，产出完整的 vault 文件字节。
///
/// 盐与 nonce 均在本函数内重新随机生成；同一输入多次加密输出不同。
///
/// # Errors
/// - [`Error::Serde`]：VaultData 无法序列化为 JSON（理论上不会发生）；
/// - [`Error::Format`]：默认 KDF 参数异常；
/// - [`Error::Io`] / 底层加密错误：极端环境问题。
pub fn encrypt(data: &VaultData, passphrase: &str) -> Result<Vec<u8>> {
    // 明文 = JSON。serialize 到 Vec<u8>，避免 UTF-8 校验的额外拷贝
    let plaintext = serde_json::to_vec(data)
        .map_err(|e| Error::Serde(format!("failed to serialize vault payload: {e}")))?;

    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    // 派生密钥：Zeroizing 保证离开作用域后密钥内容被清零
    let key = derive_key(passphrase, &salt, DEFAULT_KDF_PARAMS)?;
    let key = Zeroizing::new(key);

    // 组装头部（先于加密完成，因为头部整体要作为 AAD）
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&SUPPORTED_VAULT_VERSION.to_le_bytes());
    header.extend_from_slice(&DEFAULT_KDF_PARAMS.m_kib.to_le_bytes());
    header.extend_from_slice(&DEFAULT_KDF_PARAMS.t.to_le_bytes());
    header.extend_from_slice(&DEFAULT_KDF_PARAMS.p.to_le_bytes());
    header.extend_from_slice(&salt);
    header.extend_from_slice(&nonce_bytes);
    debug_assert_eq!(
        header.len(),
        HEADER_LEN,
        "头部布局改动必须同步更新 HEADER_LEN"
    );

    let cipher = Aes256Gcm::new(GenericArray::from_slice(&*key));
    let nonce = GenericArray::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: &plaintext,
                aad: &header,
            },
        )
        .map_err(|_| {
            Error::Usage("AES-GCM encryption failed (memory or environment error)".into())
        })?;

    let mut out = header;
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// 解密并解析 vault 文件字节。
///
/// # Errors
/// - [`Error::Format`]：不是 vault 文件、被截断、或 KDF 参数超出接受上限；
/// - [`Error::UnsupportedVersion`]：版本高于本程序支持范围（提示升级程序）；
/// - [`Error::Decrypt`]：密码错误、或密文/头部任何一字节被篡改
///   （GCM 认证失败，故意不区分这两种情况）；
/// - [`Error::Serde`]：解密成功但明文不是合法 JSON（说明是程序 bug 或格式断裂）。
pub fn decrypt(bytes: &[u8], passphrase: &str) -> Result<VaultData> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::Format(format!(
            "file is {} bytes, too short for the {}-byte vault header",
            bytes.len(),
            HEADER_LEN
        )));
    }
    if &bytes[0..4] != MAGIC {
        return Err(Error::Format(
            "bad magic — not a MyApiKey vault file".into(),
        ));
    }

    // ---- 逐字段解析头部（全部小端） ----
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    let params = KdfParams {
        m_kib: u32::from_le_bytes(bytes[6..10].try_into().expect("长度已由 HEADER_LEN 保证")),
        t: u32::from_le_bytes(bytes[10..14].try_into().expect("长度已由 HEADER_LEN 保证")),
        p: u32::from_le_bytes(bytes[14..18].try_into().expect("长度已由 HEADER_LEN 保证")),
    };
    let salt: [u8; SALT_LEN] = bytes[18..34].try_into().expect("长度已由 HEADER_LEN 保证");
    let nonce_bytes: [u8; NONCE_LEN] = bytes[34..46].try_into().expect("长度已由 HEADER_LEN 保证");

    if version > SUPPORTED_VAULT_VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    // 防御性上限：挡住恶意参数导致的资源耗尽
    if params.m_kib > KDF_PARAM_LIMITS.m_kib
        || params.t > KDF_PARAM_LIMITS.t
        || params.p > KDF_PARAM_LIMITS.p
    {
        return Err(Error::Format(format!(
            "Argon2id parameters in the header (m={}, t={}, p={}) exceed accepted limits; refusing to decrypt",
            params.m_kib, params.t, params.p
        )));
    }

    let key = derive_key(passphrase, &salt, params)?;
    let key = Zeroizing::new(key);

    let cipher = Aes256Gcm::new(GenericArray::from_slice(&*key));
    let nonce = GenericArray::from_slice(&nonce_bytes);
    // AAD 绑定整个头部：改任何一个字节的头部都会让认证失败
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &bytes[HEADER_LEN..],
                aad: &bytes[..HEADER_LEN],
            },
        )
        .map_err(|_| Error::Decrypt)?;

    serde_json::from_slice(&plaintext).map_err(|e| {
        Error::Serde(format!(
            "vault payload is not valid JSON (ciphertext intact): {e}"
        ))
    })
}

/// Argon2id 密钥派生：主密码 + 盐 → 32 字节 AES 密钥。
///
/// 返回裸 `[u8; 32]`，**调用方必须立刻包进 `Zeroizing`**；
/// 本函数私有，库内仅有的两个调用点都遵守该约定。
fn derive_key(passphrase: &str, salt: &[u8], params: KdfParams) -> Result<[u8; KEY_LEN]> {
    let argon = Argon2::new(Argon2id, Version::V0x13, params.validate()?);
    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| Error::Format(format!("Argon2id computation failed: {e}")))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    //! 加密核心的安全测试：roundtrip、错误密码、逐字段篡改、截断。
    //!
    //! 原则：任何对密文或头部的单字节改动都必须导致解密失败——
    //! 这是“仓库里只有密文 + GCM 认证”这条安全承诺的全部技术依据。

    use super::*;
    use crate::model::{Protocol, Record};

    fn sample_vault() -> VaultData {
        let mut r = Record::new(
            "测试".into(),
            Protocol::Anthropic,
            "https://api.anthropic.com".into(),
            "sk-ant-api03-test".into(),
        );
        r.notes = Some("n".into());
        VaultData { records: vec![r] }
    }

    const PASS: &str = "correct horse battery staple";

    #[test]
    fn roundtrip_preserves_records() {
        let data = sample_vault();
        let bytes = encrypt(&data, PASS).expect("加密失败");
        let back = decrypt(&bytes, PASS).expect("解密失败");
        assert_eq!(back.records.len(), 1);
        assert_eq!(back.records[0].api_key, data.records[0].api_key);
        assert_eq!(back.records[0].protocol, data.records[0].protocol);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let bytes = encrypt(&sample_vault(), PASS).expect("加密失败");
        let err = decrypt(&bytes, "wrong password").unwrap_err();
        assert!(
            matches!(err, Error::Decrypt),
            "错误密码应返回 Decrypt，实际：{err}"
        );
    }

    #[test]
    fn tampering_ciphertext_fails() {
        let mut bytes = encrypt(&sample_vault(), PASS).expect("加密失败");
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01; // 翻转密文最后一个字节
        assert!(matches!(decrypt(&bytes, PASS).unwrap_err(), Error::Decrypt));
    }

    #[test]
    fn tampering_header_fails_via_aad() {
        let mut bytes = encrypt(&sample_vault(), PASS).expect("加密失败");
        // 篡改头部中的 t 参数（即使篡改后仍是“合法范围”），AAD 校验也应拦下
        bytes[10] ^= 0x01;
        assert!(
            matches!(decrypt(&bytes, PASS).unwrap_err(), Error::Decrypt),
            "头部（KDF 参数）属于 AAD，篡改必须失败"
        );
    }

    #[test]
    fn tampering_version_is_rejected() {
        let mut bytes = encrypt(&sample_vault(), PASS).expect("加密失败");
        bytes[4] = 0x63; // version 改成 99
        assert!(matches!(
            decrypt(&bytes, PASS).unwrap_err(),
            Error::UnsupportedVersion(99)
        ));
    }

    #[test]
    fn truncated_file_is_format_error() {
        let bytes = encrypt(&sample_vault(), PASS).expect("加密失败");
        let err = decrypt(&bytes[..HEADER_LEN - 1], PASS).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn garbage_magic_is_format_error() {
        let err = decrypt(b"NOT_A_VAULT_AT_ALL...............", PASS).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn malicious_kdf_params_are_rejected_before_hashing() {
        let mut bytes = encrypt(&sample_vault(), PASS).expect("加密失败");
        // 伪造 m=4 GiB 的参数；必须在 Argon2 计算之前就被上限检查拦下
        bytes[6..10].copy_from_slice(&(1u32 << 22).to_le_bytes());
        let err = decrypt(&bytes, PASS).unwrap_err();
        assert!(
            matches!(err, Error::Format(_)),
            "超大 KDF 参数应被拒绝，实际：{err}"
        );
    }

    #[test]
    fn encryption_is_semantically_random() {
        // 同一明文两次加密输出必须不同（盐/nonce 随机）
        let a = encrypt(&sample_vault(), PASS).expect("加密失败");
        let b = encrypt(&sample_vault(), PASS).expect("加密失败");
        assert_ne!(a, b);
    }

    #[test]
    fn empty_vault_roundtrips() {
        let bytes = encrypt(&VaultData::empty(), PASS).expect("加密失败");
        let back = decrypt(&bytes, PASS).expect("解密失败");
        assert!(back.records.is_empty());
    }
}
