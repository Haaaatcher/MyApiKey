//! 本地 git 仓库管理：通过 shell 调用系统 `git`（ADR D8）。
//!
//! 设计要点：
//! - 本地 `repo/` 目录从 `init` 起就是一个完整的 git 仓库——
//!   不配远端也有本地历史，纯单机使用与多机同步是同一条代码路径；
//! - git 认证默认依赖用户环境（SSH key / credential manager）；
//!   当本机存有 GitHub token（`mak init --github` 流程，ADR D13）时，
//!   通过 [`Store::with_auth`] 以环境变量注入 Authorization 头，
//!   不进命令行参数、不落 `.git/config`；
//! - 所有 git 调用集中在本模块，统一 `git -C <repo>` 前缀与错误包装，
//!   错误信息附带 stderr 摘要，方便用户进仓库目录手动救急。
//!
//! 仓库内容约定：仓库根下只有一个实质文件 `vault.mkv`（密文）。
//! commit message 使用固定机器格式 `sync: <RFC3339 时间>, <N> records`，
//! 不携带任何业务信息（记录名等）。

use crate::error::{Error, Result};
use crate::github::GitAuth;
use std::path::PathBuf;
use std::process::Command;

/// vault 文件在仓库内的相对路径。
pub const VAULT_FILE: &str = "vault.mkv";

/// 本地 git 仓库的薄封装。
#[derive(Debug, Clone)]
pub struct Store {
    /// git 仓库（工作副本）目录。
    pub repo_dir: PathBuf,
    /// 可选的 git HTTP 认证（见模块文档）；`None` = 完全交给用户 git 环境。
    auth: Option<GitAuth>,
}

impl Store {
    /// 用仓库目录构造。目录是否存在由 [`Store::ensure_init`] 处理。
    pub fn new(repo_dir: impl Into<PathBuf>) -> Self {
        Self {
            repo_dir: repo_dir.into(),
            auth: None,
        }
    }

    /// 附加 git HTTP 认证（builder 风格）。
    pub fn with_auth(mut self, auth: GitAuth) -> Self {
        self.auth = Some(auth);
        self
    }

    /// 当前认证的只读引用（`None` = 未附加）。
    pub fn auth(&self) -> Option<&GitAuth> {
        self.auth.as_ref()
    }

    /// 把认证（如有）以环境变量形式挂到 git 命令上。
    fn apply_auth(&self, cmd: &mut Command) {
        if let Some(a) = &self.auth {
            cmd.envs(a.git_envs());
        }
    }

    /// 在仓库目录中执行一条 git 命令，返回 stdout（已去尾部换行）。
    ///
    /// # Errors
    /// - [`Error::GitUnavailable`]：找不到 git 可执行文件；
    /// - [`Error::Git`]：命令退出码非零，信息含命令与 stderr 摘要（截断到 500 字符，
    ///   防止把用户刷屏）。
    fn git(&self, args: &[&str]) -> Result<String> {
        self.git_allow_fail(args).and_then(|(code, out, err)| {
            if code == 0 {
                Ok(out)
            } else {
                Err(Error::Git(format!(
                    "`git {}` failed (exit code {}): {}",
                    args.join(" "),
                    code,
                    truncate(&err, 500)
                )))
            }
        })
    }

    /// 执行 git 命令但**不**把非零退出码当错误，返回 (code, stdout, stderr)。
    /// 合并冲突、rev-parse 探测等场景需要区分退出码。
    fn git_allow_fail(&self, args: &[&str]) -> Result<(i32, String, String)> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.repo_dir).args(args);
        self.apply_auth(&mut cmd);
        let output = cmd.output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::GitUnavailable
            } else {
                Error::Git(format!("failed to launch git: {e}"))
            }
        })?;
        Ok((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }

    /// 确保仓库存在且可用：无 `.git` 则 `git init -b main`；
    /// 无提交身份（全局也没配）则写入本仓库级兜底身份，
    /// 保证离线机器上 commit 不会因缺 `user.name/email` 失败。
    ///
    /// # Errors
    /// 见 [`Store::git`]；目录创建失败返回 [`Error::Io`]。
    pub fn ensure_init(&self) -> Result<()> {
        if !self.repo_dir.join(".git").exists() {
            std::fs::create_dir_all(&self.repo_dir)?;
            self.git(&["init", "-b", "main"])?;
        }
        // 身份兜底：user.email 未设置（退出码非 0）或为空串时补本仓库级身份，
        // 保证离线机器上 commit 不会因缺身份失败
        let (code, out, _) = self.git_allow_fail(&["config", "user.email"])?;
        if code != 0 || out.is_empty() {
            self.git(&["config", "user.email", "myapikey@local"])?;
            self.git(&["config", "user.name", "MyApiKey"])?;
        }
        Ok(())
    }

    /// 读取 origin 远端 URL；未配置返回 `None`。
    pub fn remote_url(&self) -> Result<Option<String>> {
        let (code, out, _) = self.git_allow_fail(&["remote", "get-url", "origin"])?;
        Ok(if code == 0 && !out.is_empty() {
            Some(out)
        } else {
            None
        })
    }

    /// 设置（或覆盖）origin 远端。
    pub fn set_remote(&self, url: &str) -> Result<()> {
        if self.remote_url()?.is_some() {
            self.git(&["remote", "set-url", "origin", url]).map(|_| ())
        } else {
            self.git(&["remote", "add", "origin", url]).map(|_| ())
        }
    }

    /// 把工作区的 vault 文件提交入库（若确有变化）。
    ///
    /// # Returns
    /// - `Ok(true)`：产生了新提交；
    /// - `Ok(false)`：内容与上次提交一致，没有新提交（幂等保存）。
    ///
    /// # Errors
    /// 见 [`Store::git`]。
    pub fn commit_vault(&self, record_count: usize) -> Result<bool> {
        self.git(&["add", VAULT_FILE])?;
        // --short --porcelain：有暂存变更输出非空
        let staged = self.git(&["diff", "--cached", "--name-only"])?;
        if staged.is_empty() {
            return Ok(false);
        }
        let msg = format!(
            "sync: {}, {} records",
            chrono::Utc::now().to_rfc3339(),
            record_count
        );
        self.git(&["commit", "-m", &msg])?;
        Ok(true)
    }

    /// `git fetch origin main`。远端暂无 main 分支（首次推送前的空仓库）不算错误。
    pub fn fetch(&self) -> Result<()> {
        let (code, _, err) = self.git_allow_fail(&["fetch", "origin", "main"])?;
        // 远端空仓库：fetch 报 "couldn't find remote ref" —— 视为无远端数据，正常
        if code != 0 && !err.contains("couldn't find remote ref") {
            return Err(Error::Git(format!(
                "git fetch failed: {}",
                truncate(&err, 300)
            )));
        }
        Ok(())
    }

    /// origin/main 是否存在（远端是否已有数据）。
    pub fn remote_branch_exists(&self) -> bool {
        self.git_allow_fail(&["rev-parse", "--verify", "origin/main"])
            .map(|(c, _, _)| c == 0)
            .unwrap_or(false)
    }

    /// 本地领先 origin/main 的提交数（无远端分支时为全部本地提交数）。
    pub fn ahead_count(&self) -> usize {
        let base = if self.remote_branch_exists() {
            "origin/main"
        } else {
            "HEAD"
        };
        // base=HEAD 时 rev-list HEAD..HEAD = 0，改为直接数本地提交
        if base == "HEAD" {
            return self.count_commits(&["rev-list", "--count", "HEAD"]);
        }
        self.count_commits(&["rev-list", "--count", format!("{base}..HEAD").as_str()])
    }

    /// 本地落后 origin/main 的提交数。
    pub fn behind_count(&self) -> usize {
        if !self.remote_branch_exists() {
            return 0;
        }
        self.count_commits(&["rev-list", "--count", "HEAD..origin/main"])
    }

    fn count_commits(&self, args: &[&str]) -> usize {
        self.git_allow_fail(args)
            .ok()
            .and_then(|(c, out, _)| {
                if c == 0 {
                    out.trim().parse().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0)
    }

    /// 快进合并 origin/main（本地无新提交、远端有新数据的场景）。
    pub fn merge_ff_only(&self) -> Result<()> {
        self.git(&["merge", "--ff-only", "origin/main"]).map(|_| ())
    }

    /// 启动一次常规合并（本地与远端都有新提交时）。
    ///
    /// vault 是二进制密文，双方都改过必然冲突——调用方预期冲突：
    /// 返回 `Ok(true)` 表示存在冲突（合并停在半途，等待我们写入合并结果后
    /// 用 [`Store::conclude_merge`] 收尾）；`Ok(false)` 表示 git 自己合并完成。
    ///
    /// `--allow-unrelated-histories`：覆盖“设备先独立 `init`、
    /// 之后才绑定远端”的场景——两边各有独立根提交，git 默认拒绝合并。
    /// vault 的 both-added 冲突照常走我们的数据层合并，历史只是运输载体。
    ///
    /// # Errors
    /// 除冲突外的失败（如索引损坏）。
    pub fn start_merge(&self) -> Result<bool> {
        let (code, _, _) = self.git_allow_fail(&[
            "merge",
            "--no-edit",
            "--allow-unrelated-histories",
            "origin/main",
        ])?;
        if code == 0 {
            return Ok(false);
        }
        // 处于冲突状态即正常路径；其他失败需要 abort 防止仓库卡在半途
        let conflicted = self
            .git_allow_fail(&["diff", "--name-only", "--diff-filter=U"])?
            .1
            .contains(VAULT_FILE);
        if !conflicted {
            let _ = self.git_allow_fail(&["merge", "--abort"]);
            return Err(Error::Git(
                "merge with origin/main failed unexpectedly (not the usual vault conflict); aborted".into(),
            ));
        }
        Ok(true)
    }

    /// 冲突合并的收尾：暂存当前工作区并完成合并提交。
    pub fn conclude_merge(&self) -> Result<()> {
        self.git(&["add", VAULT_FILE]).map(|_| ())?;
        self.git(&["commit", "--no-edit"]).map(|_| ())
    }

    /// 读取 origin/main 上 vault 文件的字节（双端分歧时获取“远端数据”）。
    ///
    /// # Errors
    /// - [`Error::Git`]：origin/main 不存在或没有 vault 文件。
    pub fn remote_vault_bytes(&self) -> Result<Vec<u8>> {
        self.git_bytes(&["show", &format!("origin/main:{VAULT_FILE}")])
    }

    /// 读取 HEAD 上 vault 文件的字节。
    pub fn head_vault_bytes(&self) -> Result<Vec<u8>> {
        self.git_bytes(&["show", &format!("HEAD:{VAULT_FILE}")])
    }

    /// 执行会输出二进制的 git 命令（`git show` 等）。
    fn git_bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.repo_dir).args(args);
        self.apply_auth(&mut cmd);
        let output = cmd.output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::GitUnavailable
            } else {
                Error::Git(format!("failed to launch git: {e}"))
            }
        })?;
        if !output.status.success() {
            return Err(Error::Git(format!(
                "`git {}` failed: {}",
                args.join(" "),
                truncate(&String::from_utf8_lossy(&output.stderr), 300)
            )));
        }
        Ok(output.stdout)
    }

    /// 推送到 origin/main（首次推送会建立跟踪关系）。
    pub fn push(&self) -> Result<()> {
        self.git(&["push", "-u", "origin", "main"]).map(|_| ())
    }
}

/// 查询远端仓库的 main 分支是否已有提交（不依赖任何本地仓库）。
///
/// 用途：新设备 `init --remote` 时判断走“克隆接入”还是“全新初始化”。
/// `auth` 为可选的 HTTP 认证（私有仓库需要）。
///
/// # Errors
/// - [`Error::GitUnavailable`]：找不到 git；
/// - [`Error::Git`]：远端不可达 / 无权限 / 地址非法（stderr 摘要附在错误里）。
pub fn remote_has_main(url: &str, auth: Option<&GitAuth>) -> Result<bool> {
    let mut cmd = Command::new("git");
    cmd.args(["ls-remote", url, "refs/heads/main"]);
    if let Some(a) = auth {
        cmd.envs(a.git_envs());
    }
    let output = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::GitUnavailable
        } else {
            Error::Git(format!("failed to launch git: {e}"))
        }
    })?;
    if !output.status.success() {
        return Err(Error::Git(format!(
            "cannot reach the remote repository (git ls-remote failed): {}",
            truncate(&String::from_utf8_lossy(&output.stderr), 300)
        )));
    }
    // 输出形如 “<sha>\trefs/heads/main”；空输出 = 远端还没有 main 分支
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

/// 把远端仓库完整克隆到 `dir`（新设备接入已有 vault 的流程）。
///
/// `dir` 必须不存在或为空目录（git clone 的自身要求）。
/// `auth` 为可选的 HTTP 认证（私有仓库需要）。
///
/// # Errors
/// - [`Error::Git`]：克隆失败（网络、权限、目录非空等）。
pub fn clone_into(url: &str, dir: &std::path::Path, auth: Option<&GitAuth>) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg(url).arg(dir);
    if let Some(a) = auth {
        cmd.envs(a.git_envs());
    }
    let output = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::GitUnavailable
        } else {
            Error::Git(format!("failed to launch git: {e}"))
        }
    })?;
    if !output.status.success() {
        return Err(Error::Git(format!(
            "git clone failed: {}",
            truncate(&String::from_utf8_lossy(&output.stderr), 300)
        )));
    }
    Ok(())
}

/// 截断字符串到 `max` 字符，超长加省略号（错误信息防刷屏）。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    //! Store 层集成测试：真实 git 命令、临时目录，不碰网络。

    use super::*;

    fn tmp_store(tag: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("mak-store-{tag}-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");
        Store::new(dir)
    }

    #[test]
    fn init_add_commit_cycle() {
        let s = tmp_store("cycle");
        s.ensure_init().expect("init 失败");
        assert!(s.repo_dir.join(".git").is_dir());

        std::fs::write(s.repo_dir.join(VAULT_FILE), b"fake-vault-bytes").expect("写 vault 失败");
        assert!(
            s.commit_vault(1).expect("commit 失败"),
            "首次提交必须产生新提交"
        );
        // 内容不变 → 幂等，无新提交
        assert!(!s.commit_vault(1).expect("第二次 commit 失败"));

        std::fs::write(s.repo_dir.join(VAULT_FILE), b"changed").expect("改写 vault 失败");
        assert!(s.commit_vault(1).expect("第三次 commit 失败"));
        assert_eq!(s.ahead_count(), 2, "两次提交应领先基线 2 个提交");
    }

    #[test]
    fn remote_set_and_show() {
        let s = tmp_store("remote");
        s.ensure_init().expect("init 失败");
        assert!(s.remote_url().expect("查询远端失败").is_none());
        s.set_remote("https://example.com/x.git")
            .expect("设置远端失败");
        assert_eq!(
            s.remote_url().expect("再次查询失败").as_deref(),
            Some("https://example.com/x.git")
        );
        // 覆盖语义
        s.set_remote("https://example.com/y.git")
            .expect("覆盖远端失败");
        assert_eq!(
            s.remote_url().expect("第三次查询失败").as_deref(),
            Some("https://example.com/y.git")
        );
    }

    #[test]
    fn identity_fallback_keeps_commit_working() {
        // 仓库级清空身份后 ensure_init 应补上兜底身份，提交必须仍能成功
        let s = tmp_store("identity");
        std::fs::create_dir_all(&s.repo_dir).expect("创建目录失败");
        s.git(&["init", "-b", "main"]).expect("init 失败");
        s.git(&["config", "--local", "user.email", ""])
            .expect("清空 email 失败");
        s.git(&["config", "--local", "user.name", ""])
            .expect("清空 name 失败");
        s.ensure_init().expect("ensure_init 失败");
        let email = s
            .git(&["config", "--local", "user.email"])
            .expect("查询兜底 email 失败");
        assert_eq!(email, "myapikey@local", "空身份必须被兜底替换");
        std::fs::write(s.repo_dir.join(VAULT_FILE), b"x").expect("写 vault 失败");
        assert!(
            s.commit_vault(0).expect("commit 失败"),
            "兜底身份下提交必须成功"
        );
    }
}
