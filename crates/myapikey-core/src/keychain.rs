//! 系统凭据库（记住主密码）的读写封装，带**有效期（TTL）**。
//!
//! 后端（`keyring` crate 自动路由，见 docs/design.md §9.1）：
//! - Windows：Credential Manager
//! - macOS：Keychain
//! - Linux：Secret Service（GNOME Keyring / KWallet）
//!
//! 条目约定：service = `myapikey`，account = `default`
//! （account 预留命名空间，未来多 vault 时每个 vault 一个 account）。
//! 存储内容是**主密码本身**而非派生密钥——这样 KDF 参数可随 vault
//! 版本演进，改参数不需要动凭据库。
//!
//! ## TTL 模型（2026-10-24 决议）
//!
//! 主密码**输入一次即记住，默认 7 天有效**（`config.toml` 的 `remember_days`，
//! 0 = 永不记住）：
//! - 密码存凭据库；到期时间存 `<config>/remember.json`（仅时间戳，非敏感）；
//! - 每次成功输入都会重置计时（续 7 天）；
//! - [`get_with_expiry`] 发现过期时自动清理凭据库条目并返回 `None`，
//!   调用方提示重新输入——用户至多每周输一次；
//! - [`delete`]（`mak forget`）= 立即锁定：下次命令问一次，输入后又开始新的 7 天。
//!
//! 降级策略：Linux 无头/纯 SSH 环境常没有 Secret Service，
//! 所有读取返回 `Ok(None)` 由调用方退回手动输入；绝不因凭据库
//! 不可用而中断命令。

use crate::error::{Error, Result};
use crate::paths::AppDirs;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 凭据库 service 名（github.rs 的 token 条目共用同一 service）。
pub(crate) const SERVICE: &str = "myapikey";
/// 凭据库 account 名（多 vault 预留）。
const ACCOUNT: &str = "default";

/// 到期时间记录文件（放在配置目录，内容只有一个时间戳）。
const EXPIRY_FILE: &str = "remember.json";

/// `remember.json` 的形状。
#[derive(Debug, Serialize, Deserialize)]
struct Expiry {
    /// 记住状态的到期时刻（RFC 3339）。
    expires_at: String,
}

/// 带有效期地记住主密码：写凭据库 + 写到期时间。
///
/// `days` 为 0 时不写任何东西（“永不记住”模式由调用方处理提示）。
///
/// # Errors
/// - [`Error::Keyring`]：凭据库写入失败（如无头 Linux）；
/// - [`Error::Io`] / [`Error::Serde`]：到期文件写入失败。
pub fn set_ttl(dirs: &AppDirs, passphrase: &str, days: u32) -> Result<()> {
    if days == 0 {
        return Ok(());
    }
    set_raw(passphrase)?;
    let exp = Expiry {
        expires_at: (Utc::now() + chrono::Duration::days(i64::from(days))).to_rfc3339(),
    };
    let raw = serde_json::to_string(&exp)
        .map_err(|e| Error::Serde(format!("failed to serialize expiry: {e}")))?;
    std::fs::write(dirs.config_dir.join(EXPIRY_FILE), raw)?;
    Ok(())
}

/// 读取仍在有效期内的主密码；过期或不存在时清理并返回 `None`。
///
/// # Returns
/// - `Ok(Some(passphrase))`：有未过期的记住密码；
/// - `Ok(None)`：没记住 / 已过期（已顺手清理）/ 凭据库不可用。
///
/// # Errors
/// - [`Error::Serde`]：到期文件损坏（不含凭据，损坏即视为过期清理）。
pub fn get_with_expiry(dirs: &AppDirs) -> Result<Option<String>> {
    let Some(passphrase) = get_raw() else {
        return Ok(None);
    };
    // 没有到期文件（旧版本写入的永久条目）：视为已过期，走清理 + 重输
    let Ok(raw) = std::fs::read_to_string(dirs.config_dir.join(EXPIRY_FILE)) else {
        let _ = delete(dirs);
        return Ok(None);
    };
    let exp: Expiry = serde_json::from_str(&raw)
        .map_err(|e| Error::Serde(format!("failed to parse {EXPIRY_FILE}: {e}")))?;
    let Ok(expires_at) = DateTime::parse_from_rfc3339(&exp.expires_at) else {
        let _ = delete(dirs);
        return Ok(None);
    };
    if Utc::now() >= expires_at {
        let _ = delete(dirs);
        return Ok(None);
    }
    Ok(Some(passphrase))
}

/// 立即清除记住状态（凭据库条目 + 到期文件）——`mak forget` 的实现。
///
/// # Returns
/// - `Ok(true)`：清掉了凭据库条目；
/// - `Ok(false)`：本来就没有记住的密码。
///
/// # Errors
/// - [`Error::Keyring`]：条目存在但删除失败（真实异常，值得报告）。
pub fn delete(dirs: &AppDirs) -> Result<bool> {
    let _ = std::fs::remove_file(dirs.config_dir.join(EXPIRY_FILE));
    delete_raw()
}

// ---------------------------------------------------------------------------
// 凭据库裸操作（TTL 逻辑之外不直接使用）
// ---------------------------------------------------------------------------

/// 从凭据库读密码；后端不可用或无条目一律 `Ok(None)`（降级为手动输入）。
fn get_raw() -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).ok()?;
    match entry.get_password() {
        Ok(p) => Some(p),
        Err(keyring::Error::NoEntry) => None,
        Err(_) => None,
    }
}

/// 直接写凭据库（不带 TTL）。
///
/// # Errors
/// - [`Error::Keyring`]：后端不可用或写入失败。
fn set_raw(passphrase: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open credential store entry: {e}")))?;
    entry
        .set_password(passphrase)
        .map_err(|e| Error::Keyring(format!("failed to write to credential store: {e}")))
}

/// 直接删凭据库条目。
///
/// # Errors
/// - [`Error::Keyring`]：条目存在但删除失败。
fn delete_raw() -> Result<bool> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open credential store entry: {e}")))?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(Error::Keyring(format!(
            "failed to delete credential store entry: {e}"
        ))),
    }
}
