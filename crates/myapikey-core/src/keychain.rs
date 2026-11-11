//! 系统凭据库（记住解锁密码）的读写封装，带**有效期（TTL）**。
//!
//! 后端（`keyring` crate 自动路由，见 docs/design.md §9.1）：
//! - Windows：Credential Manager
//! - macOS：Keychain
//! - Linux：Secret Service（GNOME Keyring / KWallet）
//!
//! 条目约定：service = `myapikey`，account = `default`
//! （account 预留命名空间，未来多 vault 时每个 vault 一个 account）。
//! 存储内容是**解锁密码本身**而非派生密钥——这样 KDF 参数可随 vault
//! 版本演进，改参数不需要动凭据库。
//!
//! ## TTL 模型（2026-11-03 D32 修订：时长统一为自动锁定）
//!
//! 记住副本是实现细节，用户只感知「解锁密码 + 自动锁定时长」：
//! - 解锁成功 / 持续使用 → 副本有效期滚动续至 `now + auto_lock_minutes`；
//! - 空闲到期或手动锁定 → 调用方清副本，下次要求输入解锁密码；
//! - `auto_lock_minutes = 0` → 永不锁定，副本不过期（重启也免输）；
//! - 密码存凭据库；到期时间存 `<config>/remember.json`（仅时间戳，非敏感）；
//! - [`get_with_expiry`] 发现过期时自动清理凭据库条目并返回 `None`。
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
/// `MYAPIKEY_HOME` 覆盖（测试/便携）环境使用的隔离 service 名。
///
/// 这些环境的凭据条目必须与正式安装分开：不仅避免测试互相串扰，
/// 更防止隔离环境里的清理操作（如锁定清除记住密码）误删真实条目。
const SERVICE_ISOLATED: &str = "myapikey-isolated";
/// 凭据库 account 名（多 vault 预留）。
const ACCOUNT: &str = "default";

/// 按 home 来源解析实际使用的 service（见 [`SERVICE_ISOLATED`]）。
fn service_of(dirs: &AppDirs) -> &'static str {
    if dirs.from_env {
        SERVICE_ISOLATED
    } else {
        SERVICE
    }
}

/// 到期时间记录文件（放在配置目录，内容只有一个时间戳）。
const EXPIRY_FILE: &str = "remember.json";

/// `remember.json` 的形状。
#[derive(Debug, Serialize, Deserialize)]
struct Expiry {
    /// 记住状态的到期时刻（RFC 3339）。
    expires_at: String,
}

/// 带有效期地记住解锁密码：写凭据库 + 写到期时间。
///
/// `minutes` 为自动锁定时长：0 = 永不锁定，副本写为不过期（远期时刻，
/// 结构不变）；否则有效期 = `now + minutes` 分钟，由调用方随使用滚动续期。
///
/// # Errors
/// - [`Error::Keyring`]：凭据库写入失败（如无头 Linux）；
/// - [`Error::Io`] / [`Error::Serde`]：到期文件写入失败。
pub fn set_ttl(dirs: &AppDirs, passphrase: &str, minutes: u32) -> Result<()> {
    set_raw(dirs, passphrase)?;
    // 0 = 永不锁定：写一个实质永久的时刻，避免给 remember.json 引入
    // 第二种形状（无过期字段）导致旧版本读取行为分叉
    let expires_at = if minutes == 0 {
        Utc::now() + chrono::Duration::days(36500)
    } else {
        Utc::now() + chrono::Duration::minutes(i64::from(minutes))
    };
    let exp = Expiry {
        expires_at: expires_at.to_rfc3339(),
    };
    let raw = serde_json::to_string(&exp)
        .map_err(|e| Error::Serde(format!("failed to serialize expiry: {e}")))?;
    std::fs::write(dirs.config_dir.join(EXPIRY_FILE), raw)?;
    Ok(())
}

/// 读取记住副本的过期时刻（只碰 remember.json，**不碰凭据库**）。
/// 供调用方节流滚动续期——距过期尚有半程以上时无需每个 tick 重写
/// （凭据库 + 文件各一次 IO，2026-11-05 审查可选 D）。
///
/// # Returns
/// - `Ok(Some(at))`：文件存在且时间戳合法（含已过期的时刻）；
/// - `Ok(None)`：无文件 / JSON 损坏 / 时间戳非法——调用方按"需重写"处理，
///   与 [`get_with_expiry`] 的"损坏即视为过期"同语义。
///
/// # Errors
/// - [`Error::Io`]：remember.json 读取失败（非缺失）。
pub fn expires_at(dirs: &AppDirs) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    let raw = match std::fs::read_to_string(dirs.config_dir.join(EXPIRY_FILE)) {
        Ok(r) => r,
        // 缺失 = 没记住,不是错误
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    Ok(serde_json::from_str::<Expiry>(&raw)
        .ok()
        .and_then(|exp| DateTime::parse_from_rfc3339(&exp.expires_at).ok())
        .map(|t| t.with_timezone(&Utc)))
}

/// 读取仍在有效期内的解锁密码；过期或不存在时清理并返回 `None`。
///
/// # Returns
/// - `Ok(Some(passphrase))`：有未过期的记住密码；
/// - `Ok(None)`：没记住 / 已过期（已顺手清理）/ 凭据库不可用。
///
/// # Errors
/// 不失败：凭据库打不开/条目缺失、到期文件缺失或损坏均降级为
/// `Ok(None)`（清理后回手动输入）。保留 `Result` 与调用方契约一致。
///
/// 文件损坏（JSON 非法或时间戳解析失败）一律视为过期：清理条目并
/// 返回 `Ok(None)` 降级到重新输入——到期文件不含凭据，不值得为它失败。
pub fn get_with_expiry(dirs: &AppDirs) -> Result<Option<String>> {
    let Some(passphrase) = get_raw(dirs) else {
        return Ok(None);
    };
    // 没有到期文件（旧版本写入的永久条目）：视为已过期，走清理 + 重输
    let Ok(raw) = std::fs::read_to_string(dirs.config_dir.join(EXPIRY_FILE)) else {
        let _ = delete(dirs);
        return Ok(None);
    };
    // JSON 本身损坏：与"到期文件缺失"同语义——清理 + 重输（2026-10-26
    // 修复：曾返回 Err 卡住调用方且坏文件永久留存，与文档"损坏即视为
    // 过期清理"矛盾）
    let Ok(exp) = serde_json::from_str::<Expiry>(&raw) else {
        let _ = delete(dirs);
        return Ok(None);
    };
    let Ok(expires_at) = DateTime::parse_from_rfc3339(&exp.expires_at) else {
        let _ = delete(dirs);
        return Ok(None);
    };
    if Utc::now() >= expires_at {
        crate::log::info("keychain", "remembered passphrase expired; cleared");
        let _ = delete(dirs);
        return Ok(None);
    }
    Ok(Some(passphrase))
}

/// 立即清除记住状态（凭据库条目 + 到期文件）——`makara forget` 的实现。
///
/// # Returns
/// - `Ok(true)`：清掉了凭据库条目；
/// - `Ok(false)`：本来就没有记住的密码。
///
/// # Errors
/// - [`Error::Keyring`]：条目存在但删除失败（真实异常，值得报告）。
pub fn delete(dirs: &AppDirs) -> Result<bool> {
    let _ = std::fs::remove_file(dirs.config_dir.join(EXPIRY_FILE));
    delete_raw(dirs)
}

// ---------------------------------------------------------------------------
// 凭据库裸操作（TTL 逻辑之外不直接使用）
// ---------------------------------------------------------------------------

/// 从凭据库读密码；后端不可用或无条目一律 `Ok(None)`（降级为手动输入）。
fn get_raw(dirs: &AppDirs) -> Option<String> {
    let entry = keyring::Entry::new(service_of(dirs), ACCOUNT).ok()?;
    entry.get_password().ok()
}

/// 直接写凭据库（不带 TTL）。
///
/// # Errors
/// - [`Error::Keyring`]：后端不可用或写入失败。
fn set_raw(dirs: &AppDirs, passphrase: &str) -> Result<()> {
    let entry = keyring::Entry::new(service_of(dirs), ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open credential store entry: {e}")))?;
    entry
        .set_password(passphrase)
        .map_err(|e| Error::Keyring(format!("failed to write to credential store: {e}")))
}

/// 直接删凭据库条目。
///
/// # Errors
/// - [`Error::Keyring`]：条目存在但删除失败。
fn delete_raw(dirs: &AppDirs) -> Result<bool> {
    let entry = keyring::Entry::new(service_of(dirs), ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open credential store entry: {e}")))?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(Error::Keyring(format!(
            "failed to delete credential store entry: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    //! `expires_at` 只碰 remember.json（不碰凭据库），可用临时目录直测。
    //! `set_ttl` 会写真实/隔离凭据库，这里不调用——过期时刻的写入路径
    //! 由 GUI lib.rs 的集成测试（隔离 service）覆盖。

    use super::*;

    fn temp_dirs(tag: &str) -> AppDirs {
        let root = std::env::temp_dir().join(format!(
            "mak-keychain-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        AppDirs {
            config_dir: root.clone(),
            repo_dir: root.join("repo"),
            vault_path: root.join("repo/vault.mkv"),
            config_path: root.join("config.toml"),
            check_cache_path: root.join("check_cache.json"),
            lock_path: root.join("vault.lock"),
            from_env: true,
        }
    }

    #[test]
    fn expires_at_reads_timestamp_or_none() {
        let dirs = temp_dirs("exp");
        // 无文件 → None
        assert_eq!(expires_at(&dirs).unwrap(), None);
        // 合法未来时刻 → Some
        std::fs::write(
            dirs.config_dir.join(EXPIRY_FILE),
            r#"{"expires_at":"2099-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let at = expires_at(&dirs).unwrap().expect("应读到未来时刻");
        assert!(at > Utc::now());
        // 已过时刻 → 仍返回 Some(过期与否由调用方判断,本函数只读)
        std::fs::write(
            dirs.config_dir.join(EXPIRY_FILE),
            r#"{"expires_at":"2000-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let past = expires_at(&dirs).unwrap().expect("应读到过去时刻");
        assert!(past < Utc::now());
        // JSON 损坏 / 时间戳非法 → None(与 get_with_expiry 的"损坏视为
        // 过期"同语义,调用方按需重写)
        std::fs::write(dirs.config_dir.join(EXPIRY_FILE), "{broken").unwrap();
        assert_eq!(expires_at(&dirs).unwrap(), None);
        std::fs::write(
            dirs.config_dir.join(EXPIRY_FILE),
            r#"{"expires_at":"not-a-time"}"#,
        )
        .unwrap();
        assert_eq!(expires_at(&dirs).unwrap(), None);
        let _ = std::fs::remove_dir_all(dirs.config_dir);
    }
}
