//! 全部子命令的实现。
//!
//! 每个命令的套路一致：组装上下文 → 获取实例锁 → 解锁 vault →
//! 调 core 完成业务 → 打印结果。交互（提问/确认/隐藏输入）只发生在这里。

use crate::cli::{PasswdAction, RemoteAction};
use crate::prompt::{self, confirm, new_passphrase, prompt_line};
use crate::table;
use myapikey_core::model::{Protocol, Record, VaultData};
use myapikey_core::paths::AppDirs;
use myapikey_core::probe;
use myapikey_core::sync::{self, SyncOutcome};
use myapikey_core::{check_cache, config::AppConfig, keychain, lock, vault, Error, Result};
use std::path::Path;

/// 一条命令执行期的公共上下文。
struct Ctx {
    dirs: AppDirs,
    config: AppConfig,
}

/// 组装上下文（读取路径与配置，不触碰 vault）。
///
/// 若本机凭据库中存有 GitHub token（`init --github` 流程留下的），
/// 自动附加到 Store——之后所有 git 远端操作（fetch/push/clone）
/// 都带上认证，用户无感。
fn ctx() -> Result<Ctx> {
    let dirs = AppDirs::load()?;
    let config = AppConfig::load(&dirs)?;
    Ok(Ctx { dirs, config })
}

/// 组装同步通道：config 里的远端仓库 + keychain 现读 token（D25）。
///
/// 返回 `None` = 未绑定远端（单机模式）。
///
/// # Errors
/// remote 地址不是 GitHub 形态；绑定了远端但无授权。
fn remote_of(c: &Ctx) -> Result<Option<Box<dyn myapikey_core::remote::SyncStore>>> {
    let Some(url) = c.config.remote_repo.clone() else {
        return Ok(None);
    };
    // 绑定失效折叠为未绑定(2026-11-10 用户拍板:用户只感知绑定与未绑定
    // 两态,不感知失效):凭据条目不在即按单机处理——sync 给单机引导,
    // 写操作跳过推送(本地照常保存,重绑后 sync 一次带上去,数据无损)
    let Some(token) = myapikey_core::github::keychain_token(&c.dirs)? else {
        return Ok(None);
    };
    Ok(Some(myapikey_core::sync::open_remote(&url, &token)?))
}

/// 未初始化时的统一报错。
fn require_init(c: &Ctx) -> Result<()> {
    if !c.dirs.is_initialized() {
        return Err(Error::NotInitialized);
    }
    Ok(())
}

/// 读取并解密当前 vault。
fn load_vault(c: &Ctx, passphrase: &str) -> Result<VaultData> {
    let bytes = std::fs::read(&c.dirs.vault_path)?;
    vault::decrypt(&bytes, passphrase)
}

/// 保存与推送是两个独立感知的阶段：
/// - "Saving…" → 本地加密落盘（成功即数据安全，与网络无关）；
/// - "Syncing to GitHub…" → 上传（可能因网络失败，失败后本地仍可用，
///   `makara sync` 补推）。令牌现读现用（重试自然用续期后的新凭据）。
fn save_and_report(c: &mut Ctx, passphrase: &str, data: &VaultData) -> Result<()> {
    let run = |c: &mut Ctx| -> Result<()> {
        // ---- 阶段 1：本地保存（毫秒级，无网络）----
        let bytes = {
            let _spin = prompt::wait_spinner("Saving…");
            sync::save_local(&c.dirs, passphrase, data, &mut c.config)?
        };
        prompt::ok("Saved locally");

        // ---- 阶段 2：推送（auto_push 且已绑远端才执行）----
        if !c.config.auto_push {
            println!("  Auto-push is off; run `makara sync` to sync manually.");
            return Ok(());
        }
        let remote = remote_of(c)?;
        let Some(remote) = remote else {
            println!("  Standalone mode: nothing to sync.");
            return Ok(());
        };
        let spin = prompt::wait_spinner("Syncing to GitHub…");
        let expect = c.config.last_sync_sha.clone();
        // 推送失败不整体报错（2026-10-31 审查修复 M2）：本地阶段 1 已
        // 保存成功，push_pending 已置位，`makara sync` 会补推——两阶段解耦
        // 的语义就是网络失败不掩盖保存成功
        if let Err(e) = push_initialized(
            remote.as_ref(),
            &c.dirs,
            &mut c.config,
            &bytes,
            expect.as_deref(),
            data.alive_records().count(),
        ) {
            drop(spin);
            prompt::warn(&format!(
                "Sync failed: {e}\n  Data is saved locally; run `makara sync` to retry."
            ));
            return Ok(());
        }
        drop(spin);
        prompt::ok("Synced to GitHub");
        Ok(())
    };
    // 认证失败 → 刷新令牌静默续期后重试一次（与 sync 同策略）；续期
    // 失败时把原因并入错误——只看到 401 不知道续期为何没救场，等于
    // 诊断线索被吞（2026-11-03 修复：原布尔封装吞掉了失败原因）
    match run(c) {
        Err(e) if sync::is_auth_error(&e.to_string()) => {
            match myapikey_core::github::refresh_access_token(
                &c.dirs,
                &myapikey_core::github::effective_client_id()?,
            ) {
                Ok(_) => {
                    myapikey_core::log::info("cli", "token refreshed; retrying command");
                    println!("⟳ Reconnected to GitHub; retrying…");
                    run(c)
                }
                Err(re) => {
                    myapikey_core::log::error("cli", &format!("token refresh failed: {re}"));
                    Err(myapikey_core::Error::Usage(format!(
                        "{e} — re-bind with: makara remote set <owner/repo>"
                    )))
                }
            }
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

/// `init`：初始化本机（交互式）。
///
/// - 无参：交互询问是否同步到 GitHub 私有仓库；
/// - 带仓库参数（`owner/name`、完整 URL 或裸名）：跳过询问直接绑定。
///
/// 同步路径自动处理：授权（有效令牌复用，否则浏览器 Device Flow）、
/// 建仓/复用、数据对账（远端有数据 → 输入解锁密码接入，密码错误原地
/// 重问；空仓库 → 设置新解锁密码并上传基线）。
#[expect(
    clippy::too_many_lines,
    reason = "init 的远端探测/授权/建仓/对账编排是线性状态机,拆分会打断阅读顺序"
)]
pub fn init(repo: Option<&str>) -> Result<()> {
    let mut c = ctx()?;
    let reinit = c.dirs.is_initialized();
    // 已初始化 + 无参：无法推断意图（改绑？解绑？对账？）——引导到
    // 各自的正路，绝不静默改状态
    if reinit && repo.is_none() {
        return Err(Error::Usage(
            "already initialized on this machine.\n  \
             Re-bind / switch repo: makara remote set <owner/repo>\n  \
             Unbind (standalone):   makara remote unset\n  \
             Reconcile with remote: makara sync"
                .into(),
        ));
    }

    // ---- 同步意愿：参数直填 = 同步；无参 = 交互询问 ----
    let wants_remote =
        repo.is_some() || prompt::confirm("Sync to a private GitHub repository?", true);

    // ---- 分支 A：纯单机（不同步）——设置新的解锁密码，零远端操作 ----
    if !wants_remote {
        let passphrase = match std::env::var(prompt::PASSPHRASE_ENV) {
            Ok(p) if !p.is_empty() => {
                println!("Using the passphrase from {}.", prompt::PASSPHRASE_ENV);
                p
            }
            _ => new_passphrase()?,
        };
        c.dirs.ensure_dirs()?;
        sync::save(
            &c.dirs,
            &passphrase,
            &VaultData::empty(),
            None,
            false,
            &mut c.config,
        )?;
        // 记住副本按统一锁定时长写入（0 = 永不锁定，副本不过期；D32）
        let _ = keychain::set_ttl(&c.dirs, &passphrase, c.config.auto_lock_minutes);
        prompt::ok("Initialized (standalone).");
        println!("  Vault: {}", c.dirs.vault_path.display());
        println!("  Bind a remote later: makara remote set <owner/repo>");
        return Ok(());
    }

    // ---- 分支 B：同步到 GitHub 私有仓库 ----
    // 已初始化机器到此为止（2026-10-31 审查修复 S1）：接入/建基线分支
    // 会以远端内容（或空 vault）覆盖本地 vault——本地未推送的记录会
    // 静默丢失。换绑走 remote set（不动数据），对账走 sync
    if reinit {
        return Err(Error::Usage(
            "already initialized on this machine.\n  \
             Re-bind / switch repo: makara remote set <owner/repo>\n  \
             Unbind (standalone):   makara remote unset\n  \
             Reconcile with remote: makara sync"
                .into(),
        ));
    }

    // ① 授权：凭据库有效令牌复用；没有/失效 → 浏览器 Device Flow
    let token = ensure_github_token(&c.dirs)?;

    // ② 解析 owner/name：owner 恒等于授权账号（归属校验 fail fast——
    // 否则错配 owner 会产出"误建仓 + 绑定错位"的混乱状态）
    let login = myapikey_core::github::api_login(&token)?;
    let (owner, name) = match repo {
        Some(arg) => match myapikey_core::remote::parse_github_repo(arg) {
            Ok(id) => {
                let (o, n) = id
                    .split_once('/')
                    .expect("parse_github_repo guarantees one slash");
                (o.to_string(), n.to_string())
            }
            // 裸名字 → 归属当前授权账号
            _ => (login.clone(), arg.to_string()),
        },
        None => (login.clone(), c.config.default_repo_name.clone()),
    };
    if owner != login {
        return Err(Error::Usage(format!(
            "repo owner `{owner}` does not match your GitHub account (`{login}`).\n  \
             makara syncs to a private repo under your own account.\n  \
             Tip: `makara init {name}` places it under your account automatically."
        )));
    }

    // ③ 建仓/复用：不存在则自动创建私有仓库
    let _url = myapikey_core::github::api_ensure_repo(&token, &owner, &name)?;
    let repo_id = format!("{owner}/{name}");
    prompt::ok(&format!(
        "Sync repository ready (private): https://github.com/{repo_id}"
    ));

    // ④ 数据对账：远端有数据 → 接入；空 → 设置新密码并上传基线
    let gh = sync::open_remote(&repo_id, &token)?;
    let gh_dyn: &dyn myapikey_core::remote::SyncStore = &gh;
    let head = gh_dyn.fetch_head()?;

    let record_count;

    if let Some(h) = head {
        // 发现远端加密数据 → 此刻才要求输入解锁密码（解锁语义），
        // 密码错误原地重问；管道 EOF 直接报错退出。
        // env 密码只读一次、失败即退（2026-10-31 审查修复 M3：读在
        // 循环体内时，错误的 env 密码 + 交互终端 = 每轮重读同一个
        // 错误值无限重问）
        let remote_data;
        let pw;
        let env_pw = match std::env::var(prompt::PASSPHRASE_ENV) {
            Ok(p) if !p.is_empty() => Some(p),
            _ => None,
        };
        loop {
            let attempt = match &env_pw {
                Some(p) => {
                    println!("Using the passphrase from {}.", prompt::PASSPHRASE_ENV);
                    p.clone()
                }
                None => prompt::prompt_hidden_optional("Passphrase"),
            };
            if let Ok(d) = vault::decrypt(&h.bytes, &attempt) {
                remote_data = d;
                pw = attempt;
                break;
            }
            if !prompt::is_interactive() {
                return Err(Error::Usage(
                    "cannot decrypt the remote vault with the given passphrase. \
                     All devices must use the same passphrase"
                        .into(),
                ));
            }
            // env 密码错误没有重试意义（每轮都是同一个值）
            if env_pw.is_some() {
                return Err(Error::Usage(
                    "cannot decrypt the remote vault with the passphrase from \
                     the environment variable"
                        .into(),
                ));
            }
            println!("Wrong passphrase or corrupted data; try again.");
        }
        record_count = remote_data.alive_records().count();
        sync::atomic_write_vault(&c.dirs, &h.bytes)?;

        let mut cfg = c.config.clone();
        cfg.remote_repo = Some(repo_id.clone());
        cfg.save(&c.dirs)?;
        c.config = cfg;
        // 接入 = 本地与远端一致：统一走 mark_synced 记账（含
        // last_sync_at——此前手写漏了这个字段，绑定即同步却显示 never）
        sync::mark_synced(&c.dirs, &mut c.config, h.sha);

        // ---- 按 TTL 记住密码(统一锁定时长,0 = 永不过期;D32) ----
        if let Err(e) = keychain::set_ttl(&c.dirs, &pw, c.config.auto_lock_minutes) {
            prompt::warn(&format!(
                "Cannot remember the passphrase: {e}\n  It will be asked every time."
            ));
        }

        prompt::ok(&format!(
            "Initialized with {}.",
            plural(u32::try_from(record_count).unwrap_or(u32::MAX), "record")
        ));
        println!("  Vault: {}", c.dirs.vault_path.display());
        Ok(())
    } else {
        // 远端空仓库：设置新的解锁密码 → 建 vault 上传基线
        let passphrase = match std::env::var(prompt::PASSPHRASE_ENV) {
            Ok(p) if !p.is_empty() => {
                println!("Using the passphrase from {}.", prompt::PASSPHRASE_ENV);
                p
            }
            _ => new_passphrase()?,
        };
        c.dirs.ensure_dirs()?;
        let bytes = vault::encrypt(&VaultData::empty(), &passphrase)?;
        sync::atomic_write_vault(&c.dirs, &bytes)?;
        let _new_sha = push_initialized(gh_dyn, &c.dirs, &mut c.config, &bytes, None, 0)?;
        let mut cfg = c.config.clone();
        cfg.remote_repo = Some(repo_id.clone());
        cfg.save(&c.dirs)?;
        c.config = cfg;

        // 记住副本按统一锁定时长写入(0 = 永不过期;D32)
        let _ = keychain::set_ttl(&c.dirs, &passphrase, c.config.auto_lock_minutes);
        prompt::ok("Initialized with an empty vault.");
        println!("  Baseline uploaded to: https://github.com/{repo_id}");
        println!("  Vault: {}", c.dirs.vault_path.display());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

/// 解析 add 的三个字段。顺序即交互顺序：**协议三选一菜单第一步**
/// （上下键，选 Custom label 才追问标签，2026-10-25 主人定稿），
/// 之后名称、Base URL 必填。命令行给过的项始终优先直填。
fn resolve_add_fields(
    name: Option<String>,
    protocol: Option<String>,
    base_url: Option<String>,
) -> Result<(String, myapikey_core::model::Protocol, String)> {
    let all_given = [&name, &protocol, &base_url]
        .iter()
        .all(|v| v.as_deref().is_some_and(|s| !s.trim().is_empty()));
    if all_given {
        return Ok((
            name.unwrap().trim().to_string(),
            prompt::parse_protocol(&protocol.unwrap()),
            base_url.unwrap().trim().to_string(),
        ));
    }
    // 顺序即交互顺序：协议三选一菜单第一步（选 Custom 才追问标签），
    // 之后名称、Base URL 必填；命令行给过的项始终优先直填
    let protocol = match protocol {
        Some(p) if !p.trim().is_empty() => prompt::parse_protocol(&p),
        _ => prompt::select_protocol(None)?,
    };
    let name = match name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => prompt::prompt_required("Record name (e.g. OpenAI primary)")?,
    };
    let base_url = match base_url {
        Some(b) if !b.trim().is_empty() => {
            let b = b.trim().to_string();
            // 命令行参数直填：非法仍直接报错退出（脚本场景，无交互可回）
            if !myapikey_core::store::is_valid_http_url(&b) {
                return Err(Error::Usage(
                    "invalid base URL: must start with http:// or https:// followed by a host"
                        .into(),
                ));
            }
            b
        }
        // 交互输入：校验失败原地重问，不丢已填内容（2026-10-26 反馈）
        _ => prompt::prompt_http_url("Base URL (stored as-is, no normalization)")?,
    };
    Ok((name, protocol, base_url))
}

/// `add`：交互式添加记录（缺省的参数会逐项询问）。
/// 扫描导入功能已按 D20（2026-10-27）移除——M5 收敛为纯供给。
pub fn add(
    name: Option<String>,
    protocol: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    models: Vec<String>,
    notes: Option<String>,
) -> Result<()> {
    let mut c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let mut data = load_vault(&c, &passphrase)?;

    let (name_tmp, protocol_tmp, base_url_tmp) = resolve_add_fields(name, protocol, base_url)?;

    let api_key = match api_key {
        Some(k) if !k.trim().is_empty() => k.trim().to_string(),
        _ => loop {
            // 可空变体 + 显式重问：必填但拒绝静默死循环（空 Enter 只会
            // 得到一行提示而不是原地重渲染，2026-10-27 修）
            let k = prompt::prompt_hidden_optional("API key");
            if !k.is_empty() {
                break k;
            }
            println!("API key must not be empty; try again.");
        },
    };

    // 模型列表：命令行给了就直接用；否则先自动拉取（list-models 端点，
    // 与 key 校验同一个请求；失败不阻塞回落手输——D5 容错风格）。
    // 手输为空 → 占位 ["unknown"]（models 非空不变量，2026-10-27）
    let models = if models.is_empty() {
        let fetched = fetch_models_interactive(&name_tmp, &protocol_tmp, &base_url_tmp, &api_key);
        if fetched.is_empty() {
            Vec::new()
        } else {
            fetched
        }
    } else {
        models
            .into_iter()
            // 逗号分隔与重复 --model 两种写法都接受（cli-guide 承诺），
            // 中英逗号都拆
            .flat_map(|m| {
                m.split([',', '，'])
                    .map(|s| s.trim().to_string())
                    .collect::<Vec<_>>()
            })
            .filter(|m| !m.is_empty())
            .collect()
    };

    let mut record = Record::new(name_tmp, protocol_tmp, base_url_tmp, api_key);
    record.models = models;
    if let Some(n) = notes {
        record.notes = if n.is_empty() { None } else { Some(n) };
    } else {
        let n = prompt_line("Notes (optional, Enter to skip): ");
        record.notes = if n.is_empty() { None } else { Some(n) };
    }

    let id = record.id;
    let shown_name = record.name.clone();
    data.records.push(record);
    save_and_report(&mut c, &passphrase, &data)?;
    prompt::ok(&format!(
        "Added \"{shown_name}\" (id: {}…)",
        &id.simple().to_string()[..8]
    ));

    // 保存后自动探测一次（失败不阻塞，ADR D5）
    run_probe(&c, &data, &id);
    Ok(())
}

// ---------------------------------------------------------------------------
// list / show / edit / delete
// ---------------------------------------------------------------------------

/// `list` / `search` 共用的表格输出：`list` = 无查询全量（按更新时间倒序），
/// `search` = 模糊过滤并按相关度排序（core::search，与 GUI 搜索同一套
/// 规则：协议不参与、URL 分词、短查询只做子串）。
/// 2026-10-26 拆分为两个命令，语义各自独立；实现共享。
pub fn list(query: Option<&str>) -> Result<()> {
    let c = ctx()?;
    require_init(&c)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let data = load_vault(&c, &passphrase)?;
    let cache = check_cache::CheckCache::load(&c.dirs);

    let all: Vec<&Record> = data.alive_records().collect();
    let query = query.unwrap_or("").trim();
    let rows: Vec<&Record> = if query.is_empty() {
        let mut r = all;
        r.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
        r
    } else {
        myapikey_core::search::search(all, query)
    };

    if rows.is_empty() {
        if query.is_empty() {
            println!("No records yet. Add the first one with `makara add`.");
        } else {
            println!("No records match your search.");
        }
        return Ok(());
    }

    // 列宽（显示宽度）：中文按 2 列计算，保证中英混排对齐
    println!(
        "{} {} {} {} {} {} {}",
        table::pad_to("#", 4),
        table::pad_to("Name", 18),
        table::pad_to("Protocol", 12),
        table::pad_to("Base URL", 34),
        table::pad_to("Key", 20),
        table::pad_to("Models", 20),
        table::pad_to("Check", 10),
    );
    println!("{}", "-".repeat(125));
    for (i, r) in rows.iter().enumerate() {
        let check = cache.get(&r.id).map_or("-", |e| e.status.label());
        println!(
            "{} {} {} {} {} {} {}",
            table::pad_to(&(i + 1).to_string(), 4),
            table::pad_to(&table::truncate_width(&r.name, 18), 18),
            table::pad_to(&table::truncate_width(&r.protocol.display_label(), 12), 12),
            table::pad_to(&table::truncate_width(&r.base_url, 34), 34),
            table::pad_to(&r.masked_key(), 20),
            table::pad_to(&table::truncate_width(&r.models.join(","), 20), 20),
            table::pad_to(check, 10),
        );
    }
    println!("{}", "-".repeat(125));
    // 序号定位（#index）只对 `makara list` 的顺序有效；search 按相关度排序，
    // 行首序号是显示行号——提示用户用名称或 id 定位，避免对错记录操作
    let locator_hint = if query.is_empty() {
        "<name|#index|id>"
    } else {
        "<name|id>"
    };
    println!(
        "{}. Use `makara show {locator_hint}` for full details",
        plural(u32::try_from(rows.len()).unwrap_or(u32::MAX), "record")
    );
    Ok(())
}

/// 在 list 的序号/名称/id 之间解析目标记录下标。
fn resolve_target(data: &VaultData, target: &str) -> Result<usize> {
    // 纯数字 → list 序号（按同样排序）
    if let Ok(n) = target.parse::<usize>() {
        let mut rows: Vec<&Record> = data.alive_records().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
        if n >= 1 && n <= rows.len() {
            let want = rows[n - 1].id;
            return data
                .records
                .iter()
                .position(|r| r.id == want)
                .ok_or_else(|| Error::NotFound(target.into()));
        }
        return Err(Error::NotFound(format!("index {n} out of range")));
    }
    data.find(target)
        .ok_or_else(|| Error::NotFound(target.to_string()))
}

/// `show`：查看单条记录详情。**key 永远掩码显示**（安全红线，见 AGENTS.md），
/// 完整 key 只通过 `--copy` 进剪贴板。
pub fn show(target: &str, copy: bool) -> Result<()> {
    let c = ctx()?;
    require_init(&c)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let data = load_vault(&c, &passphrase)?;
    let r = data
        .records
        .get(resolve_target(&data, target)?)
        .ok_or_else(|| Error::NotFound(target.into()))?;

    println!("Name       : {}", r.name);
    println!("Protocol   : {}", r.protocol.display_label());
    println!("Base URL   : {}", r.base_url);
    if !r.models.is_empty() {
        println!("Models     : {}", r.models.join(", "));
    }
    // 安全红线（AGENTS.md）：完整 key 永远不出现在终端输出中——
    // 终端会留在滚动缓冲/录屏/截图里；明文只进剪贴板（--copy）或显式导出文件
    println!("API Key    : {}", r.masked_key());
    if !r.extra_headers.is_empty() {
        println!("Headers    :");
        for (k, v) in &r.extra_headers {
            println!("            {k}: {v}");
        }
    }
    if let Some(n) = &r.notes {
        println!("Notes      : {n}");
    }
    println!("id         : {}", r.id);
    println!("Created    : {}", r.created_at.format("%Y-%m-%d %H:%M"));
    println!("Updated    : {}", r.updated_at.format("%Y-%m-%d %H:%M"));

    if copy {
        copy_to_clipboard(&r.api_key);
    } else {
        println!("\nUse `makara key {target}` to copy the full key to the clipboard.");
    }
    Ok(())
}

/// `key`：把一条记录的完整 key 复制进剪贴板——`show <t> --copy` 的
/// 直达形态（复制 key 是高频动作，值得独立命令）。安全红线不变：
/// key 只进剪贴板，永远不打印到终端。
pub fn key(target: &str) -> Result<()> {
    let c = ctx()?;
    require_init(&c)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let data = load_vault(&c, &passphrase)?;
    let r = data
        .records
        .get(resolve_target(&data, target)?)
        .ok_or_else(|| Error::NotFound(target.into()))?;
    copy_to_clipboard(&r.api_key);
    Ok(())
}

/// 复制到剪贴板（失败给出手动替代方案，不让命令失败）。
fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.to_string())) {
        Ok(()) => {
            prompt::ok("API key copied.\n  The CLI cannot auto-clear it — clear the clipboard history after pasting.");
        }
        Err(e) => {
            prompt::warn(&format!(
                "Clipboard copy failed: {e}\n  Try again, or use `makara export` (asks twice) to write it to a file."
            ));
        }
    }
}

/// `edit`：逐项询问，回车保留原值。
pub fn edit(target: &str) -> Result<()> {
    let mut c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let mut data = load_vault(&c, &passphrase)?;
    let idx = resolve_target(&data, target)?;
    let record = &mut data.records[idx];
    let original = record.clone();

    println!(
        "Editing \"{}\" — each field is prefilled; edit in place, Enter submits.",
        record.name
    );
    // 名称：预填充编辑（必填——清空提交会原地重问；非 TTY 下 EOF 直接
    // 报错退出，避免空值重问死循环）
    loop {
        let v = if prompt::is_interactive() {
            prompt::prompt_default("Name", &record.name)
        } else {
            prompt::prompt_line_opt("Name (must not be empty): ")
                .ok_or_else(|| Error::Usage("input ended before a name was given".into()))?
        };
        let v = v.trim().to_string();
        if !v.is_empty() {
            record.name = v;
            break;
        }
        println!("Name must not be empty.");
    }
    // Base URL：预填充编辑；清空提交 = 保留现值（原值当初已过校验），
    // 新值非法原地重问
    loop {
        let v = prompt::prompt_default("Base URL", &record.base_url);
        let v = v.trim().to_string();
        if v.is_empty() {
            break; // 保留原值
        }
        if myapikey_core::store::is_valid_http_url(&v) {
            record.base_url = v;
            break;
        }
        println!("Must start with http:// or https:// followed by a host; try again.");
    }
    // key：可空隐藏输入——回车 = 保留现值（prompt_hidden 的默认拒绝空
    // 输入会让"回车保留"永远无法提交，2026-10-27 用户实测反馈）
    let key = prompt::prompt_hidden_optional("API key (Enter keeps current)");
    if !key.is_empty() {
        record.api_key = key;
    }
    // Models：预填充编辑（逗号分隔）；清空提交 = 空列表（应用走默认）
    let models_cur = record.models.join(", ");
    let mv = prompt::prompt_default("Models", &models_cur);
    record.models = parse_model_list(&mv);
    // Notes：预填充，直接编辑；清空提交 = None
    let notes_cur = record.notes.clone().unwrap_or_default();
    let notes = prompt::prompt_default("Notes", &notes_cur);
    let notes = notes.trim().to_string();
    record.notes = if notes.is_empty() { None } else { Some(notes) };

    // 无任何字段变化 → 跳过加密/提交/推送/探测（空跑的 commit 会污染
    // 同步历史，2026-10-28 优化）
    if *record == original {
        println!("No changes.");
        return Ok(());
    }
    record.updated_at = chrono::Utc::now();
    let id = record.id;
    save_and_report(&mut c, &passphrase, &data)?;
    prompt::ok(&format!("Updated \"{}\"", data.records[idx].name));
    run_probe(&c, &data, &id);
    Ok(())
}
/// `delete`：墓碑删除，需确认。
pub fn delete(target: &str) -> Result<()> {
    let mut c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let mut data = load_vault(&c, &passphrase)?;
    let idx = resolve_target(&data, target)?;
    let name = data.records[idx].name.clone();

    if !confirm(
        &format!("Delete \"{name}\"? (recoverable within the grace period, then purged)"),
        false,
    ) {
        println!("Cancelled.");
        return Ok(());
    }
    data.records[idx].mark_deleted();
    save_and_report(&mut c, &passphrase, &data)?;
    prompt::ok(&format!("Deleted \"{name}\""));
    Ok(())
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// `check`：探测一条或全部记录，结果写入本地缓存。
pub fn check(target: Option<&str>) -> Result<()> {
    let c = ctx()?;
    require_init(&c)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let data = load_vault(&c, &passphrase)?;

    let targets: Vec<Record> = match target {
        Some(t) => {
            let idx = resolve_target(&data, t)?;
            vec![data.records[idx].clone()]
        }
        None => data.alive_records().cloned().collect(),
    };
    if targets.is_empty() {
        println!("No records to check.");
        return Ok(());
    }

    let mut cache = check_cache::CheckCache::load(&c.dirs);
    if targets.len() == 1 {
        // 单条：原地检测（带 spinner 指示）
        let r = &targets[0];
        let (status, http) = probe::probe(r);
        cache.put(r.id, status, http);
        println!(
            "{} {} {} {}",
            table::pad_to(status.label(), 12),
            table::pad_to(&table::truncate_width(&r.name, 18), 18),
            r.base_url,
            http.map_or_default(|h| format!("(HTTP {h})"))
        );
    } else {
        // 全部：核心层并发探测（2026-10-31 一键检测全部）——总耗时从
        // "逐条之和"降为约最慢一条；结果与输入同序，输出表格不变
        let spin = prompt::wait_spinner(&format!(
            "Checking {} concurrently…",
            plural(u32::try_from(targets.len()).unwrap_or(u32::MAX), "record")
        ));
        let results = probe::probe_all(&targets);
        drop(spin);
        for (r, (_id, status, http)) in targets.iter().zip(&results) {
            cache.put(r.id, *status, *http);
            println!(
                "{} {} {} {}",
                table::pad_to(status.label(), 12),
                table::pad_to(&table::truncate_width(&r.name, 18), 18),
                r.base_url,
                http.map_or_default(|h| format!("(HTTP {h})"))
            );
        }
    }
    cache.save(&c.dirs)?;
    Ok(())
}

/// 数量 + 单词的英文复数（1 record / 3 records）。用户可见输出禁止
/// record(s) 这种括号缩写（AGENTS.md 提示词规则，2026-10-25）。
fn plural(n: u32, word: &str) -> String {
    format!("{n} {word}{s}", s = if n == 1 { "" } else { "s" })
}

/// 对单条记录探测并落缓存（add/edit 后自动调用）。
fn run_probe(c: &Ctx, data: &VaultData, id: &uuid::Uuid) {
    let Some(r) = data.records.iter().find(|r| r.id == *id) else {
        return;
    };
    // 探测最长 10s 超时（D5），给等待指示
    let spin = prompt::wait_spinner("Checking key against the endpoint…");
    let (status, http) = probe::probe(r);
    drop(spin);
    println!(
        "Key check: {} {}",
        status.label(),
        http.map_or_default(|h| format!("(HTTP {h})"))
    );
    let mut cache = check_cache::CheckCache::load(&c.dirs);
    cache.put(*id, status, http);
    if let Err(e) = cache.save(&c.dirs) {
        prompt::warn(&format!("Failed to save the check cache: {e}"));
    }
}

// ---------------------------------------------------------------------------
// sync / status / remote
// ---------------------------------------------------------------------------

/// `sync`：完整同步。
pub fn sync() -> Result<()> {
    let mut c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    // D55：sync 用免预校验的密码解析——密码可能属于远端而非本地 vault
    // （换绑场景），本地预校验会把正确的远端密码挡在门外
    let (passphrase, pass_source) = prompt::passphrase_for_sync(&c.dirs)?;

    // 网络等待指示器（2026-10-27 用户反馈）：下载/上传可能数秒乃至更久，
    // 转轮让等待可感知。连接类失败已由请求通道自动重试（3 次退避）。
    // 认证失败（token 过期/被回收）→ 用刷新令牌静默续期后重试一次；
    // 刷新失败维持原错误（错误文本指向重新授权）
    let run = |c: &mut Ctx| -> Result<SyncOutcome> {
        let spin = prompt::wait_spinner("Syncing with remote…");
        let remote = remote_of(c)?;
        let Some(remote) = remote else {
            return Ok(SyncOutcome {
                alive_records: std::fs::read(&c.dirs.vault_path)
                    .ok()
                    .and_then(|b| vault::decrypt(&b, &passphrase).ok())
                    .map_or(0, |d| d.alive_records().count()),
                ..Default::default()
            });
        };
        let out = sync::sync(
            &c.dirs,
            &passphrase,
            remote.as_ref(),
            c.config.tombstone_days,
            &mut c.config,
        );
        drop(spin);
        out
    };
    let out = match run(&mut c) {
        Err(e) if sync::is_auth_error(&e.to_string()) => {
            match myapikey_core::github::refresh_access_token(
                &c.dirs,
                &myapikey_core::github::effective_client_id()?,
            ) {
                Ok(_) => {
                    myapikey_core::log::info("cli", "token refreshed; retrying sync");
                    println!("⟳ Reconnected to GitHub; retrying sync…");
                    run(&mut c)?
                }
                Err(re) => {
                    myapikey_core::log::error("cli", &format!("token refresh failed: {re}"));
                    Err(myapikey_core::Error::Usage(format!(
                        "{e} — re-bind with: makara remote set <owner/repo>"
                    )))?
                }
            }
        }
        other => other?,
    };

    match (&out.remote, out.pulled, out.pushed) {
        (None, _, _) => println!(
            "Standalone mode, no remote. Nothing to sync.\n  Bind one with: makara remote set <owner/repo>"
        ),
        (_, true, true) => prompt::ok(&format!(
            "Synced: pulled remote changes{} and pushed",
            if out.merged {
                ", merged divergent edits"
            } else {
                ""
            }
        )),
        (_, true, false) => prompt::ok("Pulled remote changes"),
        (_, false, true) => prompt::ok("Pushed to remote"),
        (Some(_), false, false) => prompt::ok("Up to date"),
    }
    // D55：密码「确实用成了」（发生拉取或推送）才滚动续期记住副本——
    // sync 的密码可能属于远端而非本地 vault，失败时续期等于把错误
    // 密码再留一个锁定周期
    if (out.pulled || out.pushed) && pass_source != prompt::PassSource::Env {
        if let Err(e) = keychain::set_ttl(&c.dirs, &passphrase, c.config.auto_lock_minutes) {
            prompt::warn(&format!(
                "Cannot remember the passphrase: {e}\n  It will be asked every time."
            ));
        }
    }
    if let Some(err) = &out.push_error {
        prompt::warn(&format!("Push failed: {err}"));
    }
    println!(
        "{}",
        plural(
            u32::try_from(out.alive_records).unwrap_or(u32::MAX),
            "active record"
        )
    );

    // 同步状态（含 last_sync_at）由 core 的 mark_synced 在"达成一致"
    // 时记账（上传成功/下载替换/已是最新），此处不再手写——单机模式
    // 与推送失败不应留下"刚同步过"的痕迹
    Ok(())
}

/// `status`：仓库与同步状态总览。
pub fn status() -> Result<()> {
    let mut c = ctx()?;
    if !c.dirs.is_initialized() {
        println!("Not initialized. Start with: makara init");
        return Ok(());
    }
    // 1.0.0 → 1.1.0 升级迁移（幂等）：升级用户跑一次 status 即恢复绑定
    sync::migrate_legacy_git(&c.dirs, &mut c.config);
    let info = sync::status_summary(&c.dirs, &c.config)?;

    // 绑定失效折叠为未绑定:凭据条目不在,Remote 显示 none(standalone)
    let remote_shown = info
        .remote
        .clone()
        .filter(|_| myapikey_core::github::keychain_token(&c.dirs).is_ok());
    match &remote_shown {
        Some(u) => println!("Remote     : {u}"),
        None => println!("Remote     : none (standalone mode)"),
    }
    println!(
        "Unpushed   : {}",
        if info.pending {
            "yes (run makara sync)"
        } else {
            "no"
        }
    );
    println!(
        "Last sync  : {}",
        c.config.last_sync_at.as_deref().unwrap_or("never")
    );
    println!(
        "Tombstone  : {} days / auto push: {}",
        c.config.tombstone_days,
        if c.config.auto_push { "on" } else { "off" }
    );

    // 记录数需要解锁；密码环节失败时只跳过这部分（status 不应强制输密码）
    if std::env::var(prompt::PASSPHRASE_ENV).is_ok_and(|v| !v.is_empty())
        || keychain::get_with_expiry(&c.dirs).ok().flatten().is_some()
    {
        if let Ok((pass, _)) = prompt::unlock(&c.dirs) {
            if let Ok(data) = load_vault(&c, &pass) {
                println!("Records    : {}", data.alive_records().count());
            }
        }
    } else {
        println!("Records    : hidden (unlock to see)");
    }
    Ok(())
}

/// `remote set/show/unset`。
pub fn remote(action: RemoteAction) -> Result<()> {
    let mut c = ctx()?;
    require_init(&c)?;
    match action {
        RemoteAction::Set { url } => {
            // 统一存 GitHub 形态（owner/name），完整 URL 先归一化
            let repo = myapikey_core::remote::parse_github_repo(&url)?;
            let mut cfg = c.config.clone();
            cfg.remote_repo = Some(repo.clone());
            cfg.save(&c.dirs)?;
            c.config = cfg;
            myapikey_core::log::info("cli", &format!("remote set: {repo}"));
            prompt::ok(&format!("Remote set: {repo}"));
            // 绑定后确保授权就绪（无有效令牌 → 现场走浏览器授权）：
            // 用户只感知仓库地址，令牌由 makara 自动打理——set 完 sync 必定可用
            match ensure_github_token(&c.dirs) {
                Ok(_) => println!("  GitHub ready. Sync now with: makara sync"),
                Err(e) => prompt::warn(&format!(
                    "{e}\n  Sync will fail until the binding is complete."
                )),
            }
        }
        RemoteAction::Show => {
            // 绑定失效折叠为未绑定:凭据条目不在即显示 no remote bound
            // (2026-11-10 用户拍板:用户只感知绑定,不感知失效)
            let bound = c
                .config
                .remote_repo
                .take_if(|_| myapikey_core::github::keychain_token(&c.dirs).is_ok());
            match bound {
                Some(repo) => println!("https://github.com/{repo}"),
                None => println!("no remote bound"),
            }
        }
        // 撤销 set：解绑远端并清除本机的 GitHub 授权（回到单机）。
        // 解绑统一走 core 的 unbind_remote（remote_repo + 记账三件套
        // 一次写清，与 mark_synced 对称）。vault 原样不动——数据与
        // 远端仓库都不受影响
        RemoteAction::Unset => {
            myapikey_core::log::info("cli", "remote unset (unbind + forget authorization)");
            let mut cfg = c.config.clone();
            myapikey_core::sync::unbind_remote(&c.dirs, &mut cfg);
            let token_cleared = myapikey_core::github::keychain_delete_token(&c.dirs)?;
            prompt::ok("Unbound.");
            if token_cleared {
                println!(
                    "  The remote repository on GitHub is untouched.\n  Re-bind anytime with: makara remote set <owner/repo>"
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// passwd（set）与 lock
// ---------------------------------------------------------------------------

/// `passwd`：修改解锁密码（无参 / `set` 等价；D32 语义统一后 `unset`
/// 子命令删除，立即锁定改用 `makara lock`）。
pub fn passwd(action: Option<&PasswdAction>) -> Result<()> {
    match action {
        Some(PasswdAction::Set) | None => {}
    }
    let mut c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    let (old_pw, _) = prompt::unlock(&c.dirs)?;
    let data = load_vault(&c, &old_pw)?;

    let new_pw = new_passphrase()?;
    // 保存+推送走 save_and_report 两阶段展示（2026-11-11 用户指出改密
    // 后看不到同步过程）：改密是关键操作，静默完成让用户无从确认远端
    // 是否已更新；推送失败也不硬报错——本地已落新密文，pending 补推
    save_and_report(&mut c, &new_pw, &data)?;

    // 记住副本随新密码按统一锁定时长重写（0 = 永不锁定 = 副本不过期；
    // D32 语义统一）。写失败静默：改密码本身已成功，副本缺失只会让
    // 下次回退手动输入，无损害
    if keychain::set_ttl(&c.dirs, &new_pw, c.config.auto_lock_minutes).is_ok() {
        println!("  Credential store updated.");
    }
    println!("  Other devices will need the new passphrase on their next sync.");
    Ok(())
}

/// `makara lock`：立即锁定——清除本机记住的解锁密码副本（vault 不动）。
///
/// D32 语义统一后接替原 `makara passwd unset`：下次命令要求输入解锁密码，
/// 输入后又按自动锁定时长重新记住——适合借出电脑前手动上锁。
pub fn lock_now() -> Result<()> {
    myapikey_core::log::info("cli", "locked now (remembered copy cleared)");
    let c = ctx()?;
    let cleared = keychain::delete(&c.dirs)?;
    if cleared {
        prompt::ok("Locked. Remembered passphrase cleared.\n  The next command asks for it once, then remembers again.");
    } else {
        println!("No remembered passphrase on this machine.");
    }
    Ok(())
}

/// 已初始化语境下的上传（init 自身就是同步点，绕过 save 的置位语义）。
/// 记账统一走 core 的 push_and_mark（2026-10-31 审查：手写三件套违反
/// mark_synced 唯一写入口约定）。
fn push_initialized(
    remote: &dyn myapikey_core::remote::SyncStore,
    dirs: &AppDirs,
    cfg: &mut AppConfig,
    bytes: &[u8],
    expect: Option<&str>,
    alive: usize,
) -> Result<String> {
    myapikey_core::sync::push_and_mark(remote, dirs, cfg, bytes, expect, alive)
}

/// 确保存在可用的 GitHub 授权：凭据库里**确认仍有效**的令牌直接复用
/// （失效条目会被顺手清除）；没有则走浏览器 Device Flow。
fn ensure_github_token(dirs: &AppDirs) -> Result<String> {
    use myapikey_core::github;
    let client_id = github::effective_client_id()?;

    if let Some(t) = github::valid_keychain_token(dirs)? {
        println!("Using the existing binding.");
        Ok(t)
    } else {
        let dc = github::start_device_flow(&client_id)?;
        println!("Finish binding in the browser (one-time):");
        println!("  1. Open: {}", dc.verification_uri);
        println!("  2. Enter this code: {}", dc.user_code);
        if github::open_browser(&dc.verification_uri).is_err() {
            println!("  Could not open a browser automatically. Copy the link above.");
        }
        println!("Waiting for you to finish in the browser… this continues automatically.");
        let t = github::poll_device_token(dirs, &dc, &client_id)?;
        // scope 可见化（2026-10-30 排查教训：零权限令牌是黑盒灾难）进
        // 日志而非终端——排查依据保留，用户不感知令牌细节
        myapikey_core::log::info(
            "cli",
            &format!("token scopes: {}", t.scope.as_deref().unwrap_or("(none!)")),
        );
        // refresh token 在 poll 内部已入 keychain（有才写）；这里只存 access
        github::keychain_set_token(dirs, &t.access_token)?;
        prompt::ok("Binding complete.\n  Unbind anytime: makara remote unset.");
        Ok(t.access_token)
    }
}

// ---------------------------------------------------------------------------
// export / import
// ---------------------------------------------------------------------------

/// 在目录下生成不冲突的导出文件名：makara-export.json，已存在则
/// makara-export-2.json、-3 递增——绝不覆盖用户已有的导出。
fn unique_export_path(dir: &Path) -> std::path::PathBuf {
    let base = dir.join("makara-export.json");
    if !base.exists() {
        return base;
    }
    for i in 2u32.. {
        let p = dir.join(format!("makara-export-{i}.json"));
        if !p.exists() {
            return p;
        }
    }
    unreachable!("序号递增必然找到空位")
}

/// `export`：明文导出（两次确认 + 0600 权限）。无参数 = 导出到当前
/// 目录（2026-11-11 用户要求），复用目录分支的不冲突命名。
pub fn export(path: Option<&Path>) -> Result<()> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    myapikey_core::log::info(
        "cli",
        &format!("plaintext export requested -> {}", path.display()),
    );
    // 目标是目录：自动生成不冲突的文件名并告知（2026-10-26 主人决议：
    // 不报错、不覆盖已有文件——已存在时递增序号）
    let out_path = if path.is_dir() {
        let p = unique_export_path(&path);
        println!("Output path is a directory; exporting to: {}", p.display());
        p
    } else {
        path
    };
    let c = ctx()?;
    require_init(&c)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let data = load_vault(&c, &passphrase)?;

    let alive: Vec<&Record> = data.alive_records().collect();
    if alive.is_empty() {
        println!("No records to export.");
        return Ok(());
    }

    println!(
        "⚠ About to write {} with PLAINTEXT API keys to:",
        plural(u32::try_from(alive.len()).unwrap_or(u32::MAX), "record")
    );
    println!("  {}", out_path.display());
    println!("  Plaintext is a leak risk — delete it when done.");
    if !confirm("Export plaintext?", false)
        || !confirm("Really continue despite the leak risk?", false)
    {
        println!("Cancelled.");
        return Ok(());
    }

    let json = serde_json::to_vec_pretty(&alive)
        .map_err(|e| Error::Serde(format!("export serialization failed: {e}")))?;
    std::fs::write(&out_path, &json)?;
    restrict_permissions(&out_path);
    prompt::ok(&format!(
        "Exported to {}.\n  Contains plaintext keys. Delete it when done",
        out_path.display()
    ));
    Ok(())
}

/// 非 Windows 上把文件权限收紧为仅属主可读写；Windows 无 POSIX 权限，跳过。
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// `import`：从 JSON 导入（兼容导出格式与裸数组）。
pub fn import(path: &Path) -> Result<()> {
    myapikey_core::log::info("cli", &format!("import from {}", path.display()));
    let mut c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Usage(format!("failed to read {}: {e}", path.display())))?;

    // 两种形状：[{...}] 裸数组（export 的格式）或 {"records":[...]}
    let imported: Vec<Record> = if raw.trim_start().starts_with('[') {
        serde_json::from_str(&raw)
            .map_err(|e| Error::Usage(format!("import file is not a valid record array: {e}")))?
    } else {
        serde_json::from_str::<VaultData>(&raw)
            .map_err(|e| Error::Usage(format!("unrecognized import file format: {e}")))?
            .records
    };
    if imported.is_empty() {
        println!("No records in the import file.");
        return Ok(());
    }

    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let mut data = load_vault(&c, &passphrase)?;

    // 合并在 core（与 GUI 导入共用同一语义，D47）
    let report = myapikey_core::merge::import_records(&mut data, imported, chrono::Utc::now());
    if report.added + report.updated == 0 {
        // 幂等导入（逐条全部跳过，如导入刚导出的文件）：数据未变——
        // 不重加密落盘、不推送，避免一次无意义的远端空提交
        // （2026-11-11 用户指出）
        prompt::ok("Import done: 0 added, 0 updated.");
        return Ok(());
    }
    // 导入前快照（D52）：被覆盖的本地旧版本写当前目录——误导入时数据
    // 有保底（明文文件，权限收紧，用完即删；其 updated_at 早于现数据，
    // 直接再导入会被 LWW 跳过——是保底而非一键回滚）
    if !report.overwritten.is_empty() {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let snap = std::path::PathBuf::from(format!("makara-import-backup-{ts}.json"));
        let json = serde_json::to_string_pretty(&report.overwritten)
            .map_err(|e| Error::Usage(format!("cannot serialize the backup: {e}")))?;
        std::fs::write(&snap, json)?;
        restrict_permissions(&snap);
        prompt::warn(&format!(
            "{} record(s) were overwritten. Pre-import snapshot: {}",
            report.overwritten.len(),
            snap.display()
        ));
    }
    save_and_report(&mut c, &passphrase, &data)?;
    prompt::ok(&format!(
        "Import done: {} added, {} updated.",
        report.added, report.updated
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------
// 表格对齐与截断统一在 table 模块（按显示宽度，中文占 2 列）。

// ---------------------------------------------------------------------------
// 版本（D16：CLI 不自替换，只提示新版本 + 下载指引）
// ---------------------------------------------------------------------------

/// `version`：只打印当前版本，不联网（2026-10-26 拆分：联网检查归 update）。
pub fn version() -> Result<()> {
    println!("myapikey {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}

/// `update`：联网查最新 Release，对照当前版本；有新版时**零依赖自更新**
/// ——下载 GitHub Release 的平台压缩包、解压、替换自身（D40），全程
/// 不要求 cargo / Rust 工具链（产品面向大众用户）。
///
/// Windows 自锁的解法（D16 当年放弃自替换的原因）：运行中的 exe 不能
/// 被覆盖，但**可以重命名**——先把自身改名备份，新二进制落到原路径，
/// 下次启动即新版。非交互环境（管道/脚本）不自动动手，只打印直链。
/// 网络失败/无 Release 都不报错——检查只是增强，不该让命令失败。
pub fn update() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{current}");
    // GitHub API 网络请求，给等待指示（失败不阻塞，见下）。
    // 令牌从存储带上：匿名限额 60 次/小时/IP 对检查太紧，有授权升到
    // 5000/小时；未授权机器降级匿名调用，行为不变
    let c = ctx()?;
    let token = myapikey_core::github::keychain_token(&c.dirs)?;
    let spin = prompt::wait_spinner("Checking for updates…");
    let release = myapikey_core::github::latest_release_tag(token.as_deref());
    drop(spin);
    match release {
        Ok(Some(tag)) => {
            let latest = tag.trim_start_matches('v');
            println!("Latest version:  {tag}");
            if is_newer(latest, current) {
                // 有新版直接自更新，不询问（2026-11-11 用户拍板的流程）；
                // 仅限交互终端——管道/脚本里静默替换二进制有意外性，
                // 降级为打印直链（安全侧）
                println!("⚠ A newer version is available.");
                if prompt::is_interactive() {
                    do_upgrade(&tag, &c.dirs)?;
                } else {
                    let (url, _) = release_asset(&tag)?;
                    println!("  Download manually: {url}");
                }
            } else {
                prompt::ok("Up to date.");
            }
        }
        Ok(None) => println!("No releases published yet."),
        Err(e) => println!("Could not check for updates: {e}"),
    }
    Ok(())
}

/// 当前编译目标对应的 Release 附件平台串（与 release.yml 矩阵一一
/// 对齐）；无预编译包的平台返回 `None`，`update` 给出手动下载出路。
fn release_target() -> Option<&'static str> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else {
        None
    }
}

/// 最新 tag 对应的平台附件直链与扩展名（CI 平铺打包：包内根直接是
/// 二进制）。完整 URL 形如
/// `…/releases/download/v1.1.1/makara-v1.1.1-x86_64-pc-windows-msvc.zip`。
fn release_asset(tag: &str) -> Result<(String, &'static str)> {
    let target = release_target().ok_or_else(|| {
        Error::Usage(
            "no prebuilt binary for this platform.\n  \
             Download from: https://github.com/Haaaatcher/MyApiKey/releases"
                .into(),
        )
    })?;
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let url = format!(
        "https://github.com/Haaaatcher/MyApiKey/releases/download/{tag}/makara-{tag}-{target}.{ext}"
    );
    Ok((url, ext))
}

/// 下载附件，TTY 下行内百分比进度（`\r` 刷新），管道/脚本静默。
/// 下载在 core 完成（复用其 HTTP 栈），这里只负责进度呈现。
fn download_asset(url: &str, dest: &Path, dirs: &AppDirs) -> Result<()> {
    use std::io::Write as _;
    let token = myapikey_core::github::keychain_token(dirs)?;
    let show = prompt::is_interactive();
    myapikey_core::github::download_asset(token.as_deref(), url, dest, &mut |done, total| {
        if show {
            match total {
                Some(t) if t > 0 => print!("\r  Downloading… {}%", done * 100 / t),
                // 总长未知：退化为已下载量（整数 KiB，免浮点转换）
                _ => print!("\r  Downloading… {} KiB", done / 1024),
            }
            std::io::stdout().flush().ok();
        }
    })?;
    if show {
        println!("\r  Downloading… 100%");
    }
    Ok(())
}

/// 解压更新包：Windows 用 **绝对路径** 的 System32 tar（Win10 1803+
/// 内置 bsdtar，支持 zip 读取）——PATH 里的 `tar` 可能解析到 Git Bash
/// 的 GNU tar（不支持 zip，用户环境 PATH 顺序不可控，D52 实测）；
/// POSIX 用系统 tar（tar.gz）。均零新依赖。
fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("C:\\Windows\\System32\\tar.exe");
        c.arg("-xf");
        c
    } else {
        let mut c = std::process::Command::new("tar");
        c.arg("-xzf");
        c
    };
    let status = cmd
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .map_err(|e| Error::Usage(format!("cannot run the system tar: {e}")))?;
    if !status.success() {
        return Err(Error::Usage(format!(
            "extract failed (tar exit code {}).\n  \
             Download and update manually: https://github.com/Haaaatcher/MyApiKey/releases",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

/// 用新二进制替换自身（D40 的核心技巧）：拷到同目录暂存名 → 自身
/// 改名 `.update-old`（Windows 允许重命名运行中的 exe，句柄保持有效）
/// → 暂存名落位；落位失败把备份放回原位（回滚），`.old` 残留无害。
fn replace_self(new_bin: &Path) -> Result<()> {
    let cur = std::env::current_exe()
        .map_err(|e| Error::Usage(format!("cannot locate the running binary: {e}")))?;
    let staging = cur.with_extension("update-new");
    let backup = cur.with_extension("update-old");
    std::fs::copy(new_bin, &staging)?;
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(&cur, &backup)
        .map_err(|e| Error::Usage(format!("cannot rename the running binary: {e}")))?;
    if let Err(e) = std::fs::rename(&staging, &cur) {
        // 回滚：优先把备份 rename 回原位；rename 也失败（极罕见）则用
        // copy 兜底——无论如何不能让 makara 命令从磁盘上消失
        if std::fs::rename(&backup, &cur).is_err() {
            let _ = std::fs::copy(&backup, &cur);
        }
        return Err(Error::Usage(format!("cannot place the new binary: {e}")));
    }
    Ok(())
}

/// 检查通过后的升级动作：下载（行内进度 + sha256 校验）→ 解压 →
/// 替换自身。每步一行状态；任一步失败即上抛（用户回到可手动下载的
/// 出路）。临时目录成败都清理（④）。
fn do_upgrade(tag: &str, dirs: &AppDirs) -> Result<()> {
    let tmp = std::env::temp_dir().join(format!("makara-update-{tag}"));
    std::fs::create_dir_all(&tmp)
        .map_err(|e| Error::Usage(format!("cannot create a temp dir: {e}")))?;
    let result = do_upgrade_in(tag, &tmp, dirs);
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn do_upgrade_in(tag: &str, tmp: &Path, dirs: &AppDirs) -> Result<()> {
    let (url, ext) = release_asset(tag)?;
    let archive = tmp.join(format!("makara-{tag}.{ext}"));
    let hash_url = format!("{url}.sha256");
    let hash_file = tmp.join(format!("makara-{tag}.{ext}.sha256"));
    download_asset(&url, &archive, dirs)?;
    // 完整性校验（D40 后续加强落地）：Release 附件旁的 .sha256 与
    // 本地文件逐一比对（⑥）
    download_asset(&hash_url, &hash_file, dirs)?;
    let expected = parse_sha256_file(&hash_file);
    let actual = sha256_of_file(&archive)?;
    if let Some(expected) = expected {
        if !expected.eq_ignore_ascii_case(&actual) {
            return Err(Error::Usage(format!(
                "checksum mismatch (expected {expected}, got {actual}).\n  \
                 Download and update manually: https://github.com/Haaaatcher/MyApiKey/releases"
            )));
        }
    }
    println!("  Extracting…");
    extract_archive(&archive, tmp)?;
    let bin = tmp.join(if cfg!(windows) {
        "makara.exe"
    } else {
        "makara"
    });
    if !bin.exists() {
        return Err(Error::Usage(format!(
            "unexpected archive layout (no {} inside)",
            bin.display()
        )));
    }
    println!("  Replacing binary…");
    replace_self(&bin)?;
    println!("✓ Upgraded to {tag}.");
    println!("  Takes effect on the next launch.");
    Ok(())
}

/// 解析 .sha256 附件：`sha256sum` 形态（`<hash>  <filename>`）取第一
/// 段；解析失败返回 `None`（跳过校验不阻塞）。
fn parse_sha256_file(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let hash = text.split_whitespace().next()?.to_string();
    (hash.len() == 64).then_some(hash)
}

/// 用系统工具算文件 SHA-256（零依赖：Windows `certutil` / POSIX
/// `sha256sum`），返回小写十六进制。
fn sha256_of_file(path: &Path) -> Result<String> {
    let output = if cfg!(windows) {
        std::process::Command::new("certutil")
            .args(["-hashfile"])
            .arg(path)
            .arg("SHA256")
            .output()
    } else {
        std::process::Command::new("sha256sum").arg(path).output()
    }
    .map_err(|e| Error::Usage(format!("cannot run the hash tool: {e}")))?;
    let text = String::from_utf8_lossy(&output.stdout);
    // certutil 的输出混有「SHA256」标题与本地化说明，只有哈希行整行
    // 均为十六进制——按行精确提取，避免字符级过滤混入标题里的数字
    let hex = if cfg!(windows) {
        text.lines()
            .map(str::trim)
            .filter(|l| l.len() >= 60 && l.chars().all(|c| c.is_ascii_hexdigit()))
            .collect::<String>()
    } else {
        text.split_whitespace().next().unwrap_or("").to_string()
    };
    if hex.len() != 64 {
        return Err(Error::Usage("cannot compute the file checksum".into()));
    }
    Ok(hex.to_ascii_lowercase())
}

/// 语义化版本比较：`candidate` 是否严格新于 `current`。
/// 逐段数字比较，段数不足按 0 补（0.2 > 0.1.9；0.1.0 == 0.1）。
/// 解析失败的段按 0 处理（宁可漏报，不误报新版本）。
fn is_newer(candidate: &str, current: &str) -> bool {
    fn seg(v: &str) -> std::iter::Map<std::str::Split<'_, char>, fn(&str) -> u64> {
        v.split('.').map(|p| p.parse::<u64>().unwrap_or(0))
    }
    let c = seg(candidate);
    let cur = seg(current);
    // 逐段比较，第一个不相等的段决定结果（semver 语义：高位相同才看低位）
    c.zip(cur.chain(std::iter::repeat(0)))
        .find(|(a, b)| a != b)
        .is_some_and(|(a, b)| a > b)
}

// ---------------------------------------------------------------------------
// M5 生态集成：apply / restore（供给 = 写配置文件，D21）。
// 环境变量供给（makara env）已按 D21 移除——不做用不上的能力。
// ---------------------------------------------------------------------------

/// 逗号分隔的模型名解析：逐项 trim、去空（中英逗号都接受）。
fn parse_model_list(raw: &str) -> Vec<String> {
    raw.split([',', '，'])
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect()
}

/// 交互式的模型列表获取：先调 list-models 端点自动拉取（与 key 校验同
/// 一个请求），成功则展示并让回车收下 / 手输覆盖；失败（网络 / 非 200 /
/// Custom 协议）静默回落手输提问。**拉取失败绝不阻塞添加**（D5）。
fn fetch_models_interactive(
    name: &str,
    protocol: &Protocol,
    base_url: &str,
    api_key: &str,
) -> Vec<String> {
    // probe 吃 Record——拼一个临态记录（只为借它的请求构造）
    let tmp = Record::new(
        name.to_string(),
        protocol.clone(),
        base_url.to_string(),
        api_key.to_string(),
    );
    // 网络拉取，同样给等待指示
    let spin = prompt::wait_spinner("Fetching model list from the endpoint…");
    let fetched = probe::fetch_models(&tmp);
    drop(spin);
    if let Some(fetched) = fetched {
        // 展示：≤8 个全列；更多只列前 8 + 总数（中转端可能返回上百个）
        let preview = if fetched.len() <= 8 {
            fetched.join(", ")
        } else {
            format!(
                "{}, … and {} more",
                fetched[..8].join(", "),
                fetched.len() - 8
            )
        };
        println!("Fetched {} models: {preview}", fetched.len());
        // 选择器：小清单多选勾选，大清单模糊搜索逐个添加
        return prompt::pick_models(&fetched);
    }
    // 回落：常规手输（Enter 跳过）
    let raw = prompt_line("Models (comma-separated, Enter to skip): ");
    parse_model_list(&raw)
}

#[cfg(test)]
mod tests {
    //! 版本比较回归（D16）：提示新版本的前提是比较不误报。
    //! 自更新机制回归（D40）：解压与 rename 替换是全链路里唯一能在
    //! 无网络环境下验证的核心——真链路待真实发版自然验证。
    use super::{extract_archive, is_newer, parse_model_list, release_asset, replace_self};

    /// 平台附件 URL 形态（D40）：仓库前缀、tag、目标串与扩展名全部
    /// 就位——自更新下载按它构造，错一个字符就是 404。
    #[test]
    fn release_asset_url_shape() {
        let (url, ext) = release_asset("v9.9.9").expect("当前编译平台应有预编译包");
        assert!(url.starts_with(
            "https://github.com/Haaaatcher/MyApiKey/releases/download/v9.9.9/makara-v9.9.9-"
        ));
        if cfg!(windows) {
            assert_eq!(ext, "zip");
            assert!(url.ends_with("x86_64-pc-windows-msvc.zip"));
        } else {
            assert_eq!(ext, "tar.gz");
            assert!(url.ends_with(".tar.gz"));
        }
    }

    /// 解压机制（D40）：构造一个与 release.yml 平铺打包同构的 tar.gz，
    /// 解压后包根直接是二进制——自更新按包根取文件。
    #[test]
    fn extract_archive_flattens_ci_layout() {
        let tmp = std::env::temp_dir().join(format!("mak-extract-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("临时目录创建");
        // 构造包：与 release.yml 打包同构（tar -C pkg makara.exe）
        let payload = tmp.join("makara.exe");
        std::fs::write(&payload, b"fake binary").expect("写假二进制");
        let archive = tmp.join("update.tar.gz");
        let built = std::process::Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&tmp)
            .arg("makara.exe")
            .status()
            .expect("系统 tar 可用")
            .success();
        assert!(built, "构造 tar.gz 失败");
        let out = tmp.join("out");
        std::fs::create_dir_all(&out).expect("输出目录创建");
        extract_archive(&archive, &out).expect("解压成功");
        assert_eq!(
            std::fs::read(out.join("makara.exe")).expect("读解压产物"),
            b"fake binary",
            "解压产物应与原始文件一致"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 替换机制（D40）——rename 策略三断言：原路径落位新内容、备份
    /// 存在、内容正确。current_exe 是测试二进制自身，与真实场景同构
    /// （Windows 重命名运行中的 exe 合法，句柄保持）。
    #[test]
    fn replace_self_swaps_binary_and_keeps_backup() {
        let cur = std::env::current_exe().expect("current exe");
        let dir = cur.parent().expect("exe 目录");
        // "新版" = 自身的一个副本：验证对象是替换机制而非文件内容
        let fake = dir.join("makara-selftest-new.bin");
        std::fs::copy(&cur, &fake).expect("构造新版");
        replace_self(&fake).expect("替换成功");
        let backup = cur.with_extension("update-old");
        assert!(cur.exists(), "原路径应存在落位的新二进制");
        assert!(backup.exists(), "旧版备份应存在");
        assert_eq!(
            std::fs::read(&cur).expect("读落位文件"),
            std::fs::read(&fake).expect("读新版"),
            "落位内容应与新版一致"
        );
        // 备份指向正在运行的测试 exe——Windows 锁定运行中文件的删除，
        // 残留是机制的自然产物（真实场景由下次 update 的 replace_self
        // 顺手清理：那时旧文件已不在运行）
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::remove_file(&fake);
    }

    #[test]
    fn semver_compare() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        // 段数不足按 0 补
        assert!(is_newer("0.2", "0.1.9"));
        assert!(!is_newer("0.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        // 非数字段按 0：不误报
        assert!(!is_newer("x.y", "0.0.0"));
    }

    /// 模型列表解析：中英逗号、空白容错、空项剔除。
    #[test]
    fn model_list_parsing() {
        assert_eq!(
            parse_model_list("GLM-5.2, GLM-5-Air"),
            vec!["GLM-5.2", "GLM-5-Air"]
        );
        assert_eq!(parse_model_list("a，b"), vec!["a", "b"]);
        assert_eq!(parse_model_list(" , ,"), [] as [std::string::String; 0]);
        assert_eq!(parse_model_list(""), [] as [std::string::String; 0]);
    }
}
