//! 全部子命令的实现。
//!
//! 每个命令的套路一致：组装上下文 → 获取实例锁 → 解锁 vault →
//! 调 core 完成业务 → 打印结果。交互（提问/确认/隐藏输入）只发生在这里。

use crate::cli::RemoteAction;
use crate::prompt::{self, confirm, new_passphrase, prompt_line};
use crate::table;
use myapikey_core::model::{Record, VaultData};
use myapikey_core::paths::AppDirs;
use myapikey_core::probe;
use myapikey_core::store::Store;
use myapikey_core::sync::{self, SyncOutcome};
use myapikey_core::{check_cache, config::AppConfig, keychain, lock, vault, Error, Result};
use std::path::Path;

/// 一条命令执行期的公共上下文。
struct Ctx {
    dirs: AppDirs,
    store: Store,
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
    let mut store = Store::new(&dirs.repo_dir);
    if let Ok(Some(token)) = myapikey_core::github::keychain_token() {
        store = store.with_auth(myapikey_core::github::GitAuth::github_token(&token));
    }
    Ok(Ctx {
        store,
        dirs,
        config,
    })
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

/// 保存 + 提交 +（按配置）自动推送，并打印同步摘要。
fn save_and_report(c: &Ctx, passphrase: &str, data: &VaultData) -> Result<()> {
    let out = sync::save(&c.store, &c.dirs, passphrase, data, c.config.auto_push)?;
    report_push(&out);
    Ok(())
}

/// 打印推送结果（失败不视为命令失败）。
fn report_push(out: &SyncOutcome) {
    if let Some(err) = &out.push_error {
        println!("⚠ Saved locally, but push failed: {err}");
        println!("  The remote usually has new changes — run `mak sync` to merge and push.");
    } else if out.pushed {
        println!("✓ Pushed to remote");
    }
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

/// `init`：设主密码、建本地仓库、（可选）绑远端。
///
/// 远端分两种情况：
/// - 远端**已有数据**（main 分支存在）→ **克隆接入**：新设备拿到完整历史，
///   并立即验证主密码能解开远端 vault（所有设备必须同一主密码）；
/// - 远端为空/未建 → 全新初始化，绑定远端并做首次推送。
pub fn init(remote: Option<&str>, github: Option<&str>) -> Result<()> {
    let mut c = ctx()?;
    if c.dirs.is_initialized() {
        return Err(Error::Usage(
            "already initialized on this machine; to reset, delete the config directory manually"
                .into(),
        ));
    }

    // 环境变量提供了主密码则跳过交互（脚本化初始化/CI 场景）
    let passphrase = match std::env::var(prompt::PASSPHRASE_ENV) {
        Ok(p) if !p.is_empty() => {
            println!("(using the passphrase from {})", prompt::PASSPHRASE_ENV);
            p
        }
        _ => new_passphrase()?,
    };

    // --github：Device Flow 鉴权 → 自动创建/识别仓库（ADR D13），
    // 之后统一走既有的 --remote 流程；token 附到 Store 供本次与后续 push 使用
    let mut remote = remote.map(str::to_string);
    if let Some(name) = github {
        let (url, token) = github_setup(name)?;
        remote = Some(url);
        c.store = c
            .store
            .clone()
            .with_auth(myapikey_core::github::GitAuth::github_token(&token));
    }
    let remote = remote.as_deref();

    // 分支一：远端已有 vault → 克隆接入（第二台及以后的设备走这条路）
    let mut record_count = 0usize;
    let mut joined_existing = false;
    if let Some(url) = remote {
        if myapikey_core::store::remote_has_main(url, c.store.auth())? {
            std::fs::create_dir_all(&c.dirs.config_dir)?;
            match join_existing(&c, url, &passphrase) {
                Ok(n) => {
                    record_count = n;
                    joined_existing = true;
                }
                // 克隆成功但密码解不开远端 vault：删掉刚克隆的目录，
                // 让用户换正确密码后可以重新 init（避免卡在“已初始化”状态）
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&c.dirs.repo_dir);
                    return Err(e);
                }
            }
        }
    }

    // 分支二：全新初始化（无远端，或远端还是空仓库）
    if !joined_existing {
        c.dirs.ensure_dirs()?;
        c.store.ensure_init()?;
        // 空 vault 落盘 + 首次提交
        sync::save(&c.store, &c.dirs, &passphrase, &VaultData::empty(), false)?;
        if let Some(url) = remote {
            c.store.set_remote(url)?;
            match c.store.push() {
                Ok(()) => println!("✓ Remote bound, first push done: {url}"),
                Err(e) => println!("⚠ Remote bound, but the first push failed: {e}\n  Run `mak sync` later to retry."),
            }
        }
    }

    // 按 TTL 记住密码（2026-10-24 决议：默认 7 天，输入一次即可；
    // remember_days = 0 时跳过）。不再逐次询问——行为由 config.toml 统一控制。
    let days = c.config.remember_days;
    if days > 0 {
        match keychain::set_ttl(&c.dirs, &passphrase, days) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("⚠ Cannot remember the passphrase ({e}); it will be asked every time.")
            }
        }
    }

    println!("\n✓ Initialized — {record_count} record(s)");
    if joined_existing {
        println!("  Joined an existing vault; run `mak sync` to get the latest.");
    }
    if days > 0 {
        println!("  Passphrase remembered for {days} day(s) — lock any time with `mak forget`.");
    } else {
        println!("  Passphrase remembering is disabled (remember_days = 0).");
    }
    println!("  Repository: {}", c.dirs.repo_dir.display());
    if remote.is_none() {
        println!("  No remote yet — for multi-device sync run: mak remote set <url>");
    }
    Ok(())
}

/// 克隆远端并验证主密码，返回接入的记录数。
fn join_existing(c: &Ctx, url: &str, passphrase: &str) -> Result<usize> {
    myapikey_core::store::clone_into(url, &c.dirs.repo_dir, c.store.auth())?;
    let bytes = std::fs::read(&c.dirs.vault_path)?;
    let data = vault::decrypt(&bytes, passphrase).map_err(|_| {
        Error::Usage(
            "远端 vault 无法用输入的主密码解密。所有设备必须使用同一个主密码；\
             若这是你的首次部署，请确认仓库地址是一个 MyApiKey 同步仓库"
                .into(),
        )
    })?;
    let n = data.alive_records().count();
    println!("✓ Joined from remote: {url}");
    Ok(n)
}

/// `--github` 流程：拿到可用 token（凭据库优先，否则 Device Flow），
/// 检测/创建私有仓库。返回 (克隆地址, token)——token 由调用方挂到 Store。
fn github_setup(repo_name: &str) -> Result<(String, String)> {
    use myapikey_core::github;
    let client_id = github::effective_client_id()?;

    // token：优先复用凭据库里已存的（每台设备只在第一次走浏览器授权）
    let token = match github::keychain_token()? {
        Some(t) => {
            println!("(using the saved GitHub authorization)");
            t
        }
        None => {
            let dc = github::start_device_flow(&client_id)?;
            println!("GitHub authorization (one-time only):");
            println!("  1. Open: {}", dc.verification_uri);
            println!("  2. Enter this code: {}", dc.user_code);
            if github::open_browser(&dc.verification_uri).is_err() {
                println!("  (could not open a browser automatically — copy the link above)");
            }
            println!("Waiting for authorization… this continues automatically.");
            let t = github::poll_device_token(&dc, &client_id)?;
            github::keychain_set_token(&t)?;
            println!("✓ Authorized — token saved to the credential store (clear with `mak forget --github`)");
            t
        }
    };

    let owner = github::api_login(&token)?;
    let url = github::api_ensure_repo(&token, &owner, repo_name)?;
    println!("✓ Sync repository ready (private): {url}");
    Ok((url, token))
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

/// `add`：交互式添加记录。命令行给了的字段不再询问。
pub fn add(
    name: Option<String>,
    protocol: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    notes: Option<String>,
) -> Result<()> {
    let c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let mut data = load_vault(&c, &passphrase)?;

    let name = match name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => loop {
            let n = prompt_line("Record name (e.g. OpenAI primary): ");
            if !n.is_empty() {
                break n;
            }
            println!("Name must not be empty.");
        },
    };
    let protocol = match protocol {
        Some(p) if !p.trim().is_empty() => prompt::parse_protocol(&p),
        _ => prompt::select_protocol(None),
    };
    let base_url = match base_url {
        Some(b) if !b.trim().is_empty() => b.trim().to_string(),
        _ => loop {
            let b = prompt_line("Base URL (stored as-is, no normalization): ");
            if !b.is_empty() {
                break b;
            }
            println!("Base URL must not be empty.");
        },
    };
    let api_key = match api_key {
        Some(k) if !k.trim().is_empty() => k.trim().to_string(),
        _ => {
            let k = prompt::prompt_hidden("API Key: ");
            if k.is_empty() {
                return Err(Error::Usage("API key must not be empty".into()));
            }
            k
        }
    };

    let mut record = Record::new(name, protocol, base_url, api_key);
    if let Some(n) = notes {
        record.notes = if n.is_empty() { None } else { Some(n) };
    } else {
        let n = prompt_line("Notes (Enter to skip): ");
        record.notes = if n.is_empty() { None } else { Some(n) };
    }

    let id = record.id;
    let shown_name = record.name.clone();
    data.records.push(record);
    save_and_report(&c, &passphrase, &data)?;
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

/// `list`：掩码表格。
pub fn list() -> Result<()> {
    let c = ctx()?;
    require_init(&c)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let data = load_vault(&c, &passphrase)?;
    let cache = check_cache::CheckCache::load(&c.dirs);

    let mut rows: Vec<&Record> = data.alive_records().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.updated_at));

    if rows.is_empty() {
        println!("No records yet — add the first one with `mak add`.");
        return Ok(());
    }

    // 列宽（显示宽度）：中文按 2 列计算，保证中英混排对齐
    println!(
        "{} {} {} {} {} {}",
        table::pad_to("#", 4),
        table::pad_to("Name", 18),
        table::pad_to("Protocol", 12),
        table::pad_to("BASE URL", 34),
        table::pad_to("KEY", 20),
        table::pad_to("Check", 10),
    );
    println!("{}", "-".repeat(104));
    for (i, r) in rows.iter().enumerate() {
        let check = cache.get(&r.id).map(|e| e.status.label()).unwrap_or("-");
        println!(
            "{} {} {} {} {} {}",
            table::pad_to(&(i + 1).to_string(), 4),
            table::pad_to(&table::truncate_width(&r.name, 18), 18),
            table::pad_to(&table::truncate_width(&r.protocol.to_string(), 12), 12),
            table::pad_to(&table::truncate_width(&r.base_url, 34), 34),
            table::pad_to(&r.masked_key(), 20),
            table::pad_to(check, 10),
        );
    }
    println!("{}", "-".repeat(104));
    println!(
        "{} record(s) — `mak show <name|#index|id>` for full details",
        rows.len()
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
    println!("Protocol   : {}", r.protocol);
    println!("Base URL   : {}", r.base_url);
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
            println!("✓ API key copied to the clipboard\n  (the CLI cannot auto-clear it — clear the clipboard history after pasting)");
            Ok(())
        }
        Err(e) => {
            println!("⚠ Clipboard copy failed ({e}) — copy the API key above manually.");
            Ok(())
        }
    }
}

/// `edit`：逐项询问，回车保留原值。
pub fn edit(target: &str) -> Result<()> {
    let c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let mut data = load_vault(&c, &passphrase)?;
    let idx = resolve_target(&data, target)?;
    let record = &mut data.records[idx];

    println!(
        "Editing \"{}\" — press Enter to keep each current value.",
        record.name
    );
    let name = prompt_line(&format!("Name [{}]: ", record.name));
    if !name.is_empty() {
        record.name = name;
    }
    let url = prompt_line(&format!("Base URL [{}]: ", record.base_url));
    if !url.is_empty() {
        record.base_url = url;
    }
    let key = prompt::prompt_hidden("API key (Enter keeps current): ");
    if !key.is_empty() {
        record.api_key = key;
    }
    let notes_cur = record.notes.clone().unwrap_or_default();
    let notes = prompt_line(&format!("Notes [{notes_cur}]: "));
    // 输入 “-” 表示清空备注
    if notes == "-" {
        record.notes = None;
    } else if !notes.is_empty() {
        record.notes = Some(notes);
    }
    record.protocol = prompt::select_protocol(Some(&record.protocol));

    record.updated_at = chrono::Utc::now();
    let id = record.id;
    save_and_report(&c, &passphrase, &data)?;
    println!("✓ Updated \"{}\"", data.records[idx].name);
    run_probe(&c, &data, &id);
    Ok(())
}

/// `delete`：墓碑删除，需确认。
pub fn delete(target: &str) -> Result<()> {
    let c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;
    let mut data = load_vault(&c, &passphrase)?;
    let idx = resolve_target(&data, target)?;
    let name = data.records[idx].name.clone();

    if !confirm(&format!("Delete \"{name}\"? (recoverable via sync history within the grace period, then purged)"), false) {
        println!("Cancelled.");
        return Ok(());
    }
    data.records[idx].mark_deleted();
    save_and_report(&c, &passphrase, &data)?;
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
    for r in &targets {
        let (status, http) = probe::probe(r);
        cache.put(r.id, status, http);
        println!(
            "{} {} {} {}",
            table::pad_to(status.label(), 12),
            table::pad_to(&table::truncate_width(&r.name, 18), 18),
            r.base_url,
            http.map(|h| format!("(HTTP {h})")).unwrap_or_default()
        );
    }
    cache.save(&c.dirs)?;
    Ok(())
}

/// 对单条记录探测并落缓存（add/edit 后自动调用）。
fn run_probe(c: &Ctx, data: &VaultData, id: &uuid::Uuid) {
    let Some(r) = data.records.iter().find(|r| r.id == *id) else {
        return;
    };
    let (status, http) = probe::probe(r);
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
    let c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    let (passphrase, _) = prompt::unlock(&c.dirs)?;

    let out = sync::sync(&c.store, &c.dirs, &passphrase, c.config.tombstone_days)?;

    match (&out.remote, out.pulled, out.pushed) {
        (None, _, _) => println!(
            "Standalone mode (no remote) — nothing to sync.\n  Bind one with: mak remote set <url>"
        ),
        (_, true, true) => println!(
            "✓ Synced: pulled remote changes{} and pushed",
            if out.merged {
                " (merged divergent edits)"
            } else {
                ""
            }
        ),
        (_, true, false) => println!("✓ Pulled remote changes (nothing local to push)"),
        (_, false, true) => println!("✓ Pushed local changes to remote"),
        (Some(_), false, false) => println!("✓ Up to date (local matches remote)"),
    }
    if let Some(err) = &out.push_error {
        println!("⚠ Push failed: {err}");
    }
    println!("{} active record(s)", out.alive_records);

    // 更新“上次同步时间”
    let mut cfg = c.config.clone();
    cfg.last_sync_at = Some(chrono::Utc::now().to_rfc3339());
    cfg.save(&c.dirs)?;
    Ok(())
}

/// `status`：仓库与同步状态总览。
pub fn status() -> Result<()> {
    let c = ctx()?;
    if !c.dirs.is_initialized() {
        println!("Not initialized — start with: mak init");
        return Ok(());
    }
    let info = sync::status_summary(&c.store)?;

    println!("Repository : {}", c.dirs.repo_dir.display());
    match &info.remote {
        Some(u) => println!("Remote     : {u}"),
        None => println!("Remote     : none (standalone mode)"),
    }
    println!("Unpushed   : {} commit(s)", info.ahead);
    println!("To pull    : {} commit(s)", info.behind);
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
        println!("Records    : hidden (passphrase not remembered; run `mak sync` to see)");
    }
    Ok(())
}

/// `remote set/show`。
pub fn remote(action: RemoteAction) -> Result<()> {
    let c = ctx()?;
    require_init(&c)?;
    match action {
        RemoteAction::Set { url } => {
            c.store.set_remote(&url)?;
            println!("✓ Remote set: {url}");
            println!("  Sync now with: mak sync");
        }
        RemoteAction::Show => match c.store.remote_url()? {
            Some(u) => println!("{u}"),
            None => println!("(no remote bound)"),
        },
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// passwd / forget
// ---------------------------------------------------------------------------

/// `passwd`：修改主密码（重加密整个 vault 并推送，同步更新凭据库）。
pub fn passwd() -> Result<()> {
    let c = ctx()?;
    require_init(&c)?;
    let _guard = lock::acquire(&c.dirs)?;
    let (old, _) = prompt::unlock(&c.dirs)?;
    let data = load_vault(&c, &old)?;

    let new = new_passphrase()?;
    sync::save(&c.store, &c.dirs, &new, &data, c.config.auto_push)?;

    // 凭据库同步：统一按 TTL 模型记住新密码（remember_days = 0 时跳过）
    let remembered = c.config.remember_days > 0
        && keychain::set_ttl(&c.dirs, &new, c.config.remember_days).is_ok();
    println!("✓ Master passphrase changed — vault re-encrypted and saved");
    if remembered {
        println!("  Credential store updated.");
    }
    println!("  Other devices will need the new passphrase on their next sync.");
    Ok(())
}

/// `forget`：立即锁定（清除记住的主密码）；`--github` 只清除 GitHub token。
///
/// TTL 模型下的语义：forget = "现在锁起来"。下次命令会要求输入一次，
/// 输入后又开始新的记住周期（默认 7 天）——适合借出电脑前手动上锁。
pub fn forget(github: bool) -> Result<()> {
    if github {
        match myapikey_core::github::keychain_delete_token()? {
            true => println!(
                "✓ GitHub authorization cleared — git now uses your local git credentials."
            ),
            false => println!("No saved GitHub authorization on this machine."),
        }
        return Ok(());
    }
    let c = ctx()?;
    match keychain::delete(&c.dirs)? {
        true => println!("✓ Locked — the passphrase was cleared from this machine.\n  The next command asks for it once, then remembers it again."),
        false => println!("No remembered passphrase on this machine."),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// export / import
// ---------------------------------------------------------------------------

/// `export`：明文导出（两次确认 + 0600 权限）。
pub fn export(path: &Path) -> Result<()> {
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
        "⚠ About to write {} record(s) with PLAINTEXT API keys to:",
        alive.len()
    );
    println!("  {}", path.display());
    println!("  A plaintext file is a leak risk — delete it as soon as you are done.");
    if !confirm("Export plaintext?", false)
        || !confirm("Really continue despite the leak risk?", false)
    {
        println!("Cancelled.");
        return Ok(());
    }

    let json = serde_json::to_vec_pretty(&alive)
        .map_err(|e| Error::Serde(format!("export serialization failed: {e}")))?;
    std::fs::write(path, &json)?;
    restrict_permissions(path);
    println!(
        "✓ Exported to {}\n  Contains plaintext keys — delete it when done",
        path.display()
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
    let c = ctx()?;
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
    save_and_report(&c, &passphrase, &data)?;
    println!("✓ Import done — added {added}, updated {updated} (newer updated_at wins)");
    Ok(())
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------
// 表格对齐与截断统一在 table 模块（按显示宽度，中文占 2 列）。
