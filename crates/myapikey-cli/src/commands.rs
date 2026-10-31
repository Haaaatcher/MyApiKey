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
    let Some(token) = myapikey_core::github::keychain_token()? else {
        return Err(Error::Usage(
            "a remote is bound but no GitHub authorization exists on this machine.              Run `mak init` to re-authorize"
                .into(),
        ));
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
///   `mak sync` 补推）。令牌现读现用（重试自然用续期后的新凭据）。
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
            println!("  Auto-push is off; run `mak sync` to sync manually.");
            return Ok(());
        }
        let remote = remote_of(c)?;
        let Some(remote) = remote else {
            println!("  Standalone mode: nothing to sync.");
            return Ok(());
        };
        let _spin = prompt::wait_spinner("Syncing to GitHub…");
        let expect = c.config.last_sync_sha.clone();
        // 推送失败不整体报错（2026-10-31 审查修复 M2）：本地阶段 1 已
        // 保存成功，push_pending 已置位，`mak sync` 会补推——两阶段解耦
        // 的语义就是网络失败不掩盖保存成功
        if let Err(e) = push_initialized(
            remote.as_ref(),
            &c.dirs,
            &mut c.config,
            &bytes,
            expect,
            data.alive_records().count(),
        ) {
            drop(_spin);
            prompt::warn(&format!(
                "Sync failed: {e}\n  Data is saved locally; run `mak sync` to retry."
            ));
            return Ok(());
        }
        drop(_spin);
        prompt::ok("Synced to GitHub");
        Ok(())
    };
    // 认证失败 → 刷新令牌静默续期后重试一次（与 sync 同策略）
    match run(c) {
        Err(e)
            if sync::is_auth_error(&e.to_string())
                && myapikey_core::github::try_refresh_and_store(
                    &myapikey_core::github::effective_client_id()?,
                ) =>
        {
            println!("⟳ GitHub authorization refreshed; retrying…");
            run(c)
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
/// 建仓/复用、数据对账（远端有数据 → 输入主密码接入，密码错误原地
/// 重问；空仓库 → 设置新主密码并上传基线）。
pub fn init(repo: Option<&str>) -> Result<()> {
    let mut c = ctx()?;
    let reinit = c.dirs.is_initialized();
    // 已初始化 + 无参：无法推断意图（改绑？解绑？对账？）——引导到
    // 各自的正路，绝不静默改状态
    if reinit && repo.is_none() {
        return Err(Error::Usage(
            "already initialized on this machine.\n  \
             Re-bind / switch repo: mak remote set <owner/repo>\n  \
             Unbind (standalone):   mak remote unset\n  \
             Reconcile with remote: mak sync"
                .into(),
        ));
    }

    // ---- 同步意愿：参数直填 = 同步；无参 = 交互询问 ----
    let wants_remote =
        repo.is_some() || prompt::confirm("Sync to a private GitHub repository?", true);

    // ---- 分支 A：纯单机（不同步）——设置新的主密码，零远端操作 ----
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
        let days = c.config.remember_days;
        if days > 0 {
            let _ = keychain::set_ttl(&c.dirs, &passphrase, days);
        }
        prompt::ok("Initialized (standalone).");
        println!("  Vault: {}", c.dirs.vault_path.display());
        println!("  Bind a remote later: mak remote set <owner/repo>");
        return Ok(());
    }

    // ---- 分支 B：同步到 GitHub 私有仓库 ----
    // 已初始化机器到此为止（2026-10-31 审查修复 S1）：接入/建基线分支
    // 会以远端内容（或空 vault）覆盖本地 vault——本地未推送的记录会
    // 静默丢失。换绑走 remote set（不动数据），对账走 sync
    if reinit {
        return Err(Error::Usage(
            "already initialized on this machine.\n  \
             Re-bind / switch repo: mak remote set <owner/repo>\n  \
             Unbind (standalone):   mak remote unset\n  \
             Reconcile with remote: mak sync"
                .into(),
        ));
    }

    // ① 授权：凭据库有效令牌复用；没有/失效 → 浏览器 Device Flow
    let token = ensure_github_token()?;

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
        None => (
            login.clone(),
            myapikey_core::github::DEFAULT_REPO_NAME.to_string(),
        ),
    };
    if owner != login {
        return Err(Error::Usage(format!(
            "repo owner `{owner}` does not match the authorized account `{login}`.\n  \
             mak syncs to a private repo under the authorized account.\n  \
             Tip: `mak init {name}` places it under your account automatically."
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

    match head {
        Some(h) => {
            // 发现远端加密数据 → 此刻才要求输入主密码（解锁语义），
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
                    None => prompt::prompt_hidden_optional("Master passphrase"),
                };
                match vault::decrypt(&h.bytes, &attempt) {
                    Ok(d) => {
                        remote_data = d;
                        pw = attempt;
                        break;
                    }
                    Err(_) => {
                        if !prompt::is_interactive() {
                            return Err(Error::Usage(
                                "cannot decrypt the remote vault with the given passphrase. \
                                 All devices must use the same master passphrase"
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
                }
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

            // ---- 按 TTL 记住密码 ----
            let days = c.config.remember_days;
            if days > 0 {
                match keychain::set_ttl(&c.dirs, &pw, days) {
                    Ok(()) => {}
                    Err(e) => prompt::warn(&format!(
                        "Cannot remember the passphrase: {e}\n  It will be asked every time."
                    )),
                }
            }

            prompt::ok(&format!(
                "Initialized with {}.",
                plural(record_count as u32, "record")
            ));
            if days > 0 {
                println!(
                    "  Passphrase remembered for {} (lock: mak passwd unset).",
                    plural(days, "day")
                );
            } else {
                println!("  Passphrase remembering is disabled.");
            }
            println!("  Vault: {}", c.dirs.vault_path.display());
            Ok(())
        }
        None => {
            // 远端空仓库：设置新的主密码 → 建 vault 上传基线
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

            let days = c.config.remember_days;
            if days > 0 {
                let _ = keychain::set_ttl(&c.dirs, &passphrase, days);
            }
            prompt::ok("Initialized with an empty vault.");
            println!("  Baseline uploaded to: https://github.com/{repo_id}");
            if days > 0 {
                println!(
                    "  Passphrase remembered for {} (lock: mak passwd unset).",
                    plural(days, "day")
                );
            } else {
                println!("  Passphrase remembering is disabled.");
            }
            println!("  Vault: {}", c.dirs.vault_path.display());
            Ok(())
        }
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
    println!(
        "✓ Added \"{shown_name}\" (id: {}…)",
        &id.simple().to_string()[..8]
    );

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
            println!("No records yet. Add the first one with `mak add`.");
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
        let check = cache.get(&r.id).map(|e| e.status.label()).unwrap_or("-");
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
    // 序号定位（#index）只对 `mak list` 的顺序有效；search 按相关度排序，
    // 行首序号是显示行号——提示用户用名称或 id 定位，避免对错记录操作
    let locator_hint = if query.is_empty() {
        "<name|#index|id>"
    } else {
        "<name|id>"
    };
    println!(
        "{}. Use `mak show {locator_hint}` for full details",
        plural(rows.len() as u32, "record")
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
        copy_to_clipboard(&r.api_key)?;
    } else {
        println!("\nUse `mak show {target} --copy` to copy the full key to the clipboard.");
    }
    Ok(())
}

/// 复制到剪贴板（失败给出手动替代方案，不让命令失败）。
fn copy_to_clipboard(text: &str) -> Result<()> {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.to_string())) {
        Ok(()) => {
            println!("✓ API key copied.\n  The CLI cannot auto-clear it — clear the clipboard history after pasting.");
            Ok(())
        }
        Err(e) => {
            println!("⚠ Clipboard copy failed: {e}\n  Try again, or use `mak export` (asks twice) to write it to a file.");
            Ok(())
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
    println!("✓ Updated \"{}\"", data.records[idx].name);
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
    println!("✓ Deleted \"{name}\"");
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
            http.map(|h| format!("(HTTP {h})")).unwrap_or_default()
        );
    } else {
        // 全部：核心层并发探测（2026-10-31 一键检测全部）——总耗时从
        // "逐条之和"降为约最慢一条；结果与输入同序，输出表格不变
        let _spin = prompt::wait_spinner(&format!(
            "Checking {} concurrently…",
            plural(targets.len() as u32, "record")
        ));
        let results = probe::probe_all(&targets);
        drop(_spin);
        for (r, (_id, status, http)) in targets.iter().zip(&results) {
            cache.put(r.id, *status, *http);
            println!(
                "{} {} {} {}",
                table::pad_to(status.label(), 12),
                table::pad_to(&table::truncate_width(&r.name, 18), 18),
                r.base_url,
                http.map(|h| format!("(HTTP {h})")).unwrap_or_default()
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
    let _spin = prompt::wait_spinner("Checking key against the endpoint…");
    let (status, http) = probe::probe(r);
    drop(_spin);
    println!(
        "Key check: {} {}",
        status.label(),
        http.map(|h| format!("(HTTP {h})")).unwrap_or_default()
    );
    let mut cache = check_cache::CheckCache::load(&c.dirs);
    cache.put(*id, status, http);
    if let Err(e) = cache.save(&c.dirs) {
        eprintln!("⚠ Failed to save the check cache: {e}");
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
    let (passphrase, _) = prompt::unlock(&c.dirs)?;

    // 网络等待指示器（2026-10-27 用户反馈）：下载/上传可能数秒乃至更久，
    // 转轮让等待可感知。连接类失败已由请求通道自动重试（3 次退避）。
    // 认证失败（token 过期/被回收）→ 用刷新令牌静默续期后重试一次；
    // 刷新失败维持原错误（错误文本指向重新授权）
    let run = |c: &mut Ctx| -> Result<SyncOutcome> {
        let _spin = prompt::wait_spinner("Syncing with remote…");
        let remote = remote_of(c)?;
        let Some(remote) = remote else {
            return Ok(SyncOutcome {
                alive_records: std::fs::read(&c.dirs.vault_path)
                    .ok()
                    .and_then(|b| vault::decrypt(&b, &passphrase).ok())
                    .map(|d| d.alive_records().count())
                    .unwrap_or(0),
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
        drop(_spin);
        out
    };
    let out = match run(&mut c) {
        Err(e)
            if sync::is_auth_error(&e.to_string())
                && myapikey_core::github::try_refresh_and_store(
                    &myapikey_core::github::effective_client_id()?,
                ) =>
        {
            println!("⟳ GitHub authorization refreshed; retrying sync…");
            run(&mut c)?
        }
        other => other?,
    };

    match (&out.remote, out.pulled, out.pushed) {
        (None, _, _) => println!(
            "Standalone mode, no remote. Nothing to sync.\n  Bind one with: mak remote set <owner/repo>"
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
    if let Some(err) = &out.push_error {
        prompt::warn(&format!("Push failed: {err}"));
    }
    println!("{}", plural(out.alive_records as u32, "active record"));

    // 同步状态（含 last_sync_at）由 core 的 mark_synced 在"达成一致"
    // 时记账（上传成功/下载替换/已是最新），此处不再手写——单机模式
    // 与推送失败不应留下"刚同步过"的痕迹
    Ok(())
}

/// `status`：仓库与同步状态总览。
pub fn status() -> Result<()> {
    let mut c = ctx()?;
    if !c.dirs.is_initialized() {
        println!("Not initialized. Start with: mak init");
        return Ok(());
    }
    // 1.0.0 → 1.1.0 升级迁移（幂等）：升级用户跑一次 status 即恢复绑定
    sync::migrate_legacy_git(&c.dirs, &mut c.config);
    let info = sync::status_summary(&c.dirs, &c.config)?;

    match &info.remote {
        Some(u) => println!("Remote     : {u}"),
        None => println!("Remote     : none (standalone mode)"),
    }
    println!(
        "Unpushed   : {}",
        if info.pending {
            "yes (run mak sync)"
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
    if std::env::var(prompt::PASSPHRASE_ENV)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
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
            prompt::ok(&format!("Remote set: {repo}"));
            // 绑定后确保授权就绪（无有效令牌 → 现场走浏览器授权）：
            // 用户只感知仓库地址，令牌由 mak 自动打理——set 完 sync 必定可用
            match ensure_github_token() {
                Ok(_) => println!("  GitHub ready. Sync now with: mak sync"),
                Err(e) => prompt::warn(&format!("{e}\n  Sync will fail until authorized.")),
            }
        }
        RemoteAction::Show => match c.config.remote_repo.clone() {
            Some(repo) => println!("https://github.com/{repo}"),
            None => println!("no remote bound"),
        },
        // 撤销 set：解绑远端并清除本机的 GitHub 授权（回到单机）。
        // 解绑统一走 core 的 unbind_remote（remote_repo + 记账三件套
        // 一次写清，与 mark_synced 对称）。vault 原样不动——数据与
        // 远端仓库都不受影响
        RemoteAction::Unset => {
            let mut cfg = c.config.clone();
            myapikey_core::sync::unbind_remote(&c.dirs, &mut cfg);
            let token_cleared = myapikey_core::github::keychain_delete_token()?;
            prompt::ok("Remote unbound; GitHub authorization forgotten.");
            if token_cleared {
                println!(
                    "  The remote repository on GitHub is untouched.\n  Re-bind anytime with: mak remote set <owner/repo>"
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// passwd（set / unset）
// ---------------------------------------------------------------------------

/// `passwd`：修改主密码（无参 / `set`）；`unset` 清除本机记住的密码
/// 副本（立即锁定；vault 不动）。
pub fn passwd(action: Option<PasswdAction>) -> Result<()> {
    match action {
        Some(PasswdAction::Unset) => return passwd_unset(),
        Some(PasswdAction::Set) | None => {}
    }
    let mut c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    let (old_pw, _) = prompt::unlock(&c.dirs)?;
    let data = load_vault(&c, &old_pw)?;

    let new_pw = new_passphrase()?;
    let remote = remote_of(&c)?;
    let remote_dyn: Option<&dyn myapikey_core::remote::SyncStore> = remote
        .as_ref()
        .map(|r| r as &dyn myapikey_core::remote::SyncStore);
    sync::save(
        &c.dirs,
        &new_pw,
        &data,
        remote_dyn,
        c.config.auto_push,
        &mut c.config,
    )?;

    // 凭据库同步：统一按 TTL 模型记住新密码（remember_days = 0 时跳过）
    let remembered = c.config.remember_days > 0
        && keychain::set_ttl(&c.dirs, &new_pw, c.config.remember_days).is_ok();
    prompt::ok("Master passphrase changed.");
    if remembered {
        println!("  Credential store updated.");
    }
    println!("  Other devices will need the new passphrase on their next sync.");
    Ok(())
}

/// `passwd unset`：清除本机记住的主密码（立即锁定）。
///
/// TTL 模型下的语义：unset = "现在锁起来"。下次命令会要求输入一次，
/// 输入后又开始新的记住周期（默认 7 天）——适合借出电脑前手动上锁。
pub fn passwd_unset() -> Result<()> {
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
    expect: Option<String>,
    alive: usize,
) -> Result<String> {
    myapikey_core::sync::push_and_mark(remote, dirs, cfg, bytes, expect, alive)
}

/// 确保存在可用的 GitHub 授权：凭据库里**确认仍有效**的令牌直接复用
/// （失效条目会被顺手清除）；没有则走浏览器 Device Flow。
fn ensure_github_token() -> Result<String> {
    use myapikey_core::github;
    let client_id = github::effective_client_id()?;

    match github::valid_keychain_token()? {
        Some(t) => {
            println!("Using the saved GitHub authorization.");
            Ok(t)
        }
        None => {
            let dc = github::start_device_flow(&client_id)?;
            println!("GitHub authorization (one-time only):");
            println!("  1. Open: {}", dc.verification_uri);
            println!("  2. Enter this code: {}", dc.user_code);
            if github::open_browser(&dc.verification_uri).is_err() {
                println!("  Could not open a browser automatically. Copy the link above.");
            }
            println!("Waiting for authorization… this continues automatically.");
            let t = github::poll_device_token(&dc, &client_id)?;
            // scope 可见化（2026-10-30 排查教训：零权限令牌是黑盒灾难）；
            // 输出职责在 CLI——core 只回传数据
            println!(
                "  Token scopes: {}",
                t.scope.as_deref().unwrap_or("(none!)")
            );
            // refresh token 在 poll 内部已入 keychain（有才写）；这里只存 access
            github::keychain_set_token(&t.access_token)?;
            prompt::ok(
                "Authorized. Token saved to the credential store.\n  Forget it: mak remote unset.",
            );
            Ok(t.access_token)
        }
    }
}

// ---------------------------------------------------------------------------
// export / import
// ---------------------------------------------------------------------------

/// 在目录下生成不冲突的导出文件名：myapikey-export.json，已存在则
/// myapikey-export-2.json、-3 递增——绝不覆盖用户已有的导出。
fn unique_export_path(dir: &Path) -> std::path::PathBuf {
    let base = dir.join("myapikey-export.json");
    if !base.exists() {
        return base;
    }
    for i in 2u32.. {
        let p = dir.join(format!("myapikey-export-{i}.json"));
        if !p.exists() {
            return p;
        }
    }
    unreachable!("序号递增必然找到空位")
}

/// `export`：明文导出（两次确认 + 0600 权限）。
pub fn export(path: &Path) -> Result<()> {
    // 目标是目录：自动生成不冲突的文件名并告知（2026-10-26 主人决议：
    // 不报错、不覆盖已有文件——已存在时递增序号）
    let out_path = if path.is_dir() {
        let p = unique_export_path(path);
        println!("Output path is a directory; exporting to: {}", p.display());
        p
    } else {
        path.to_path_buf()
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
        plural(alive.len() as u32, "record")
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
    println!(
        "✓ Exported to {}.\n  Contains plaintext keys. Delete it when done",
        out_path.display()
    );
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

    let (mut added, mut updated) = (0usize, 0usize);
    for mut r in imported {
        match data.records.iter_mut().find(|x| x.id == r.id) {
            // 同 id：按 updated_at 新者胜，导入数据与手工编辑同一套语义
            Some(existing) => {
                if r.updated_at > existing.updated_at {
                    *existing = r;
                    updated += 1;
                }
            }
            None => {
                r.updated_at = chrono::Utc::now();
                data.records.push(r);
                added += 1;
            }
        }
    }
    save_and_report(&mut c, &passphrase, &data)?;
    println!("✓ Import done: {added} added, {updated} updated.");
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

/// `update`：联网查最新 Release 并提示升级命令。
/// 网络失败/无 Release 都不报错——检查只是增强，不该让命令失败。
pub fn update() -> Result<()> {
    // GitHub API 网络请求，给等待指示（失败不阻塞，见下）
    let _spin = prompt::wait_spinner("Checking for updates…");
    let release = myapikey_core::github::latest_release_tag();
    drop(_spin);
    match release {
        Ok(Some(tag)) => {
            let latest = tag.trim_start_matches('v');
            let current = env!("CARGO_PKG_VERSION");
            if is_newer(latest, current) {
                // 目前唯一实际可用的更新通道是 cargo（2026-10-26 主人确认：
                // Release 二进制分发尚未启用）。--tag 锁定与提示一致的版本，
                // 避免装到 main 分支未发布的代码
                println!("New version {latest} is available.");
                println!("  Upgrade: cargo install --git https://github.com/Haaaatcher/MyApiKey --tag {tag} myapikey-cli");
            } else {
                println!("Up to date.");
            }
        }
        Ok(None) => println!("No releases published yet."),
        Err(e) => println!("Could not check for updates: {e}"),
    }
    Ok(())
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
// 环境变量供给（mak env）已按 D21 移除——不做用不上的能力。
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
    let _spin = prompt::wait_spinner("Fetching model list from the endpoint…");
    let fetched = probe::fetch_models(&tmp);
    drop(_spin);
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
    use super::{is_newer, parse_model_list};

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
        assert!(parse_model_list(" , ,").is_empty());
        assert!(parse_model_list("").is_empty());
    }
}
