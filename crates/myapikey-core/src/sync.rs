//! 同步引擎（D25）：GitHub Contents API 直连，单文件 CAS 同步。
//!
//! 取代 git 协议（M4）。远端状态 = 一个 blob sha；本地状态 = vault
//! 密文 + config 的 `last_sync_sha`（与远端一致的最后时点）与
//! `push_pending`（本地有未推送修改）。三个基本场景：
//!
//! 1. **仅远端有新数据**（本地干净）：下载远端密文替换本地；
//! 2. **双方都有新数据**（远端 sha ≠ last_sync_sha 且本地 dirty）：
//!    取远端密文与本地数据在**数据层**按 LWW+墓碑合并
//!    （[`crate::merge`]），上传合并结果；上传遇 409（同步期间远端
//!    又变了）→ 重拉重合并（有限轮次）；
//! 3. **仅本地有新数据**：直接上传。
//!
//! 上传 best-effort：保存路径上的失败收进 `push_error` 不阻塞本地保存。
//! 每次 PUT 在远端产生一个 git commit（message 透传），GitHub 网页
//! 历史照旧可见。
//!
//! 本地无 git 仓库：`<config_dir>/repo/.git`（旧版遗留）在首次同步时
//! 清理（历史在远端仓库，本地纯冗余）。

use crate::config::AppConfig;
use crate::error::{Error, Result};
use crate::merge::merge_vaults;
use crate::model::VaultData;
use crate::paths::AppDirs;
use crate::remote::{GitHubStore, RemoteHead, SyncStore};
use crate::vault;

/// 一次同步/保存的结果摘要，供 CLI/GUI 展示。
#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    /// 配置的 origin 远端地址（`None` = 纯单机模式）。
    pub remote: Option<String>,
    /// 是否从远端并入了新数据（下载或合并）。
    pub pulled: bool,
    /// 是否发生了双方分歧的数据层合并（区别于纯下载）。
    pub merged: bool,
    /// 是否成功推送到远端。
    pub pushed: bool,
    /// push 失败的原因（None = 成功或未尝试推送）。
    pub push_error: Option<String>,
    /// 操作结束时的存活记录数。
    pub alive_records: usize,
}

/// 从 config 的 remote 地址与 token 组装 GitHub 同步通道。
///
/// 返回 `None` = 未绑定远端（纯单机）。
///
/// # Errors
/// remote 地址不是 GitHub 形态。
pub fn open_remote(url: &str, token: &str) -> Result<Box<dyn SyncStore>> {
    Ok(Box::new(GitHubStore {
        token: token.to_string(),
        repo: crate::remote::parse_github_repo(url)?,
    }))
}

/// 原子写 vault（tmp + rename）：直写中途崩溃会留半截密文即数据全失；
/// tmp 写完再 rename，任意时刻崩溃旧文件都完好（rename 原子替换目标——
/// Windows 上是 MoveFileExW(REPLACE_EXISTING)，无需也不能先删旧文件，
/// 2026-10-31 审查修复：此前的 remove+rename 在两步之间留下"旧文件已
/// 删、新文件未就位"的非原子窗口，崩溃/掉电即整库丢失）。父目录不存
/// 在则先建（全新机器接入远端数据时 `repo/` 尚未创建——缺这一步会在
/// os error 3 上炸掉）。
pub fn atomic_write_vault(dirs: &AppDirs, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dirs.vault_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dirs.vault_path.with_extension("mkv.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &dirs.vault_path)?;
    Ok(())
}

/// 同步记账（数据层唯一写入口）：任何"本地与远端达成一致"的时刻——
/// 上传成功、下载替换、接入完成——都必须经本函数写 config 的同步
/// 状态三件套（`last_sync_sha` / `push_pending=false` / `last_sync_at`）
/// 并落盘。禁止调用方各自手写字段：状态字段一旦散落多处手写，必有
/// 遗漏（GUI 接入后显示 never 的根因，2026-10-31 修复）。
pub fn mark_synced(dirs: &AppDirs, cfg: &mut AppConfig, sha: String) {
    cfg.last_sync_sha = Some(sha);
    cfg.push_pending = false;
    cfg.last_sync_at = Some(chrono::Utc::now().to_rfc3339());
    // best-effort 落盘（2026-10-31 审查可选采纳，注释固化语义）：此刻
    // 数据本体已保存成功，记账写盘失败（磁盘满等）只影响下次 sync 的
    // 差异判断——最坏多推一次幂等副本，报错反而会让"同步成功"误报
    // 为失败。由调用方决定是否感知：本函数刻意不返回 Result。
    let _ = cfg.save(dirs);
}

/// 解绑远端（数据层唯一写入口，与 [`mark_synced`] 对称的另一半）：
/// `remote_repo = None` 并把同步记账三件套一并归零后落盘。任何解绑
/// 语义的出口——CLI `remote unset`、GUI 忘记授权——都必须经本函数，
/// 禁止各自手写字段（解绑漏清 `last_sync_*` 会出现"单机模式却有上次
/// 同步时间"的矛盾显示，2026-10-31 修复）。
///
/// 只动 config 与记账；vault 文件与远端仓库由调用方决定去留，本函数
/// 不碰。
pub fn unbind_remote(dirs: &AppDirs, cfg: &mut AppConfig) {
    cfg.remote_repo = None;
    cfg.last_sync_sha = None;
    cfg.push_pending = false;
    cfg.last_sync_at = None;
    let _ = cfg.save(dirs);
}

/// 上传一个版本并记账（config 状态三件套）。CAS 冲突原样上抛给调用方。
fn push_one(
    remote: &dyn SyncStore,
    dirs: &AppDirs,
    cfg: &mut AppConfig,
    bytes: &[u8],
    expect: Option<String>,
    alive: usize,
) -> Result<String> {
    // expect = 调用方声明的远端状态（CAS 基准），由调用方按场景给出
    let message = format!(
        "sync: {}, {} records",
        chrono::Utc::now().to_rfc3339(),
        alive
    );
    let sha = remote.put_blob(&message, bytes, expect.as_deref())?;
    mark_synced(dirs, cfg, sha.clone());
    Ok(sha)
}

/// 已初始化语境下的公开上传+记账组合（init 自身就是同步点，绕过
/// [`save_local`] 的置位语义）：PUT → `mark_synced` 记账三件套。
/// CLI/GUI 的 init 收尾统一走本入口——记账唯一写入口约定（见
/// [`mark_synced`]）同样适用于这条路径，调用方不得手写字段
/// （2026-10-31 审查：CLI push_initialized 曾手写三件套）。
///
/// # Errors
/// CAS 冲突 / 网络失败原样上抛（记账不会发生）。
pub fn push_and_mark(
    remote: &dyn SyncStore,
    dirs: &AppDirs,
    cfg: &mut AppConfig,
    bytes: &[u8],
    expect: Option<String>,
    alive: usize,
) -> Result<String> {
    push_one(remote, dirs, cfg, bytes, expect, alive)
}

/// 保存数据到本机并（按配置）best-effort 推送。
///
/// 这是所有修改类命令（add/edit/delete/passwd）的统一出口：
/// 加密 → 原子写 `vault.mkv` → 置 `push_pending` →（可选）上传。
///
/// # Errors
/// 加密/写盘失败即返回错误（本地都不成功必须报错）；
/// 仅上传失败被收进 `push_error` 不算错误。
/// 阶段 1（保存）：加密 → 原子写 vault → 置"有未推送修改"。
///
/// 独立于推送（阶段 2）——保存成功但同步失败是完全可用的状态
/// （push_pending 标记待推，sync 补推），两者的感知与重试解耦。
///
/// 返回密文（供调用方阶段 2 上传复用，避免重复加密）。
///
/// # Errors
/// 加密/写盘失败。
pub fn save_local(
    dirs: &AppDirs,
    passphrase: &str,
    data: &VaultData,
    cfg: &mut AppConfig,
) -> Result<Vec<u8>> {
    let bytes = vault::encrypt(data, passphrase)?;
    // 先持久化"有未推送修改"再写 vault：config 写盘失败（磁盘满等）
    // 只会让下次 sync 多推一次相同内容（幂等无害）；反过来（vault 已写
    // 而 pending 丢失）会静默丢同步。vault 写失败则报错返回，多置位的
    // pending 同样幂等无害。
    cfg.push_pending = true;
    cfg.save(dirs)?;
    atomic_write_vault(dirs, &bytes)?;
    Ok(bytes)
}

pub fn save(
    dirs: &AppDirs,
    passphrase: &str,
    data: &VaultData,
    remote: Option<&dyn SyncStore>,
    auto_push: bool,
    cfg: &mut AppConfig,
) -> Result<SyncOutcome> {
    let mut out = SyncOutcome {
        remote: Some(cfg_remote_label(cfg)),
        ..Default::default()
    };
    let bytes = save_local(dirs, passphrase, data, cfg)?;
    out.alive_records = count_alive(data);

    if let (true, Some(r)) = (auto_push, remote) {
        match push_one(
            r,
            dirs,
            cfg,
            &bytes,
            cfg.last_sync_sha.clone(),
            out.alive_records,
        ) {
            Ok(_new_sha) => out.pushed = true,
            Err(Error::RemoteConflict) => {
                out.push_error = Some(
                    "the remote vault has new changes; run `mak sync` to merge and push".into(),
                );
            }
            Err(e) => out.push_error = Some(e.to_string()),
        }
    }
    Ok(out)
}

/// config 里的远端地址（status/摘要展示用）。
fn cfg_remote_label(cfg: &AppConfig) -> String {
    cfg.remote_repo.clone().unwrap_or_default()
}

/// 执行一次完整同步（`mak sync` / GUI 同步按钮的核心）。
///
/// 入参 `tombstone_days` 来自本机配置。
///
/// # Errors
/// - [`Error::NotInitialized`]：本机没有 vault 文件；
/// - [`Error::Decrypt`]：本机密码错误；
/// - [`Error::Usage`]：远端 vault 用了不同主密码（两台设备必须同一密码）；
/// - 网络/认证错误。
pub fn sync(
    dirs: &AppDirs,
    passphrase: &str,
    remote: &dyn SyncStore,
    tombstone_days: u32,
    cfg: &mut AppConfig,
) -> Result<SyncOutcome> {
    migrate_legacy_git(dirs, cfg);

    if !dirs.vault_path.is_file() {
        return Err(Error::NotInitialized);
    }
    let local_bytes = std::fs::read(&dirs.vault_path)?;
    let mut local_data = vault::decrypt(&local_bytes, passphrase)?;

    let mut out = SyncOutcome {
        alive_records: count_alive(&local_data),
        remote: Some(cfg_remote_label(cfg)),
        ..Default::default()
    };

    // 远端当前状态（None = 空仓库）
    let mut head: Option<RemoteHead> = remote.fetch_head()?;
    let mut attempts = 0usize;

    loop {
        let local_dirty = cfg.push_pending || cfg.last_sync_sha.is_none();
        match (local_dirty, &head) {
            // 本地干净 + 远端未变：无事（幂等——上次 PUT 成功但记账未及
            // 写盘时，远端内容 == 本地内容，下次按 behind 重下也无害）。
            // 已达成一致同样经唯一记账入口：刷新 last_sync_at（"上次同步"
            // 反映最近一次对账时刻），sha/pending 幂等不变
            (false, Some(h)) if Some(h.sha.as_str()) == cfg.last_sync_sha.as_deref() => {
                mark_synced(dirs, cfg, h.sha.clone());
            }
            // 本地干净 + 远端新：下载替换
            (false, Some(h)) => {
                let pulled_data =
                    vault::decrypt(&h.bytes, passphrase).map_err(remote_password_error)?;
                atomic_write_vault(dirs, &h.bytes)?;
                mark_synced(dirs, cfg, h.sha.clone());
                out.pulled = true;
                out.alive_records = count_alive(&pulled_data);
            }
            // 本地干净 + 远端无文件（仓库被删重建）：以本地为准创建
            (false, None) => {
                let bytes = std::fs::read(&dirs.vault_path)?;
                push_one(remote, dirs, cfg, &bytes, None, out.alive_records)?;
                out.pushed = true;
            }
            // 本地 dirty + 远端无文件：直接创建
            (true, None) => {
                let bytes = vault::encrypt(&local_data, passphrase)?;
                push_one(remote, dirs, cfg, &bytes, None, count_alive(&local_data))?;
                out.pushed = true;
            }
            // 本地 dirty + 远端未变：直接上传
            (true, Some(h)) if Some(h.sha.as_str()) == cfg.last_sync_sha.as_deref() => {
                let bytes = vault::encrypt(&local_data, passphrase)?;
                push_one(
                    remote,
                    dirs,
                    cfg,
                    &bytes,
                    Some(h.sha.clone()),
                    count_alive(&local_data),
                )?;
                out.pushed = true;
            }
            // 双方都有新数据（或首次迁移建基线）：数据层 LWW 合并
            (true, Some(h)) => {
                let remote_data =
                    vault::decrypt(&h.bytes, passphrase).map_err(remote_password_error)?;
                let merged = merge_vaults(
                    &local_data,
                    &remote_data,
                    chrono::Utc::now(),
                    tombstone_days,
                );
                let bytes = vault::encrypt(&merged, passphrase)?;
                atomic_write_vault(dirs, &bytes)?;
                local_data = merged;
                let merged_alive = count_alive(&local_data);
                match push_one(remote, dirs, cfg, &bytes, Some(h.sha.clone()), merged_alive) {
                    Ok(_new_sha) => {
                        out.pulled = true;
                        out.merged = true;
                        out.alive_records = merged_alive;
                        out.pushed = true;
                    }
                    // 同步期间远端又变了（另一台设备抢先上传）：重拉重合并
                    Err(Error::RemoteConflict) => {
                        attempts += 1;
                        if attempts >= 3 {
                            out.push_error = Some(
                                "the remote kept changing during sync; try again in a moment"
                                    .into(),
                            );
                            break;
                        }
                        head = remote.fetch_head()?;
                        continue;
                    }
                    Err(e) => {
                        out.push_error = Some(e.to_string());
                        break;
                    }
                }
            }
        }
        break;
    }

    Ok(out)
}

/// 旧版（git 同步时代）遗留的一次性迁移：远端绑定在 git config 里而
/// 新架构存应用 config——删除 `.git` 前先把 remote 地址抢救出来，
/// 否则老用户升级后同步会静默失效（表现为"没绑远端"）。
///
/// 迁移读取用一次性 git 子进程调用（本模块唯一的 spawn——仅服务
/// 存量迁移，新装机器 `.git` 不存在自然跳过；未来版本可移除）。
fn migrate_legacy_git(dirs: &AppDirs, cfg: &mut AppConfig) {
    let git_dir = dirs.repo_dir.join(".git");
    if !git_dir.is_dir() {
        return;
    }
    // config 已有绑定（可能用户在新版里改过）：以新值为准，直接清理
    if cfg.remote_repo.is_none() {
        let url = std::process::Command::new("git")
            .args([
                "-C",
                &dirs.repo_dir.to_string_lossy(),
                "remote",
                "get-url",
                "origin",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|u| !u.is_empty());
        if let Some(Ok(repo)) = url.as_deref().map(crate::remote::parse_github_repo) {
            cfg.remote_repo = Some(repo);
            let _ = cfg.save(dirs);
        }
    }
    let _ = std::fs::remove_dir_all(&git_dir);
}

/// 判断错误文本是否属于"认证/授权失败"（HTTP 401/403 的结构化
/// 文本，见 remote.rs）。供 CLI/GUI 的"token 静默续期重试"共用。
/// 判定是否认证类错误（触发刷新令牌静默重试一次）：401 之外，
/// 令牌被 SSO/权限撤销时 GitHub 也可能回 403——`auth_or_limit` 已把
/// 限流单独转成不含 "HTTP 403" 的文案，不会误入本判定
/// （2026-10-31 审查可选采纳）。
pub fn is_auth_error(msg: &str) -> bool {
    msg.contains("GitHub authorization failed")
        || msg.contains("HTTP 401")
        || msg.contains("HTTP 403")
        || msg.contains("Bad credentials")
}

/// 远端 vault 解密失败时的错误转换：给出"两台设备必须同密码"的明确指引。
fn remote_password_error(e: Error) -> Error {
    match e {
        Error::Decrypt => Error::Usage(
            "failed to decrypt the remote vault: it was encrypted with a different master \
             passphrase (or the local one is wrong). All devices must use the same master \
             passphrase"
                .into(),
        ),
        other => other,
    }
}

/// 存活记录数（墓碑不计）。
fn count_alive(d: &VaultData) -> usize {
    d.records.iter().filter(|r| !r.is_deleted()).count()
}

/// `mak status` / GUI 状态页需要的同步摘要（纯本地，不联网）。
#[derive(Debug, Clone)]
pub struct StatusInfo {
    /// origin 远端地址（None = 单机）。
    pub remote: Option<String>,
    /// 本地有未推送的修改（保存过但尚未成功上传）。
    pub pending: bool,
    /// 最近一次成功同步时间（RFC 3339）。
    pub last_sync_at: Option<String>,
}

/// 收集同步状态摘要（只读，不需要密码，不联网——远端是否有新数据
/// 由 `sync` 联网判断）。
///
/// # Errors
/// config 读取失败。
pub fn status_summary(_dirs: &AppDirs, cfg: &AppConfig) -> Result<StatusInfo> {
    Ok(StatusInfo {
        remote: cfg.remote_repo.clone(),
        pending: cfg.push_pending,
        last_sync_at: cfg.last_sync_at.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Protocol, Record};
    use crate::remote::mem::MemStore;
    use std::rc::Rc;

    fn dirs_at(tag: &str) -> (AppDirs, AppConfig) {
        let root = std::env::temp_dir().join(format!("mak-sync-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root.join("repo")).unwrap();
        let dirs = AppDirs {
            config_dir: root.clone(),
            repo_dir: root.join("repo"),
            vault_path: root.join("repo/vault.mkv"),
            config_path: root.join("config.toml"),
            check_cache_path: root.join("check_cache.json"),
            lock_path: root.join("vault.lock"),
            from_env: true,
        };
        (dirs, AppConfig::default())
    }

    fn vault_with(name: &str, updated_min_ago: i64) -> VaultData {
        let mut d = VaultData::default();
        let mut r = Record::new(
            name.to_string(),
            Protocol::OpenAi,
            "https://x.example".into(),
            format!("key-{name}"),
        );
        r.updated_at = chrono::Utc::now() - chrono::Duration::minutes(updated_min_ago);
        d.records.push(r);
        d
    }

    fn digest(d: &VaultData) -> String {
        d.records
            .iter()
            .map(|r| r.name.clone())
            .collect::<Vec<_>>()
            .join(",")
    }

    fn load(dirs: &AppDirs, pw: &str) -> VaultData {
        vault::decrypt(&std::fs::read(&dirs.vault_path).unwrap(), pw).unwrap()
    }

    /// 场景 1：本地干净、远端新 → 下载替换（pulled，不 merged）。
    #[test]
    fn behind_downloads_remote() {
        let (dirs, mut cfg) = dirs_at("behind");
        let mem = Rc::new(MemStore::default());
        let remote_vault = vault_with("from-remote", 10);
        let bytes = vault::encrypt(&remote_vault, "pw").unwrap();
        mem.0.borrow_mut().sha = "sha-r1".into();
        mem.0.borrow_mut().bytes = bytes.clone();

        let local = vault_with("local-old", 30);
        let lb = vault::encrypt(&local, "pw").unwrap();
        std::fs::write(&dirs.vault_path, &lb).unwrap();
        cfg.last_sync_sha = Some("sha-stale".into());

        let out = sync(&dirs, "pw", mem.as_ref(), 30, &mut cfg).unwrap();
        assert!(out.pulled && !out.merged && !out.pushed);
        assert_eq!(digest(&load(&dirs, "pw")), "from-remote");
        assert_eq!(cfg.last_sync_sha.as_deref(), Some("sha-r1"));
        assert!(!cfg.push_pending);
    }

    /// 场景 2：双方都有新数据 → LWW 合并 + 上传。
    #[test]
    fn diverged_merges_lww_and_pushes() {
        let (dirs, mut cfg) = dirs_at("diverge");
        let mem = Rc::new(MemStore::default());
        let remote_vault = vault_with("remote-new", 10);
        let bytes = vault::encrypt(&remote_vault, "pw").unwrap();
        mem.0.borrow_mut().sha = "sha-r2".into();
        mem.0.borrow_mut().bytes = bytes;
        let local = vault_with("local-new", 5);
        let lb = vault::encrypt(&local, "pw").unwrap();
        std::fs::write(&dirs.vault_path, &lb).unwrap();
        cfg.last_sync_sha = Some("sha-r1".into());
        cfg.push_pending = true;

        let out = sync(&dirs, "pw", mem.as_ref(), 30, &mut cfg).unwrap();
        assert!(out.merged && out.pushed);
        let merged = load(&dirs, "pw");
        assert!(merged.records.iter().any(|r| r.name == "remote-new"));
        assert!(merged.records.iter().any(|r| r.name == "local-new"));
        assert!(!cfg.push_pending);
        assert_eq!(mem.0.borrow().history.len(), 1);
    }

    /// 场景 3：仅本地新 → 直接上传。
    #[test]
    fn local_only_pushes() {
        let (dirs, mut cfg) = dirs_at("push-only");
        let mem = Rc::new(MemStore::default());
        let base = vault_with("base", 60);
        let bb = vault::encrypt(&base, "pw").unwrap();
        mem.0.borrow_mut().sha = "sha-base".into();
        mem.0.borrow_mut().bytes = bb.clone();
        std::fs::write(&dirs.vault_path, &bb).unwrap();
        cfg.last_sync_sha = Some("sha-base".into());
        cfg.push_pending = true;

        let out = sync(&dirs, "pw", mem.as_ref(), 30, &mut cfg).unwrap();
        assert!(out.pushed && !out.pulled && !out.merged);
        assert!(!cfg.push_pending);
    }

    /// 已是最新也刷新记账时间：本地干净 + 远端未变 → 无数据动作，
    /// 但 last_sync_at 走 mark_synced 刷新（"上次同步"= 最近一次对账
    /// 时刻），sha 与 pending 幂等不变。CLI/GUI 尾部的手写记账已删，
    /// 本用例锁死该场景仍会更新时间（2026-10-31 回归）。
    #[test]
    fn up_to_date_refreshes_last_sync_at() {
        let (dirs, mut cfg) = dirs_at("uptodate");
        let mem = Rc::new(MemStore::default());
        let bytes = vault::encrypt(&vault_with("a", 5), "pw").unwrap();
        mem.0.borrow_mut().sha = "sha-base".into();
        mem.0.borrow_mut().bytes = bytes.clone();
        std::fs::write(&dirs.vault_path, &bytes).unwrap();
        cfg.last_sync_sha = Some("sha-base".into());
        cfg.last_sync_at = Some("2026-01-01T00:00:00+00:00".into());
        cfg.push_pending = false;

        let out = sync(&dirs, "pw", mem.as_ref(), 30, &mut cfg).unwrap();

        assert!(!out.pulled && !out.pushed && !out.merged, "无事发生");
        assert_eq!(cfg.last_sync_sha.as_deref(), Some("sha-base"), "sha 不变");
        assert!(!cfg.push_pending);
        let at = cfg.last_sync_at.as_deref().unwrap();
        assert_ne!(at, "2026-01-01T00:00:00+00:00", "对账时刻已刷新");
        // 落盘验证
        let re = AppConfig::load(&dirs).unwrap();
        assert_eq!(re.last_sync_at.as_deref(), Some(at));
    }

    /// 同步期间远端又变了（409 竞态）→ 重拉重合并，最终收敛。
    #[test]
    fn conflict_retries_and_converges() {
        let (dirs, mut cfg) = dirs_at("conflict");
        let mem = Rc::new(MemStore::default());
        let remote_vault = vault_with("remote-new", 10);
        let bytes = vault::encrypt(&remote_vault, "pw").unwrap();
        mem.0.borrow_mut().sha = "sha-r2".into();
        mem.0.borrow_mut().bytes = bytes;
        let local = vault_with("local-new", 5);
        let lb = vault::encrypt(&local, "pw").unwrap();
        std::fs::write(&dirs.vault_path, &lb).unwrap();
        cfg.last_sync_sha = Some("sha-r1".into());
        cfg.push_pending = true;
        mem.0.borrow_mut().conflict_next = true;

        let out = sync(&dirs, "pw", mem.as_ref(), 30, &mut cfg).unwrap();
        assert!(out.merged && out.pushed, "push_error={:?}", out.push_error);
        let merged = load(&dirs, "pw");
        assert!(merged.records.iter().any(|r| r.name == "remote-new"));
        assert!(merged.records.iter().any(|r| r.name == "local-new"));
    }

    /// 旧版迁移：repo/.git 存在且 config 无 remote → 抢救 git remote
    /// 地址到 config.remote_repo 后清理 .git；config 已有绑定则以新值为
    /// 准（不覆盖）。迁移后 is_initialized 仍为真（vault 在）。
    #[test]
    fn legacy_git_remote_migrated_then_cleaned() {
        use std::process::Command;

        let (dirs, mut cfg) = dirs_at("migrate");
        // 造一个带 origin 的旧版仓库（git 可执行在 CI/开发机均存在；
        // 本测试是迁移路径的唯一覆盖，依赖 git 是可接受的）
        let st = Command::new("git")
            .args(["init", "-b", "main", &dirs.repo_dir.to_string_lossy()])
            .output()
            .expect("git init");
        assert!(st.status.success());
        let st = Command::new("git")
            .args([
                "-C",
                &dirs.repo_dir.to_string_lossy(),
                "remote",
                "add",
                "origin",
                "https://github.com/someone/vault.git",
            ])
            .output()
            .expect("git remote add");
        assert!(st.status.success());

        migrate_legacy_git(&dirs, &mut cfg);

        assert_eq!(
            cfg.remote_repo.as_deref(),
            Some("someone/vault"),
            "remote 地址应被抢救并归一化"
        );
        assert!(!dirs.repo_dir.join(".git").exists(), ".git 应被清理");

        // config 已有绑定时不覆盖
        let _ = Command::new("git")
            .args(["init", &dirs.repo_dir.to_string_lossy()])
            .output();
        let _ = Command::new("git")
            .args([
                "-C",
                &dirs.repo_dir.to_string_lossy(),
                "remote",
                "add",
                "origin",
                "https://github.com/other/vault.git",
            ])
            .output();
        cfg.remote_repo = Some("someone/vault".into());
        migrate_legacy_git(&dirs, &mut cfg);
        assert_eq!(
            cfg.remote_repo.as_deref(),
            Some("someone/vault"),
            "已有绑定不被覆盖"
        );
        assert!(!dirs.repo_dir.join(".git").exists());
    }

    /// mark_synced 三件套原子记账：sha / pending=false / at 非空同落
    /// config——手写记账漏字段（绑定即同步却显示 never）的回归锁。
    #[test]
    fn mark_synced_writes_all_three_fields() {
        let (dirs, mut cfg) = dirs_at("mark");
        cfg.push_pending = true;
        std::fs::create_dir_all(&dirs.config_dir).unwrap();
        mark_synced(&dirs, &mut cfg, "sha-x".into());
        assert_eq!(cfg.last_sync_sha.as_deref(), Some("sha-x"));
        assert!(!cfg.push_pending);
        assert!(cfg.last_sync_at.is_some());
        // 落盘验证：重新加载仍能看到
        let re = AppConfig::load(&dirs).unwrap();
        assert_eq!(re.last_sync_sha.as_deref(), Some("sha-x"));
    }

    /// 解绑与记账对称：`unbind_remote` 清 `remote_repo` + 记账三件套，
    /// 落盘后重新加载仍是干净的单机状态——否则解绑后状态页出现
    /// "单机模式却有上次同步时间"的矛盾显示（2026-10-31 回归）。
    #[test]
    fn unbind_remote_clears_repo_and_sync_state() {
        let (dirs, mut cfg) = dirs_at("unbind");
        cfg.remote_repo = Some("you/myapikey-vault".into());
        std::fs::create_dir_all(&dirs.config_dir).unwrap();
        mark_synced(&dirs, &mut cfg, "sha-x".into());
        assert!(cfg.last_sync_at.is_some(), "前置：先有同步记账");

        unbind_remote(&dirs, &mut cfg);

        assert!(cfg.remote_repo.is_none());
        assert!(cfg.last_sync_sha.is_none());
        assert!(cfg.last_sync_at.is_none());
        assert!(!cfg.push_pending);
        // 落盘验证：重新加载也干净
        let re = AppConfig::load(&dirs).unwrap();
        assert!(re.remote_repo.is_none());
        assert!(re.last_sync_at.is_none());
        assert!(re.last_sync_sha.is_none());
    }

    /// 原子写在父目录（repo/）不存在时自动建目录——全新机器接入远端
    /// 数据时 `repo/` 尚未创建（2026-10-30 os error 3 回归：init 接入
    /// 分支裸写未建目录）。
    #[test]
    fn atomic_write_creates_missing_parent_dir() {
        let (dirs, mut _cfg) = dirs_at("atomic-parent");
        // 模拟"目录刚被整体删除"的全新机器
        std::fs::remove_dir_all(&dirs.repo_dir).unwrap();

        atomic_write_vault(&dirs, b"content").unwrap();

        assert_eq!(
            std::fs::read(&dirs.vault_path).unwrap(),
            b"content",
            "vault 应已落盘（含父目录自动创建）"
        );
    }

    /// 空仓库 + 本地有数据 → 创建远端文件。
    #[test]
    fn empty_remote_creates_file() {
        let (dirs, mut cfg) = dirs_at("empty-remote");
        let mem = Rc::new(MemStore::default());
        let local = vault_with("solo", 1);
        let lb = vault::encrypt(&local, "pw").unwrap();
        std::fs::write(&dirs.vault_path, &lb).unwrap();
        cfg.push_pending = true;

        let out = sync(&dirs, "pw", mem.as_ref(), 30, &mut cfg).unwrap();
        assert!(out.pushed);
        assert!(mem.0.borrow().sha.starts_with("sha-"));
    }
}
