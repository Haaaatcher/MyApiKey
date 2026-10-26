//! 同步引擎：fetch → 解密合并 → commit → push。
//!
//! 三个基本场景（详见 docs/design.md §5.2）：
//!
//! 1. **仅远端有新数据**（本地干净落后）：`git merge --ff-only origin/main`
//!    快进，工作区 vault 即为远端版本，无需产生新提交；
//! 2. **双方都有新数据**（分歧）：必然在二进制 vault 上冲突——
//!    这在预期之内。取出 origin/main 上的密文与本机数据，
//!    在**数据层**按 LWW+墓碑合并（见 [`crate::merge`]），
//!    重新加密写入并完成一个合并提交。git 只负责传输与历史，
//!    冲突语义完全由我们自己掌握；
//! 3. **仅本地有新数据**：直接 push。
//!
//! push 一律 best-effort：失败收集到 `push_error` 提示用户，
//! 不阻塞本地保存（ADR：保存即静默同步，失败仅提示）。
//!
//! git 认证完全交给用户环境（SSH key / credential manager），
//! 本模块不接触任何 git 凭据。

use crate::error::{Error, Result};
use crate::merge::merge_vaults;
use crate::model::VaultData;
use crate::paths::AppDirs;
use crate::store::Store;
use crate::vault;

/// 一次同步/保存的结果摘要，供 CLI 展示。
#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    /// 配置的 origin 远端地址（`None` = 纯单机模式）。
    pub remote: Option<String>,
    /// 是否从远端并入了新数据（快进或合并）。
    pub pulled: bool,
    /// 是否发生了双方分歧的数据层合并（区别于纯快进）。
    pub merged: bool,
    /// 是否成功推送到远端。
    pub pushed: bool,
    /// push 失败的原因（None = 成功或未尝试推送）。
    pub push_error: Option<String>,
    /// 操作结束时的存活记录数。
    pub alive_records: usize,
}

/// 保存数据到本机并提交；`auto_push` 为真且配置了远端时顺手推送（best-effort）。
///
/// 这是所有修改类命令（add/edit/delete/passwd/import）的统一出口：
/// 加密 → 写 `vault.mkv` → commit →（可选）push。
///
/// # Errors
/// 加密/写盘/commit 失败即返回错误（本地都不成功必须报错）；
/// 仅 push 失败被收进 `push_error` 不算错误。
pub fn save(
    store: &Store,
    dirs: &AppDirs,
    passphrase: &str,
    data: &VaultData,
    auto_push: bool,
) -> Result<SyncOutcome> {
    let mut out = SyncOutcome::default();
    let bytes = vault::encrypt(data, passphrase)?;
    // 原子写（tmp + rename，与 config.rs 同法）：直写中途崩溃/断电会留下
    // 半截 vault，加密文件损坏即数据全失——tmp 未 rename 完成前旧文件
    // 完好（2026-10-26 采纳审查建议；git 历史是第二道兜底，这里是第一道）
    let tmp = dirs.vault_path.with_extension("mkv.tmp");
    std::fs::write(&tmp, &bytes)?;
    if dirs.vault_path.exists() {
        std::fs::remove_file(&dirs.vault_path)?;
    }
    std::fs::rename(&tmp, &dirs.vault_path)?;
    store.commit_vault(count_alive(data))?;
    out.alive_records = count_alive(data);

    if auto_push && store.remote_url()?.is_some() {
        match store.push() {
            Ok(()) => out.pushed = true,
            Err(e) => out.push_error = Some(e.to_string()),
        }
    }
    Ok(out)
}

/// 执行一次完整同步（`mak sync` 的核心）。
///
/// 入参 `tombstone_days` 来自本机配置（[`crate::config::AppConfig`]）。
///
/// # Errors
/// - [`Error::NotInitialized`]：本机没有 vault 文件；
/// - [`Error::Decrypt`]：本机密码错误；
/// - [`Error::Usage`]：远端 vault 用了不同主密码（明确提示，两台设备必须同一密码）；
/// - git / IO 错误。
pub fn sync(
    store: &Store,
    dirs: &AppDirs,
    passphrase: &str,
    tombstone_days: u32,
) -> Result<SyncOutcome> {
    if !dirs.vault_path.is_file() {
        return Err(Error::NotInitialized);
    }

    // 1) 读入并解密本机数据（同时验证密码）
    let local_bytes = std::fs::read(&dirs.vault_path)?;
    let local_data = vault::decrypt(&local_bytes, passphrase)?;

    let mut out = SyncOutcome {
        alive_records: count_alive(&local_data),
        remote: store.remote_url()?,
        ..Default::default()
    };

    // 2) 纯单机模式：没有远端，无事可做
    if out.remote.is_none() {
        return Ok(out);
    }

    // 3) 拉取远端引用
    store
        .fetch()
        .map_err(|e| auth_hint(e, store.auth().is_some()))?;

    if store.remote_branch_exists() {
        let behind = store.behind_count();
        let ahead = store.ahead_count();

        if behind > 0 {
            if ahead == 0 {
                // 场景 1：干净落后 → 快进
                store.merge_ff_only()?;
                let remote_bytes = std::fs::read(&dirs.vault_path)?;
                // 远端内容若解不开，说明远端换了密码（或本机密码输错）
                let pulled_data =
                    vault::decrypt(&remote_bytes, passphrase).map_err(remote_password_error)?;
                out.pulled = true;
                out.alive_records = count_alive(&pulled_data);
            } else {
                // 场景 2：双方分歧 → 数据层合并
                let _conflicted = store.start_merge()?;
                let remote_bytes = store.remote_vault_bytes()?;
                let remote_data =
                    vault::decrypt(&remote_bytes, passphrase).map_err(remote_password_error)?;

                let merged = merge_vaults(
                    &local_data,
                    &remote_data,
                    chrono::Utc::now(),
                    tombstone_days,
                );
                let enc = vault::encrypt(&merged, passphrase)?;
                std::fs::write(&dirs.vault_path, &enc)?;
                store.commit_vault(count_alive(&merged))?;

                out.pulled = true;
                out.merged = true;
                out.alive_records = count_alive(&merged);
            }

            // 场景 3（或合并后）：本地有未推送提交 → 推送
            if store.ahead_count() > 0 {
                match store.push() {
                    Ok(()) => out.pushed = true,
                    Err(e) => {
                        out.push_error = Some(auth_hint(e, store.auth().is_some()).to_string())
                    }
                }
            }
        } else if store.ahead_count() > 0 {
            // 场景 3：仅本地有新数据 → 直接推送
            match store.push() {
                Ok(()) => out.pushed = true,
                Err(e) => out.push_error = Some(auth_hint(e, store.auth().is_some()).to_string()),
            }
        }
    } else {
        // 远端仓库还是空的（没有 main 分支）：场景 3 的边界——本地必然
        // 领先（vault 已提交），直接推送建分支。不补这个分支的话，
        // "绑定空仓库后靠 sync 推送"永远推不上去且不报任何错误。
        match store.push() {
            Ok(()) => out.pushed = true,
            Err(e) => out.push_error = Some(auth_hint(e, store.auth().is_some()).to_string()),
        }
    }

    Ok(out)
}

/// git 远端操作失败且本次带认证时，在错误后附加自愈指引。
///
/// GitHub 对私有仓库在凭据失效时统一报 "Repository not found"（避免泄露
/// 仓库存在性），用户无法从字面看出是 token 过期。此处提示重新授权命令。
/// 只有错误文本里出现这两类关键词才附加，避免误导无关错误。
fn auth_hint(e: Error, has_auth: bool) -> Error {
    const HINT: &str = ", your saved GitHub authorization may have expired. \
                        Run `mak init --github` to re-authorize";
    if !has_auth {
        return e;
    }
    let msg = e.to_string();
    if msg.contains("Repository not found") || msg.contains("Authentication failed") {
        Error::Usage(format!("{msg}{HINT}"))
    } else {
        e
    }
}

/// 远端 vault 解密失败时的错误转换：给出“两台设备必须同密码”的明确指引。
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

/// `mak status` 需要的仓库状态摘要。
#[derive(Debug, Clone)]
pub struct StatusInfo {
    /// origin 远端地址（None = 单机）。
    pub remote: Option<String>,
    /// 本地领先远端的提交数（未推送的变更）。
    pub ahead: usize,
    /// 本地落后远端的提交数（可 pull 的更新）。
    pub behind: usize,
}

/// 收集仓库状态摘要（只读操作，不需要密码）。
///
/// # Errors
/// git 调用异常（仓库损坏级别）。
pub fn status_summary(store: &Store) -> Result<StatusInfo> {
    Ok(StatusInfo {
        remote: store.remote_url()?,
        ahead: store.ahead_count(),
        behind: store.behind_count(),
    })
}

#[cfg(test)]
mod tests {
    //! 双设备同步的端到端集成测试：真实 git + 真实加密（不碰网络）。
    //!
    //! 拓扑：device_a --push/pull--> bare.git <--clone-- device_b
    //! bare 仓库即“远端”，用本地路径模拟 GitHub，git 本地传输无需任何凭据。

    use super::*;
    use crate::model::{Protocol, Record};

    const PASS: &str = "test-passphrase-1";

    /// 一台“设备”：独立目录 + Store + AppDirs。
    struct Device {
        store: Store,
        dirs: AppDirs,
    }

    fn device(tag: &str) -> Device {
        let base =
            std::env::temp_dir().join(format!("mak-sync-{tag}-{}", uuid::Uuid::now_v7().simple()));
        let repo_dir = base.join("repo");
        let vault_path = repo_dir.join(crate::store::VAULT_FILE);
        let config_path = base.join("config.toml");
        let check_cache_path = base.join("check_cache.json");
        let lock_path = base.join("vault.lock");
        std::fs::create_dir_all(&repo_dir).expect("创建仓库目录失败");
        let dirs = AppDirs {
            config_dir: base,
            repo_dir: repo_dir.clone(),
            vault_path,
            config_path,
            check_cache_path,
            lock_path,
            from_env: false,
        };
        let store = Store::new(&repo_dir);
        store.ensure_init().expect("git init 失败");
        // 测试仓库显式注入本地身份，保证与宿主机全局配置无关（hermetic）
        store.git_t(&["config", "user.email", "t@t"]);
        store.git_t(&["config", "user.name", "T"]);
        Device { store, dirs }
    }

    /// 测试辅助：暴露私有的 git 调用（仅测试内使用）。
    impl Store {
        fn git_t(&self, args: &[&str]) {
            use std::process::Command;
            let st = Command::new("git")
                .arg("-C")
                .arg(&self.repo_dir)
                .args(args)
                .status()
                .expect("git 执行失败");
            assert!(st.success(), "git {:?} 必须成功", args);
        }
        fn git_out(&self, args: &[&str]) -> String {
            use std::process::Command;
            let out = Command::new("git")
                .arg("-C")
                .arg(&self.repo_dir)
                .args(args)
                .output()
                .expect("git 执行失败");
            assert!(out.status.success(), "git {:?} 必须成功", args);
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
    }

    /// 组网：a 初始化并推送，建 bare 仓库作为远端，b 克隆接入。
    fn two_devices(tag: &str) -> (Device, Device) {
        let a = device(tag);
        // 初始 vault：一条记录
        let mut data = VaultData::empty();
        data.records.push(Record::new(
            "base".into(),
            Protocol::OpenAi,
            "https://a".into(),
            "sk-a".into(),
        ));
        save(&a.store, &a.dirs, PASS, &data, false).expect("初始保存失败");

        // bare 远端
        let bare =
            std::env::temp_dir().join(format!("mak-bare-{tag}-{}", uuid::Uuid::now_v7().simple()));
        a.store.git_t(&[
            "clone",
            "--bare",
            a.dirs.repo_dir.to_str().expect("路径非法"),
            bare.to_str().expect("路径非法"),
        ]);
        a.store
            .git_t(&["remote", "add", "origin", bare.to_str().expect("路径非法")]);
        a.store.git_t(&["push", "-u", "origin", "main"]);

        // b 克隆接入
        let b_base = std::env::temp_dir().join(format!(
            "mak-sync-b-{tag}-{}",
            uuid::Uuid::now_v7().simple()
        ));
        let b_repo = b_base.join("repo");
        let b_vault = b_repo.join(crate::store::VAULT_FILE);
        let b_config = b_base.join("config.toml");
        let b_cache = b_base.join("check_cache.json");
        let b_lock = b_base.join("vault.lock");
        std::fs::create_dir_all(&b_base).expect("创建目录失败");
        use std::process::Command;
        let st = Command::new("git")
            .arg("clone")
            .arg(bare.to_str().expect("路径非法"))
            .arg(b_repo.to_str().expect("路径非法"))
            .status()
            .expect("clone 执行失败");
        assert!(st.success(), "clone 必须成功");
        let b_dirs = AppDirs {
            config_dir: b_base,
            repo_dir: b_repo.clone(),
            vault_path: b_vault,
            config_path: b_config,
            check_cache_path: b_cache,
            lock_path: b_lock,
            from_env: false,
        };
        let b = Device {
            store: Store::new(&b_repo),
            dirs: b_dirs,
        };
        b.store.git_t(&["config", "user.email", "t@t"]);
        b.store.git_t(&["config", "user.name", "T"]);
        (a, b)
    }

    fn load(dev: &Device) -> VaultData {
        let bytes = std::fs::read(&dev.dirs.vault_path).expect("读 vault 失败");
        vault::decrypt(&bytes, PASS).expect("解密失败")
    }

    fn rec_named(name: &str) -> Record {
        Record::new(
            name.into(),
            Protocol::Anthropic,
            "https://x".into(),
            "sk-x".into(),
        )
    }

    #[test]
    fn fresh_init_device_with_remote_set_converges() {
        // 场景：设备 b 没走 clone 接入，而是先独立 init（独立根提交），
        // 之后才 remote set + sync —— 必须靠 --allow-unrelated-histories 收敛
        let a = device("unrel-a");
        let mut ad = VaultData::empty();
        ad.records.push(rec_named("shared"));
        save(&a.store, &a.dirs, PASS, &ad, false).expect("a 保存失败");

        let bare =
            std::env::temp_dir().join(format!("mak-bare-unrel-{}", uuid::Uuid::now_v7().simple()));
        a.store.git_t(&[
            "clone",
            "--bare",
            a.dirs.repo_dir.to_str().expect("路径非法"),
            bare.to_str().expect("路径非法"),
        ]);
        a.store
            .git_t(&["remote", "add", "origin", bare.to_str().expect("路径非法")]);
        a.store.git_t(&["push", "-u", "origin", "main"]);

        // b：完全独立的仓库（不同根提交），只做 set_remote
        let b = device("unrel-b");
        save(&b.store, &b.dirs, PASS, &VaultData::empty(), false).expect("b 保存失败");
        b.store
            .set_remote(bare.to_str().expect("路径非法"))
            .expect("设置远端失败");
        // 直接 push 必然被拒（non-fast-forward），这正是用户会遇到的路径
        assert!(b.store.push().is_err(), "独立历史的 push 必须失败");

        let s = sync(&b.store, &b.dirs, PASS, 30).expect("b sync 失败");
        assert!(s.merged, "不相关历史必须走数据层合并");
        let data_b = load(&b);
        let names: Vec<&str> = data_b.alive_records().map(|r| r.name.as_str()).collect();
        assert!(
            names.contains(&"shared"),
            "b 必须拿到 a 的记录，实际: {names:?}"
        );
    }

    #[test]
    fn offline_edits_on_both_sides_merge_and_converge() {
        let (a, b) = two_devices("diverge");

        // b 离线加一条 Y 并推送
        let mut bd = load(&b);
        bd.records.push(rec_named("Y"));
        save(&b.store, &b.dirs, PASS, &bd, true).expect("b 保存失败");

        // a 离线加一条 X；push 会因远端已前进而失败（best-effort，仅记录错误）
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut ad = load(&a);
        ad.records.push(rec_named("X"));
        let out = save(&a.store, &a.dirs, PASS, &ad, true).expect("a 保存失败");
        assert!(
            out.push_error.is_some(),
            "a 此时的 push 应失败（non-fast-forward）"
        );

        // a 执行 sync：发现分歧 → 数据层合并 → 推送
        let s = sync(&a.store, &a.dirs, PASS, 30).expect("a sync 失败");
        assert!(s.merged, "双方分歧必须走数据层合并");
        assert!(s.pushed, "合并后应推送成功");

        // b 再 sync：快进拉取，两边数据完全一致
        std::thread::sleep(std::time::Duration::from_millis(20));
        let sb = sync(&b.store, &b.dirs, PASS, 30).expect("b sync 失败");
        assert!(sb.pulled && !sb.merged, "b 应走快进路径");

        let data_a = load(&a);
        let names_a: Vec<&str> = data_a.alive_records().map(|r| r.name.as_str()).collect();
        let data_b = load(&b);
        let names_b: Vec<&str> = data_b.alive_records().map(|r| r.name.as_str()).collect();
        assert_eq!(names_a, names_b, "同步后两边记录必须一致");
        for expect in ["base", "X", "Y"] {
            assert!(
                names_a.contains(&expect),
                "记录 {expect} 必须存在: {names_a:?}"
            );
        }
    }

    #[test]
    fn deletion_tombstone_propagates_to_other_device() {
        let (a, b) = two_devices("tombstone");

        // a 删除 base 记录并同步
        let mut ad = load(&a);
        let idx = ad.find("base").expect("找到 base");
        ad.records[idx].mark_deleted();
        save(&a.store, &a.dirs, PASS, &ad, true).expect("a 删除保存失败");

        // b 同步：拿到墓碑，base 消失
        std::thread::sleep(std::time::Duration::from_millis(20));
        sync(&b.store, &b.dirs, PASS, 30).expect("b sync 失败");
        let data_b = load(&b);
        let names_b: Vec<&str> = data_b.alive_records().map(|r| r.name.as_str()).collect();
        assert!(
            !names_b.contains(&"base"),
            "删除必须传播到 b，实际: {names_b:?}"
        );
    }

    #[test]
    fn sync_without_remote_is_noop_success() {
        let a = device("solo");
        let mut ad = VaultData::empty();
        ad.records.push(rec_named("only"));
        save(&a.store, &a.dirs, PASS, &ad, false).expect("保存失败");

        let s = sync(&a.store, &a.dirs, PASS, 30).expect("单机 sync 失败");
        assert!(!s.pulled && !s.pushed && s.remote.is_none());
        assert_eq!(s.alive_records, 1);
    }

    #[test]
    fn status_summary_counts() {
        let (a, _b) = two_devices("status");
        let info = status_summary(&a.store).expect("状态查询失败");
        assert!(info.remote.is_some());
        assert_eq!(info.behind, 0, "刚推送完不应落后");
        assert_eq!(info.ahead, 0, "刚推送完不应领先");

        // a 本地再保存一条（不推送）→ ahead = 1
        let mut ad = load(&a);
        ad.records.push(rec_named("new"));
        save(&a.store, &a.dirs, PASS, &ad, false).expect("保存失败");
        let info = status_summary(&a.store).expect("状态查询失败");
        assert_eq!(info.ahead, 1);
    }

    #[test]
    fn git_history_visible() {
        // 验证“本地即 git 仓库”：历史可查，提交信息为机器格式
        let (a, _b) = two_devices("history");
        let log = a.store.git_out(&["log", "--oneline"]);
        assert!(
            log.contains("sync:"),
            "提交信息必须是 sync: 机器格式，实际: {log}"
        );
    }
}
