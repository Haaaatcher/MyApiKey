//! 各操作系统上的目录布局。
//!
//! 布局（ADR/D7，详见 docs/design.md §3.3）：
//!
//! ```text
//! <config_dir>/
//! ├── repo/              # 本地 git 仓库（从 init 起就是完整 git 仓库）
//! │   └── vault.mkv      # 加密后的 vault（仓库中唯一的实质内容）
//! ├── config.toml        # 非敏感配置
//! ├── check_cache.json   # key 校验结果缓存（本地专属，不参与同步）
//! └── vault.lock         # 实例锁
//! ```
//!
//! `<config_dir>` 的取值优先级：
//! 1. 环境变量 `MYAPIKEY_HOME`（测试与便携部署用，直接指定的目录就是配置根）；
//! 2. 系统标准配置目录：Linux `~/.config/myapikey`、
//!    macOS `~/Library/Application Support/myapikey`、
//!    Windows `%APPDATA%\myapikey`。
//!
//! 所有路径都由本模块集中定义（AGENTS.md 要求），任何模块不得自行拼接路径。

use crate::error::{Error, Result};
use std::path::PathBuf;

/// 本应用在单台设备上的全部路径。
///
/// 惰性创建：`AppDirs::load()` 只保证对象本身可用，
/// `ensure_dirs()` 才真正落盘创建目录（`init` 与首次写入前调用）。
#[derive(Debug, Clone)]
pub struct AppDirs {
    /// 配置根目录。
    pub config_dir: PathBuf,
    /// 本地 git 仓库目录（`<config_dir>/repo`）。
    pub repo_dir: PathBuf,
    /// 加密 vault 文件（`<repo_dir>/vault.mkv`）。
    pub vault_path: PathBuf,
    /// 非敏感配置文件（`<config_dir>/config.toml`）。
    pub config_path: PathBuf,
    /// key 校验结果缓存（`<config_dir>/check_cache.json`）。
    pub check_cache_path: PathBuf,
    /// 实例锁文件（`<config_dir>/vault.lock`）。
    pub lock_path: PathBuf,
}

/// 环境变量名：覆盖配置根目录（测试 / 便携模式）。
pub const HOME_ENV: &str = "MYAPIKEY_HOME";

impl AppDirs {
    /// 解析当前设备的应用路径。
    ///
    /// # Errors
    /// - [`Error::Usage`]：`MYAPIKEY_HOME` 已设置但值是空的；
    /// - [`Error::Io`]：系统无法给出配置目录（极端情况，如 HOME 丢失）。
    pub fn load() -> Result<Self> {
        let base = match std::env::var(HOME_ENV) {
            Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
            Ok(_) => {
                return Err(Error::Usage(format!(
                    "environment variable {HOME_ENV} must not be empty"
                )))
            }
            Err(_) => {
                let sys = dirs::config_dir().ok_or_else(|| {
                    Error::Io(std::io::Error::other(
                        "cannot determine the system config directory",
                    ))
                })?;
                sys.join("myapikey")
            }
        };
        let repo_dir = base.join("repo");
        Ok(Self {
            vault_path: repo_dir.join("vault.mkv"),
            config_path: base.join("config.toml"),
            check_cache_path: base.join("check_cache.json"),
            lock_path: base.join("vault.lock"),
            repo_dir,
            config_dir: base,
        })
    }

    /// 创建所需的全部目录（幂等，已存在则跳过）。
    ///
    /// # Errors
    /// - [`Error::Io`]：目录创建失败（权限不足等）。
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.repo_dir)?;
        Ok(())
    }

    /// 判断本机是否已完成 `init`：以“git 仓库存在且 vault 文件存在”为准。
    ///
    /// config.toml / 缓存 / 锁文件不算初始化标志——它们可能在 init 之前就被
    /// 读取配置的路径触碰出来。
    pub fn is_initialized(&self) -> bool {
        self.repo_dir.join(".git").is_dir() && self.vault_path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn myapikey_home_overrides_base() {
        let tmp = std::env::temp_dir().join(format!("mak-test-{}", uuid::Uuid::now_v7()));
        // 临时设置环境变量后必须恢复，避免污染同进程其他测试
        std::env::set_var(HOME_ENV, &tmp);
        let dirs = AppDirs::load().expect("加载失败");
        std::env::remove_var(HOME_ENV);

        assert_eq!(dirs.config_dir, tmp);
        assert_eq!(dirs.vault_path, tmp.join("repo").join("vault.mkv"));
        assert_eq!(dirs.lock_path, tmp.join("vault.lock"));
        assert!(dirs.check_cache_path.starts_with(&dirs.config_dir));
    }
}
