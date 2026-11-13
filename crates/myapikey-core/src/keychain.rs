//! 系统凭据库（记住解锁密码）的读写封装，带**有效期（TTL）**。
//!
//! 后端（`keyring` crate 自动路由，见 docs/design.md §9.1）：
//! - Windows：Credential Manager
//! - macOS：Keychain
//! - Linux：Secret Service（GNOME Keyring / KWallet）
//!
//! 条目约定（D57 起）：service = `makara`，account = `default`
//! （account 预留命名空间，未来多 vault 时每个 vault 一个 account）。
//! D57 前的条目在旧 service 名 `myapikey` 下——读路径「新名 miss →
//! 旧名回退 + 迁移写回」兼容既有用户；旧条目不主动删除（macOS 上
//! 删除同样触发授权弹窗，见 [`SERVICE_LEGACY`]）。
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

/// 凭据库 service 名（D57 起为 `makara`，产品统一命名；非 macOS 平台的
/// github.rs 令牌条目共用同一 service。授权弹窗里向用户展示的就是它——
/// 旧名 `myapikey` 与产品名割裂，见 [`SERVICE_LEGACY`]）。
pub(crate) const SERVICE: &str = "makara";
/// D57 前的旧 service 名：主密码副本与（D56 前各平台的）GitHub 令牌的
/// 历史条目都在其下。读路径回退到这里并迁移写回新名；**旧条目一律不
/// 主动删除**——macOS 上删除同样触发授权弹窗，违背 D56 治理目标
/// （`forget`/`remote unset` 的清理路径除外：读路径授权过则删除自然
/// 免弹，对旧条目尽力删除、失败静默）。
pub(crate) const SERVICE_LEGACY: &str = "myapikey";
/// `MYAPIKEY_HOME` 覆盖（测试/便携）环境使用的隔离 service 名。
///
/// 这些环境的凭据条目必须与正式安装分开：不仅避免测试互相串扰，
/// 更防止隔离环境里的清理操作（如锁定清除记住密码）误删真实条目。
/// 命名历史独立，不随 D57 改名（隔离条目无跨版本迁移语义）。
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
///
/// D57 迁移：新 service 名下无条目时回退读旧名（`myapikey`）——命中即
/// 迁入新名（写回）+ 日志留痕。隔离环境跳过回退（[`SERVICE_ISOLATED`]
/// 无历史条目，没有迁移语义）。
fn get_raw(dirs: &AppDirs) -> Option<String> {
    let svc = service_of(dirs);
    let legacy_svc = if dirs.from_env {
        None
    } else {
        Some(SERVICE_LEGACY)
    };
    get_with_migration(svc, legacy_svc, ACCOUNT, &dirs.config_dir)
}

/// 迁移完成标记文件（放应用 config 目录，存在即表示遗留条目已清账）。
/// 仅 macOS 使用（防授权弹窗反复的标记；其他平台凭据库回退无弹窗成本，
/// 不需要标记——见 [`get_with_migration`]）。
#[cfg(target_os = "macos")]
const MIGRATED_FLAG: &str = "keychain-migrated.flag";

/// 按「当前 service + 可选遗留 service」读取的迁移式读取核心。
///
/// 供 [`get_raw`]、github.rs 的令牌读取与 macOS 专属测试共用：新条目
/// 命中直接返回；miss 且给定了遗留名时读遗留条目，命中即迁移（写回
/// 新名，失败静默——迁移写回只是优化，下次会再触发回退，不丢失数据）。
/// 两次读取都失败返回 `None`。
///
/// D57 增补——**迁移标记（仅 macOS）**：命中迁移、或确认遗留条目根本
/// 不存在（NoEntry）时写 [`MIGRATED_FLAG`]；标记存在则不再回退读取。
/// 动因：TTL 到期/锁定清理只删新名条目，若无标记，macOS 上每个清理
/// 周期后都会再次回退读遗留条目、反复触发授权弹窗（实测）。保守边界
/// ——遗留条目「存在但读取失败」（如授权被拒）不写标记，下次仍会尝试。
///
/// **2026-11-13 Windows 实测修正——标记必须平台门控**：标记是全局一个
/// 文件，而主密码副本与 GitHub 令牌是两条独立迁移线——主密码先迁移
/// 落标记后，令牌的回退读取被标记误杀，导致已绑定用户被折叠成
/// standalone（靶机真实数据复现）。Windows/Linux 凭据库无授权弹窗
/// 成本，回退永远安全，标记逻辑 `cfg` 门控为 macOS 专属。
pub(crate) fn get_with_migration(
    service: &str,
    legacy_service: Option<&str>,
    account: &str,
    config_dir: &std::path::Path,
) -> Option<String> {
    let current = keyring::Entry::new(service, account).ok()?;
    if let Ok(p) = current.get_password() {
        return Some(p);
    }
    // 非 macOS 不用标记（config_dir 仅 macOS 的防弹窗标记需要）
    #[cfg(not(target_os = "macos"))]
    let _ = config_dir;
    let legacy_service = legacy_service?;
    #[cfg(target_os = "macos")]
    {
        let flag = config_dir.join(MIGRATED_FLAG);
        if flag.exists() {
            return None;
        }
    }
    let legacy_entry = keyring::Entry::new(legacy_service, account).ok()?;
    match legacy_entry.get_password() {
        // 命中迁移：写回新条目 + 直接返回迁移值（不再回读新条目——少
        // 一次凭据库访问，macOS 上少一次弹窗风险）。**标记只在写回成功
        // 后落**：写回失败（凭据库写异常，如 Windows 非交互会话）时若
        // 落标记，下次新条目 miss + 标记挡回退 = 副本静默丢失
        Ok(p) => {
            crate::log::info(
                "keychain",
                "legacy service entry hit (myapikey); migrating passphrase copy to makara",
            );
            let written = current.set_password(&p).is_ok();
            #[cfg(target_os = "macos")]
            if written {
                let _ = std::fs::write(config_dir.join(MIGRATED_FLAG), "1");
            }
            // 非 macOS 不落标记，written 无人消费——显式消化免 unused 警告
            #[cfg(not(target_os = "macos"))]
            let _ = written;
            Some(p)
        }
        // 旧条目根本不存在：无可迁移之物，标记后永不再回退（仅 macOS
        // 需要标记防弹窗；其他平台条目不存在本就无害）
        Err(keyring::Error::NoEntry) => {
            #[cfg(target_os = "macos")]
            {
                let _ = std::fs::write(config_dir.join(MIGRATED_FLAG), "1");
            }
            None
        }
        // 存在但读取失败（授权被拒/后端异常）：保守不标记，下次重试
        Err(_) => None,
    }
}

/// 直接写凭据库（不带 TTL）。始终写新 service 名（[`SERVICE`]）。
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

/// 直接删凭据库条目。D57 起对旧 service 名条目**尽力删除、失败静默**：
/// `forget` 的语义是清掉所有记住的副本，残留旧条目等于留下孤儿密码；
/// 而 macOS 上读路径（解锁）已对旧条目授权过，删除自然免弹——未授权
/// 的场景删除失败也不值得为它失败（残留由用户手动清理）。
///
/// # Errors
/// - [`Error::Keyring`]：新名条目存在但删除失败。
fn delete_raw(dirs: &AppDirs) -> Result<bool> {
    let entry = keyring::Entry::new(service_of(dirs), ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open credential store entry: {e}")))?;
    let mut removed = match entry.delete_credential() {
        Ok(()) => true,
        Err(keyring::Error::NoEntry) => false,
        Err(e) => {
            return Err(Error::Keyring(format!(
                "failed to delete credential store entry: {e}"
            )));
        }
    };
    if !dirs.from_env {
        if let Ok(legacy) = keyring::Entry::new(SERVICE_LEGACY, ACCOUNT) {
            // 旧条目删成功也算「清掉了记住的副本」——只删掉新条目
            // NoEntry 而旧条目清掉的场合，forget 报 false 是误报
            if legacy.delete_credential().is_ok() {
                removed = true;
            }
        }
    }
    Ok(removed)
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

    /// D57 迁移核心（[`get_with_migration`]）的行为测试：自建一对测试
    /// service 条目验证「旧名命中 → 迁移写回新名 → 再读走新名」。条目
    /// 由本测试创建（创建者隐式授权，不触发弹窗），结束即清理；keyring
    /// 后端不可用（无头 Linux）时整段断言无意义，直接跳过。
    /// 仅 macOS 跑：Windows/Linux CI 的凭据库语义与开发机差异大，且
    /// 本测试的治理目标（授权弹窗）是 macOS 特有，其余平台由既有
    /// 集成测试与真机冒烟覆盖。
    #[cfg(target_os = "macos")]
    #[test]
    fn migration_moves_entry_from_legacy_to_current_service() {
        use uuid::Uuid;

        // 随机 service 名：测试自隔离，绝不触碰真实 makara/myapikey 条目
        let svc = format!("mak-test-{}", Uuid::now_v7().simple());
        let legacy_svc = format!("mak-test-legacy-{}", Uuid::now_v7().simple());
        let acct = "test";
        let dir = std::env::temp_dir().join(format!(
            "mak-migration-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let flag = dir.join(MIGRATED_FLAG);
        let write = |service: &str, value: &str| {
            keyring::Entry::new(service, acct)
                .unwrap()
                .set_password(value)
                .unwrap();
        };
        let read_current = |service: &str| {
            keyring::Entry::new(service, acct)
                .unwrap()
                .get_password()
                .ok()
        };
        let cleanup = || {
            for s in [&svc, &legacy_svc] {
                if let Ok(e) = keyring::Entry::new(s, acct) {
                    let _ = e.delete_credential();
                }
            }
        };
        cleanup(); // 防理论上的同名碰撞（uuid 碰撞可忽略，防御性清理）
        write(&legacy_svc, "old-passphrase");
        assert_eq!(read_current(&svc), None, "新条目尚未存在");

        // 第一次读：新名 miss → 旧名命中 → 迁移写回 + 落标记
        let got = get_with_migration(&svc, Some(&legacy_svc), acct, &dir);
        assert_eq!(got.as_deref(), Some("old-passphrase"));
        assert!(flag.exists(), "迁移完成应落标记");
        // 迁移落地：新条目已存在（下次读取不再回退）
        assert_eq!(
            read_current(&svc).as_deref(),
            Some("old-passphrase"),
            "迁移应写入新条目"
        );
        // 第二次读：直接走新条目
        assert_eq!(
            get_with_migration(&svc, Some(&legacy_svc), acct, &dir).as_deref(),
            Some("old-passphrase")
        );

        // 标记生效（D57 增补的核心）：清掉新条目后读取**不再回退**——
        // TTL 清理周期后不得反复回退读遗留条目触发授权弹窗
        keyring::Entry::new(&svc, acct)
            .unwrap()
            .delete_credential()
            .unwrap();
        assert_eq!(
            get_with_migration(&svc, Some(&legacy_svc), acct, &dir),
            None
        );

        // 无遗留条目（NoEntry）也落标记：防某些环境反复撞弹窗授权
        // （前提：先删标记，否则标记存在时不会走到遗留读取）
        cleanup();
        let _ = std::fs::remove_file(&flag);
        assert_eq!(
            get_with_migration(&svc, Some(&legacy_svc), acct, &dir),
            None
        );
        assert_eq!(
            std::fs::read_to_string(&flag).unwrap(),
            "1",
            "NoEntry 应落标记"
        );
        // 遗留名 None（隔离语义）：只读当前，miss 即 None
        write(&svc, "current-only");
        assert_eq!(
            get_with_migration(&svc, None, acct, &dir).as_deref(),
            Some("current-only")
        );
        cleanup();
    }
}
