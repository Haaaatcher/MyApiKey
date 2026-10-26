//! 非敏感配置（`config.toml`）。
//!
//! 只放设备本地的行为开关与状态，**任何敏感信息（主密码、api_key）都不许进本文件**。
//! 与 vault 不同，本文件是明文 TOML、不随 git 仓库同步——
//! 每台设备有自己的偏好（例如墓碑保留期）是合理的。

use crate::error::{Error, Result};
use crate::paths::AppDirs;
use serde::{Deserialize, Serialize};

/// 应用配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    /// 删除墓碑的保留天数。
    ///
    /// 墓碑在合并时被清理的条件是 `deleted_at` 距今超过该天数。
    /// 默认 30 天：足以覆盖“一台设备一个月没开机”的常见场景；
    /// 设置过短会让长期离线设备上的已删记录复活（设计权衡见 docs/design.md §5.3）。
    pub tombstone_days: u32,

    /// 保存记录后是否自动尝试 push（失败只提示不阻塞）。
    ///
    /// 关闭后仅在本机 commit，跨设备同步完全依赖手动 `mak sync`。
    pub auto_push: bool,

    /// 主密码记住的有效期天数（2026-10-24 决议：输入一次默认记住一周）。
    ///
    /// 每次成功输入都会重置计时；到期后下次命令会要求重新输入一次并再次续期。
    /// 设为 `0` = 永不记住（每次命令都手动输入）。
    pub remember_days: u32,

    /// GUI 空闲自动锁定分钟数（软锁：销毁内存中的解锁状态）。
    ///
    /// 锁定后回到解锁页；若主密码仍在记忆有效期内，一键解锁即可恢复。
    pub auto_lock_minutes: u32,

    /// 最近一次成功同步的时间（RFC 3339，本机展示用，非精确账本）。
    pub last_sync_at: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            tombstone_days: 30,
            auto_push: true,
            remember_days: 7,
            auto_lock_minutes: 10,
            last_sync_at: None,
        }
    }
}

impl AppConfig {
    /// 从 `config.toml` 加载；文件不存在时返回默认值（首次运行前调用是合法场景）。
    ///
    /// # Errors
    /// - [`Error::Io`]：文件存在但读取失败；
    /// - [`Error::Serde`]：TOML 内容非法。损坏时直接报错而不是静默重置——
    ///   用户改过配置，静默丢配置比报错更糟。
    pub fn load(dirs: &AppDirs) -> Result<Self> {
        if !dirs.config_path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&dirs.config_path)?;
        toml::from_str(&raw).map_err(|e| Error::Serde(format!("failed to parse config.toml: {e}")))
    }

    /// 写回 `config.toml`（原子写：先写临时文件再改名，避免半截文件）。
    ///
    /// # Errors
    /// - [`Error::Serde`] / [`Error::Io`]：序列化或写盘失败。
    pub fn save(&self, dirs: &AppDirs) -> Result<()> {
        let raw = toml::to_string_pretty(self)
            .map_err(|e| Error::Serde(format!("failed to serialize config.toml: {e}")))?;
        let tmp = dirs.config_path.with_extension("toml.tmp");
        std::fs::write(&tmp, raw)?;
        // Windows 上 rename 目标存在时需要先移除；配置文件场景直接覆盖语义
        if dirs.config_path.exists() {
            std::fs::remove_file(&dirs.config_path)?;
        }
        std::fs::rename(&tmp, &dirs.config_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrip_and_missing_file() {
        let tmp = std::env::temp_dir().join(format!("mak-cfg-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&tmp).expect("创建临时目录失败");
        let dirs = AppDirs {
            config_dir: tmp.clone(),
            repo_dir: tmp.join("repo"),
            vault_path: tmp.join("repo/vault.mkv"),
            config_path: tmp.join("config.toml"),
            check_cache_path: tmp.join("check_cache.json"),
            lock_path: tmp.join("vault.lock"),
            from_env: false,
        };

        // 文件不存在 → 默认值
        let cfg = AppConfig::load(&dirs).expect("加载失败");
        assert_eq!(cfg.tombstone_days, 30);
        assert!(cfg.auto_push);

        // 保存后回读一致
        let mut cfg = cfg;
        cfg.tombstone_days = 7;
        cfg.last_sync_at = Some("2026-10-23T00:00:00Z".into());
        cfg.save(&dirs).expect("保存失败");
        let back = AppConfig::load(&dirs).expect("回读失败");
        assert_eq!(back.tombstone_days, 7);
        assert_eq!(back.last_sync_at.as_deref(), Some("2026-10-23T00:00:00Z"));
    }
}
