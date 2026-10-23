//! key 校验结果的本地缓存。
//!
//! 缓存**只属于本机**，绝不进入 vault / git 仓库（docs/design.md §6）：
//! 校验结果随时间与网络环境变化，参与同步只会制造 merge 噪音。
//!
//! 文件形状：`{ "<record_id>": {"status": "...", "checked_at": "...", "http": 401} }`。

use crate::error::{Error, Result};
use crate::paths::AppDirs;
use crate::probe::CheckStatus;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// 单条记录的校验快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckEntry {
    /// 校验结论。
    pub status: CheckStatus,
    /// 校验发生时间（RFC 3339）。
    pub checked_at: DateTime<Utc>,
    /// 服务端返回的 HTTP 状态码（`None` = 没拿到响应，如超时/DNS 失败）。
    pub http: Option<u16>,
}

/// 整个缓存：record_id → 最近一次校验结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckCache(BTreeMap<Uuid, CheckEntry>);

impl CheckCache {
    /// 从缓存文件加载；文件不存在或损坏时返回空缓存
    /// （缓存丢了无害——重新 `check` 即可，不值得为一个缓存报错打断用户）。
    pub fn load(dirs: &AppDirs) -> Self {
        std::fs::read(&dirs.check_cache_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    }

    /// 原子写回缓存文件（先写临时文件再改名）。
    ///
    /// # Errors
    /// - [`Error::Serde`] / [`Error::Io`]：序列化或写盘失败。
    pub fn save(&self, dirs: &AppDirs) -> Result<()> {
        let raw = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Serde(format!("failed to serialize check cache: {e}")))?;
        let tmp = dirs.check_cache_path.with_extension("json.tmp");
        std::fs::write(&tmp, raw)?;
        if dirs.check_cache_path.exists() {
            std::fs::remove_file(&dirs.check_cache_path)?;
        }
        std::fs::rename(&tmp, &dirs.check_cache_path)?;
        Ok(())
    }

    /// 查询某条记录的最近校验结果。
    pub fn get(&self, id: &Uuid) -> Option<&CheckEntry> {
        self.0.get(id)
    }

    /// 写入/覆盖一条校验结果。
    pub fn put(&mut self, id: Uuid, status: CheckStatus, http: Option<u16>) {
        self.0.insert(
            id,
            CheckEntry {
                status,
                checked_at: Utc::now(),
                http,
            },
        );
    }
}
