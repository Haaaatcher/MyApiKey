//! # myapikey-gui 核心
//!
//! 会话状态与全部业务操作。`commands.rs` 里的 `#[tauri::command]`
//! 只是薄封装，业务逻辑全部在本模块——因此可以脱离 Tauri 运行时做
//! 集成测试（见文末 tests）。
//!
//! 安全约束（与 CLI 一致，见 AGENTS.md）：
//! - `api_key` 明文只存在于：内存中的会话、`reveal_key`（用户显式点击）
//!   与剪贴板（`copy_key`，30 秒后尽力清除）；发给前端的列表数据一律掩码；
//! - 主密码只在内存（会话）与凭据库（TTL 记忆）；
//! - 错误信息一律英文且不包含任何凭据。

use myapikey_core::check_cache::CheckCache;
use myapikey_core::config::AppConfig;
use myapikey_core::github;
use myapikey_core::keychain;
use myapikey_core::model::{Protocol, Record, VaultData};
use myapikey_core::paths::AppDirs;
use myapikey_core::sync::{self, SyncOutcome};
use myapikey_core::vault;
use myapikey_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Tauri command 薄封装（业务全部在本文件顶层函数中）。
pub mod commands;
pub mod update;

/// Tauri manage / State 使用的状态类型。
pub type SharedState = std::sync::Arc<Shared>;

/// 剪贴板自动清除的延迟（设计 §8：30 秒后尽力清空）。
const CLIPBOARD_CLEAR_AFTER: Duration = Duration::from_secs(30);

/// 进程级共享状态（由 Tauri manage 持有）。
pub struct Shared {
    /// 目录布局（MYAPIKEY_HOME 可覆盖，便于测试）。
    pub dirs: AppDirs,
    /// 会话与活动时间戳。互斥锁短持有，业务里的网络/git 操作在锁外完成。
    pub inner: Mutex<Inner>,
    /// GUI 内写命令的串行闸门（2026-10-26 审查修复）。Tauri 异步命令可
    /// 并发进入，两个保存交错时后回写会话的一方会整体覆盖对方；save /
    /// delete / sync 全程持有此锁互斥（命令跑在 spawn_blocking 线程，
    /// 长持 std::sync::Mutex 无 await 死锁风险）。
    pub write: Mutex<()>,
}

/// 锁内状态。
pub struct Inner {
    /// 解锁后的会话；`None` = 锁定（未解锁或已自动锁）。
    pub session: Option<Session>,
    /// 最近一次操作时间（自动锁的计时依据；每次 ops 调用刷新）。
    pub last_activity: Instant,
    /// 进行中的 GitHub 设备授权（init 阶段短暂存在，仅内存）。
    pub pending_device: Option<PendingDevice>,
    /// 进行中的初始化探测（D25 交互对齐：授权/绑定后、密码表单前的
    /// 中间态——远端已探明，等前端收主密码完成接入/建基线）。
    pub pending_init: Option<PendingInit>,
}

/// 初始化探测中间态：token 已就绪、远端已探明，等主密码完成收尾。
#[derive(Clone)]
pub struct PendingInit {
    /// 解析后的仓库（owner/name）。
    pub repo_id: String,
    /// 授权令牌（finish 阶段上传基线用；接入场景不需上传但记账写盘
    /// 后 remote 已绑定，保留以备 sync）。
    pub token: String,
    /// 远端当前密文（`None` = 空仓库/未创建，finish 时上传基线）。
    pub head: Option<(String, Vec<u8>)>,
}

/// 解锁会话：明文主密码与解密后的数据。
pub struct Session {
    /// 主密码（保存/同步时重加密用）。
    pub passphrase: String,
    /// 当前数据（写操作后即时更新）。
    pub data: VaultData,
}

/// 设备授权中间态。`dc.device_code` 是机密，绝不能发给前端。
#[derive(Clone)]
pub struct PendingDevice {
    /// OAuth client_id。
    pub client_id: String,
    /// 设备码信息（轮询用，整体保留以复用 expires_in/interval）。
    pub dc: github::DeviceCode,
}

impl Shared {
    /// 组装共享状态：路径 + 配置 + 已存 GitHub token 的 git 认证。
    ///
    /// # Errors
    /// 路径解析失败（如 MYAPIKEY_HOME 非法）。
    pub fn new() -> Result<Self> {
        let dirs = AppDirs::load()?;
        Ok(Self {
            write: Mutex::new(()),
            inner: Mutex::new(Inner {
                session: None,
                last_activity: Instant::now(),
                pending_device: None,
                pending_init: None,
            }),
            dirs,
        })
    }

    /// 读当前配置（文件缺失时给默认值）。
    ///
    /// 刻意不缓存（2026-10-26 评估后放弃审查建议的缓存优化）：文档承诺
    /// 进阶用户手改 config.toml 即时生效（如 tombstone_days），缓存会让
    /// 它直到重启才生效；自动锁线程每 15 秒一次的小文件读，代价可忽略。
    pub fn config(&self) -> AppConfig {
        AppConfig::load(&self.dirs).unwrap_or_default()
    }
}

/// 组装同步通道：config 里的远端仓库 + keychain 现读 token（D25）。
///
/// token **每次现读**而非启动时缓存——凭据库可能被另一端刷新（CLI 重新
/// 授权、token 失效自愈），现读让"刷了就能用"成为结构保证而非修补。
///
/// 返回 `None` = 未绑定远端（单机模式）。
///
/// # Errors
/// remote 地址不是 GitHub 形态。
fn remote_of(sh: &Shared) -> Result<Option<Box<dyn myapikey_core::remote::SyncStore>>> {
    let Some(url) = sh.config().remote_repo.clone() else {
        return Ok(None);
    };
    let Some(token) = myapikey_core::github::keychain_token()? else {
        return Err(Error::Usage(
            "a remote is bound but no GitHub authorization exists on this machine.              Re-run the GitHub setup from the settings page"
                .into(),
        ));
    };
    Ok(Some(sync::open_remote(&url, &token)?))
}

/// 用系统默认浏览器打开外部链接（前端授权页链接点击时调用——
/// WebView 内导航会离开应用，必须交给系统浏览器）。
///
/// # Errors
/// 系统打开浏览器失败（无头环境等）。
pub fn open_external(url: &str) -> Result<()> {
    github::open_browser(url)
}

/// 写路径认证失败时的自动续期：keychain 有刷新令牌 → 静默换新 access
/// token（+ 轮换后的刷新令牌）。返回是否续期成功（成功后调用方重试
/// 一次写操作——重试会现读新凭据；失败维持原错误与重授权指引）。
fn try_refresh_and_retry_setup(_sh: &Shared) -> bool {
    // client_id 取环境变量覆盖（2026-10-31 审查采纳：原硬编码会让覆盖
    // 部署下的刷新必败）；取失败视为不可刷新，维持原错误
    let Ok(client_id) = github::effective_client_id() else {
        return false;
    };
    myapikey_core::github::try_refresh_and_store(&client_id)
}

// ---------------------------------------------------------------------------
// 视图模型（发给前端的数据，key 一律掩码）
// ---------------------------------------------------------------------------

/// 单条记录的前端视图。
#[derive(Debug, Clone, Serialize)]
pub struct RecordView {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    /// 掩码 key（如 `sk-p...0000`）。
    pub masked_key: String,
    /// 模型列表（2026-10-27 增补，可空）。
    pub models: Vec<String>,
    pub notes: Option<String>,
    pub updated_at: String,
    /// 最近一次校验结论（本机缓存；`None` = 从未校验）。
    pub check: Option<CheckView>,
}

/// 校验结论视图。
#[derive(Debug, Clone, Serialize)]
pub struct CheckView {
    /// 机器可读状态（snake_case）。
    pub status: String,
    /// 展示标签（英文）。
    pub label: String,
    pub http: Option<u16>,
}

/// 应用启动时前端需要的一次性状态。
#[derive(Debug, Serialize)]
pub struct GuiState {
    pub initialized: bool,
    pub unlocked: bool,
    /// 记忆中的主密码是否可用（决定“一键解锁”按钮）。
    pub has_remembered: bool,
}

/// `mak sync` 的前端回显。
#[derive(Debug, Serialize, Default)]
pub struct SyncView {
    pub pulled: bool,
    pub merged: bool,
    pub pushed: bool,
    pub push_error: Option<String>,
    pub alive: usize,
}

/// 状态页数据。
#[derive(Debug, Serialize)]
pub struct StatusView {
    pub remote: Option<String>,
    /// 本地有未推送修改（保存过但未成功上传）。
    pub pending: bool,
    pub last_sync: Option<String>,
    pub alive: usize,
}

/// 设置页数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsView {
    pub remote: Option<String>,
    pub tombstone_days: u32,
    pub auto_push: bool,
    pub remember_days: u32,
    pub auto_lock_minutes: u32,
}

/// 初始化的第一步返回：直接完成，或需要浏览器授权。
#[derive(Debug, Serialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum InitStage {
    /// 全部完成（无 GitHub 或 token 已存在直接复用）。
    /// `refresh = true` 表示这只是**授权刷新**（已初始化机器的 token 失效
    /// 自愈，2026-10-26 补齐闭环：vault 未动），前端据此选文案。
    Done {
        joined: bool,
        records: usize,
        #[serde(default)]
        refresh: bool,
    },
    /// 需要用户在浏览器完成设备授权，然后调 `init_device_authorized`。
    DeviceCode {
        user_code: String,
        verification_uri: String,
    },
    /// 探测完成（授权/绑定后）：远端状态已明确，前端据此显示密码表单
    /// —— `has_data = true` 显示"输入主密码接入"；false 显示"设置新的
    /// 主密码"。用户提交后调 `init_finish` 收尾。
    ProbeDone { has_data: bool, repo: String },
}

/// 记录编辑表单（前端提交）。
#[derive(Debug, Deserialize)]
pub struct RecordInput {
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub api_key: String,
    /// 模型列表（2026-10-27 增补）：`None` = 未提交该字段（编辑时保持
    /// 原值）；`Some(vec)` 逐项 trim、去空后整体替换。
    pub models: Option<Vec<String>>,
    pub notes: Option<String>,
}

// ---------------------------------------------------------------------------
// 业务操作（ops）——commands.rs 逐个包装
// ---------------------------------------------------------------------------

/// 刷新活动时间并拿锁内可变引用的统一入口。
fn enter(inner: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
    g.last_activity = Instant::now();
    g
}

/// 取会话（未解锁时返回友好错误）。
fn session_of(g: &mut Inner) -> std::result::Result<&mut Session, Error> {
    g.session
        .as_mut()
        .ok_or_else(|| Error::Usage("locked, unlock first".into()))
}

/// 启动状态。
///
/// # Errors
/// 目录解析类错误（极少）。
pub fn state_view(sh: &Shared) -> Result<GuiState> {
    let has_remembered = keychain::get_with_expiry(&sh.dirs).ok().flatten().is_some();
    let unlocked = enter(&sh.inner).session.is_some();
    Ok(GuiState {
        initialized: sh.dirs.is_initialized(),
        unlocked,
        has_remembered,
    })
}

/// 手动输入主密码解锁；成功后按 TTL 记忆。
///
/// # Errors
/// - [`Error::NotInitialized`] / [`Error::Decrypt`]：与 CLI 相同语义。
pub fn unlock_with(sh: &Shared, passphrase: &str) -> Result<()> {
    if !sh.dirs.vault_path.is_file() {
        return Err(Error::NotInitialized);
    }
    let bytes = std::fs::read(&sh.dirs.vault_path)?;
    let data = vault::decrypt(&bytes, passphrase)?;
    let cfg = sh.config();
    if cfg.remember_days > 0 {
        // 记不住（无头环境等）不阻塞解锁，前端可提示
        let _ = keychain::set_ttl(&sh.dirs, passphrase, cfg.remember_days);
    }
    let mut g = enter(&sh.inner);
    g.session = Some(Session {
        passphrase: passphrase.to_string(),
        data,
    });
    Ok(())
}

/// 一键解锁：尝试记忆中的主密码。
///
/// # Returns
/// `Ok(true)` 解锁成功；`Ok(false)` 无可用记忆（前端回退到手动输入）。
///
/// # Errors
/// vault 文件异常等。
pub fn unlock_remembered(sh: &Shared) -> Result<bool> {
    let Some(p) = keychain::get_with_expiry(&sh.dirs)? else {
        return Ok(false);
    };
    if !sh.dirs.vault_path.is_file() {
        return Err(Error::NotInitialized);
    }
    let bytes = std::fs::read(&sh.dirs.vault_path)?;
    match vault::decrypt(&bytes, &p) {
        Ok(data) => {
            let mut g = enter(&sh.inner);
            g.session = Some(Session {
                passphrase: p,
                data,
            });
            Ok(true)
        }
        Err(_) => {
            // 记忆已失效（他机改密）：清理后要求手动输入
            let _ = keychain::delete(&sh.dirs);
            Ok(false)
        }
    }
}

/// 手动锁定（Lock 按钮）。
///
/// 2026-10-24 决议（主人）：**锁定 = 必须重新输入主密码**。除销毁会话外
/// 还要清除"记住的密码"（等价 `mak forget`）——否则一键解锁会让锁定
/// 形同虚设：任何能碰到键盘的人点一下就能重新进入。
/// 仅当本机确实记过密码（`remember.json` 存在）才动凭据库——既避免无谓
/// 写操作，也保证从不记忆的测试环境不触碰凭据库。
pub fn lock(sh: &Shared) {
    clear_remembered_if_any(sh);
    let mut g = enter(&sh.inner);
    g.session = None;
    g.pending_device = None;
    // 初始化中间态（含令牌与远端意图）同样随锁定作废（2026-10-31
    // 审查可选采纳：此前驻留内存到进程结束）
    g.pending_init = None;
}

/// 清除记住的主密码（remember.json + 凭据库条目）。
fn clear_remembered_if_any(sh: &Shared) {
    if sh.dirs.config_dir.join("remember.json").exists() {
        let _ = keychain::delete(&sh.dirs);
    }
}

/// 空闲自动锁：由后台线程周期调用。返回是否执行了锁定。
///
/// 与手动锁定语义一致（2026-10-24 决议）：自动锁同样清除记住的密码——
/// 空闲锁定的目的就是防离开座位后的顺手操作，一键解锁会使其失效。
/// 代价是每次自动锁定后需要重输一次密码（成功输入后按 TTL 重新记住）。
pub fn lock_if_idle(sh: &Shared, idle_limit: Duration) -> bool {
    let should_lock = {
        let g = sh.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.session.is_some() && g.last_activity.elapsed() >= idle_limit
    };
    if !should_lock {
        return false;
    }
    // 凭据库清理在互斥锁外做（可能涉及系统调用），锁内只动内存
    clear_remembered_if_any(sh);
    let mut g = sh.inner.lock().unwrap_or_else(|p| p.into_inner());
    g.session = None;
    true
}

/// 存活记录视图（掩码 + 本机校验缓存），按最近修改排序。
///
/// # Errors
/// 未解锁。
pub fn list_view(sh: &Shared) -> Result<Vec<RecordView>> {
    let mut g = enter(&sh.inner);
    let session = session_of(&mut g)?;
    let cache = CheckCache::load(&sh.dirs);
    let mut views: Vec<RecordView> = session
        .data
        .alive_records()
        .map(|r| to_view(r, &cache))
        .collect();
    views.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(views)
}

/// 搜索视图：core::search 评分过滤 + 排序（2026-10-26 搜索下沉核心层，
/// 前端不再维护第二份评分实现）。空查询等价于 [`list_view`] 的排序。
///
/// # Errors
/// 未解锁。
pub fn search_view(sh: &Shared, query: &str) -> Result<Vec<RecordView>> {
    // 校验缓存是本机小 JSON，进锁前加载；视图在锁内构建完毕再返回，
    // 避免把 Record 引用带出锁的生命周期
    let cache = CheckCache::load(&sh.dirs);
    let mut g = enter(&sh.inner);
    let session = session_of(&mut g)?;
    let query = query.trim();
    Ok(if query.is_empty() {
        let mut v: Vec<RecordView> = session
            .data
            .alive_records()
            .map(|r| to_view(r, &cache))
            .collect();
        v.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        v
    } else {
        myapikey_core::search::search(session.data.alive_records(), query)
            .into_iter()
            .map(|r| to_view(r, &cache))
            .collect()
    })
}

fn to_view(r: &Record, cache: &CheckCache) -> RecordView {
    let check = cache.get(&r.id).map(|e| CheckView {
        status: serde_json::to_value(e.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        label: e.status.label().to_string(),
        http: e.http,
    });
    RecordView {
        id: r.id.to_string(),
        name: r.name.clone(),
        protocol: r.protocol.to_string(),
        base_url: r.base_url.clone(),
        masked_key: r.masked_key(),
        models: r.models.clone(),
        notes: r.notes.clone(),
        updated_at: r.updated_at.format("%Y-%m-%d %H:%M:%S").to_string(),
        check,
    }
}

/// 新增/编辑记录（`editing_id = None` 为新增）。返回保存后的视图。
///
/// # Errors
/// 未解锁 / 字段为空 / 保存失败。
pub fn save_record(
    sh: &Shared,
    input: RecordInput,
    editing_id: Option<&str>,
) -> Result<RecordView> {
    let name = input.name.trim().to_string();
    let base_url = input.base_url.trim().to_string();
    let api_key = input.api_key.trim().to_string();
    if name.is_empty() {
        return Err(Error::Usage("name must not be empty".into()));
    }
    if base_url.is_empty() {
        return Err(Error::Usage("base URL must not be empty".into()));
    }
    // 基础格式校验（2026-10-26 需求）：只拒明显非法，不改写输入
    if !myapikey_core::store::is_valid_http_url(&base_url) {
        return Err(Error::Usage(
            "invalid base URL: must start with http:// or https:// followed by a host".into(),
        ));
    }
    // 空 key 仅在"编辑"时合法（= 保持原 key 不变）；新建必须有 key
    if api_key.is_empty() && editing_id.is_none() {
        return Err(Error::Usage("API key must not be empty".into()));
    }
    let protocol = Protocol::from_label(&input.protocol);
    let notes = input
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let models = input.models.map(|ms| {
        ms.iter()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect::<Vec<_>>()
    });

    // 写命令串行闸门：全函数持有，防并发保存互相覆盖会话
    let _write_guard = sh.write.lock().unwrap_or_else(|p| p.into_inner());
    // 在会话数据的**克隆**上应用修改（2026-10-26 审查修复：此前直接改
    // session.data，落盘失败后 UI 显示新值而磁盘是旧值，且下次保存会把
    // 未确认的数据静默写出）。落盘成功才回写会话，失败则会话保持旧值
    let (passphrase, data, updated_id) = {
        let mut g = enter(&sh.inner);
        let session = session_of(&mut g)?;
        let mut data = session.data.clone();
        let updated = match editing_id {
            Some(id_str) => {
                let id = uuid::Uuid::parse_str(id_str)
                    .map_err(|_| Error::Usage("invalid record id".into()))?;
                let r = data
                    .records
                    .iter_mut()
                    .find(|r| r.id == id)
                    .ok_or_else(|| Error::NotFound(id_str.to_string()))?;
                r.name = name;
                r.protocol = protocol;
                r.base_url = base_url;
                // 空 key = 前端 key 字段保持掩码占位未动（2026-10-25 优化：
                // 改备注/名称等不再强制点 Reveal）——保留原 key，只更新其他字段
                if !api_key.is_empty() {
                    r.api_key = api_key;
                }
                if let Some(ms) = models {
                    r.models = ms;
                }
                r.notes = notes;
                r.updated_at = chrono::Utc::now();
                r.id
            }
            None => {
                let mut r = Record::new(name, protocol, base_url, api_key);
                // Record::new 不接收备注——新建时显式补上（曾在此整个丢弃
                // notes，新建记录的备注保存后消失，2026-10-25 反馈）
                r.notes = notes;
                r.models = models.unwrap_or_default();
                let id = r.id;
                data.records.push(r);
                id
            }
        };
        (session.passphrase.clone(), data, updated)
    };

    // 跨进程实例锁（与 CLI 互斥）内完成落盘
    {
        let _instance = myapikey_core::lock::acquire(&sh.dirs)?;
        let mut cfg = sh.config();
        let remote = remote_of(sh)?;
        let remote_dyn: Option<&dyn myapikey_core::remote::SyncStore> = remote
            .as_ref()
            .map(|r| r as &dyn myapikey_core::remote::SyncStore);
        // 认证失败 → 刷新令牌静默续期 → 重试一次（重试会现读新凭据）
        match sync::save(
            &sh.dirs,
            &passphrase,
            &data,
            remote_dyn,
            cfg.auto_push,
            &mut cfg,
        ) {
            Err(e)
                if myapikey_core::sync::is_auth_error(&e.to_string())
                    && try_refresh_and_retry_setup(sh) =>
            {
                let mut cfg = sh.config();
                let remote = remote_of(sh)?;
                let remote_dyn: Option<&dyn myapikey_core::remote::SyncStore> = remote
                    .as_ref()
                    .map(|r| r as &dyn myapikey_core::remote::SyncStore);
                sync::save(
                    &sh.dirs,
                    &passphrase,
                    &data,
                    remote_dyn,
                    cfg.auto_push,
                    &mut cfg,
                )?;
            }
            other => {
                other?;
            }
        }
    }

    // 回写会话 + 返回视图
    let mut g = enter(&sh.inner);
    let session = session_of(&mut g)?;
    session.data = data;
    let r = session
        .data
        .records
        .iter()
        .find(|r| r.id == updated_id)
        .ok_or_else(|| Error::NotFound(updated_id.to_string()))?;
    let view = to_view(r, &CheckCache::load(&sh.dirs));
    Ok(view)
}

/// 删除记录（墓碑）。
///
/// # Errors
/// 未解锁 / id 无效。
pub fn delete_record(sh: &Shared, id_str: &str) -> Result<()> {
    // 写闸门 + 克隆改 + 实例锁内落盘（与 save_record 同模式，防并发
    // 覆盖与跨进程写冲突）
    let _write_guard = sh.write.lock().unwrap_or_else(|p| p.into_inner());
    let (passphrase, mut data) = {
        let mut g = enter(&sh.inner);
        let session = session_of(&mut g)?;
        (session.passphrase.clone(), session.data.clone())
    };
    let id = uuid::Uuid::parse_str(id_str).map_err(|_| Error::Usage("invalid record id".into()))?;
    let r = data
        .records
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or_else(|| Error::NotFound(id_str.to_string()))?;
    r.mark_deleted();
    {
        let _instance = myapikey_core::lock::acquire(&sh.dirs)?;
        let mut cfg = sh.config();
        let remote = remote_of(sh)?;
        let remote_dyn: Option<&dyn myapikey_core::remote::SyncStore> = remote
            .as_ref()
            .map(|r| r as &dyn myapikey_core::remote::SyncStore);
        // 认证失败 → 刷新令牌静默续期 → 重试一次（重试会现读新凭据）
        match sync::save(
            &sh.dirs,
            &passphrase,
            &data,
            remote_dyn,
            cfg.auto_push,
            &mut cfg,
        ) {
            Err(e)
                if myapikey_core::sync::is_auth_error(&e.to_string())
                    && try_refresh_and_retry_setup(sh) =>
            {
                let mut cfg = sh.config();
                let remote = remote_of(sh)?;
                let remote_dyn: Option<&dyn myapikey_core::remote::SyncStore> = remote
                    .as_ref()
                    .map(|r| r as &dyn myapikey_core::remote::SyncStore);
                sync::save(
                    &sh.dirs,
                    &passphrase,
                    &data,
                    remote_dyn,
                    cfg.auto_push,
                    &mut cfg,
                )?;
            }
            other => {
                other?;
            }
        }
    }
    let mut g = enter(&sh.inner);
    session_of(&mut g)?.data = data;
    Ok(())
}

/// 显式查看完整 key（编辑抽屉的 reveal 按钮；用户主动行为）。
///
/// # Errors
/// 未解锁 / 未找到。
pub fn reveal_key(sh: &Shared, id_str: &str) -> Result<String> {
    let mut g = enter(&sh.inner);
    let session = session_of(&mut g)?;
    let id = uuid::Uuid::parse_str(id_str).map_err(|_| Error::Usage("invalid record id".into()))?;
    session
        .data
        .records
        .iter()
        .find(|r| r.id == id && !r.is_deleted())
        .map(|r| r.api_key.clone())
        .ok_or_else(|| Error::NotFound(id_str.to_string()))
}

/// 复制完整 key 到剪贴板，30 秒后尽力清除（内容未被覆盖时）。
///
/// # Errors
/// 未解锁 / 剪贴板不可用。
pub fn copy_key(sh: &Shared, id_str: &str) -> Result<()> {
    let key = reveal_key(sh, id_str)?;
    copy_and_auto_clear(key)
}

/// 复制记录的 Base URL 到剪贴板（字段级复制，2026-10-25 需求：
/// base_url 与 api_key 成对使用，各自提供复制入口）。
/// URL 非敏感，不做延时清除——只有 key 享受 30 秒自动清除。
///
/// # Errors
/// 未解锁 / 未找到 / 剪贴板不可用。
pub fn copy_base_url(sh: &Shared, id_str: &str) -> Result<String> {
    let mut g = enter(&sh.inner);
    let session = session_of(&mut g)?;
    let id = uuid::Uuid::parse_str(id_str).map_err(|_| Error::Usage("invalid record id".into()))?;
    let url = session
        .data
        .records
        .iter()
        .find(|r| r.id == id && !r.is_deleted())
        .map(|r| r.base_url.clone())
        .ok_or_else(|| Error::NotFound(id_str.to_string()))?;
    drop(g);
    let mut cb = arboard::Clipboard::new()
        .map_err(|e| Error::Usage(format!("clipboard unavailable: {e}")))?;
    cb.set_text(url.clone())
        .map_err(|e| Error::Usage(format!("clipboard write failed: {e}")))?;
    Ok(url)
}

/// 复制任意文本到剪贴板（不做延时清除；用于授权码这类短期非敏感值，
/// 2026-10-25 需求：设备授权页一键复制授权码）。
///
/// # Errors
/// 剪贴板不可用。
pub fn copy_plain(text: &str) -> Result<()> {
    let mut cb = arboard::Clipboard::new()
        .map_err(|e| Error::Usage(format!("clipboard unavailable: {e}")))?;
    cb.set_text(text.to_string())
        .map_err(|e| Error::Usage(format!("clipboard write failed: {e}")))?;
    Ok(())
}

/// 复制并在延时后清除剪贴板（独立函数便于测试语义）。
pub fn copy_and_auto_clear(text: String) -> Result<()> {
    let mut cb = arboard::Clipboard::new()
        .map_err(|e| Error::Usage(format!("clipboard unavailable: {e}")))?;
    cb.set_text(text.clone())
        .map_err(|e| Error::Usage(format!("clipboard write failed: {e}")))?;
    // 后台线程延时清除；只清“仍是我们复制的内容”，不误删用户后续复制的其他东西
    std::thread::spawn(move || {
        std::thread::sleep(CLIPBOARD_CLEAR_AFTER);
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if let Ok(current) = cb.get_text() {
                if current == text {
                    let _ = cb.set_text(String::new());
                }
            }
        }
    });
    Ok(())
}

/// 校验一条记录并写入本机缓存。
///
/// # Errors
/// 未解锁 / 未找到。
pub fn check_key(sh: &Shared, id_str: &str) -> Result<CheckView> {
    let record = {
        let mut g = enter(&sh.inner);
        let session = session_of(&mut g)?;
        let id =
            uuid::Uuid::parse_str(id_str).map_err(|_| Error::Usage("invalid record id".into()))?;
        session
            .data
            .records
            .iter()
            .find(|r| r.id == id && !r.is_deleted())
            .cloned()
            .ok_or_else(|| Error::NotFound(id_str.to_string()))?
    };
    let (status, http) = myapikey_core::probe::probe(&record);
    let mut cache = CheckCache::load(&sh.dirs);
    cache.put(record.id, status, http);
    cache.save(&sh.dirs)?;
    Ok(CheckView {
        status: serde_json::to_value(status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        label: status.label().to_string(),
        http,
    })
}

/// 批量检测结果（带记录 id，前端据此更新对应卡片的校验 chip）。
#[derive(Debug, Serialize)]
pub struct CheckResultView {
    pub id: String,
    pub status: String,
    pub label: String,
    pub http: Option<u16>,
}

/// 校验全部记录（核心层并发探测，2026-10-31 一键检测全部）并写入
/// 本机缓存。结论与 [`check_key`] 同源；返回顺序与记录列表一致。
///
/// `progress`：每条探测完成即推送一条结果（Tauri ipc Channel）——
/// 前端据此逐条点亮卡片 chip，而不是等全部完成后一次性弹出提示
/// （2026-10-31 过程反馈改进）。回调按完成顺序触发（并发下乱序），
/// 前端按 id 归位。
///
/// # Errors
/// 未解锁。
pub fn check_all_keys(
    sh: &Shared,
    progress: &tauri::ipc::Channel<CheckResultView>,
) -> Result<Vec<CheckResultView>> {
    let records: Vec<_> = {
        let mut g = enter(&sh.inner);
        let session = session_of(&mut g)?;
        session
            .data
            .records
            .iter()
            .filter(|r| !r.is_deleted())
            .cloned()
            .collect()
    };
    let view_of = |status: &myapikey_core::probe::CheckStatus, http: Option<u16>| CheckResultView {
        id: String::new(),
        status: serde_json::to_value(status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        label: status.label().to_string(),
        http,
    };
    let results = myapikey_core::probe::probe_all_with(
        &records,
        myapikey_core::probe::PROBE_CONCURRENCY,
        &mut |id, status, http| {
            let mut v = view_of(&status, http);
            v.id = id.to_string();
            let _ = progress.send(v);
        },
    );
    let mut cache = CheckCache::load(&sh.dirs);
    let mut out = Vec::with_capacity(records.len());
    for (r, (_id, status, http)) in records.iter().zip(&results) {
        cache.put(r.id, *status, *http);
        out.push(CheckResultView {
            id: r.id.to_string(),
            status: serde_json::to_value(status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            label: status.label().to_string(),
            http: *http,
        });
    }
    cache.save(&sh.dirs)?;
    Ok(out)
}

/// 同步（pull + 合并 + push），成功后更新 last_sync_at 与会话数据。
///
/// # Errors
/// 未解锁 / git 失败 / 远端密码不一致。
pub fn sync_now(sh: &Shared) -> Result<SyncView> {
    // 写闸门：sync 会重写 vault 并回写会话，与其他写命令互斥
    let _write_guard = sh.write.lock().unwrap_or_else(|p| p.into_inner());
    let (passphrase, _) = {
        let mut g = enter(&sh.inner);
        let session = session_of(&mut g)?;
        (session.passphrase.clone(), ())
    };
    let mut cfg = sh.config();
    // 跨进程实例锁内完成整个下载/合并/上传（网络耗时段也持锁——
    // CLI 侧同一时段的写会等待而非交错）
    let out: SyncOutcome = {
        let _instance = myapikey_core::lock::acquire(&sh.dirs)?;
        let remote = remote_of(sh)?;
        // 单机模式没有远端可对账：明确拒绝而非静默 no-op——静默成功
        // 会被渲染成"已是最新"，暗示和远端对过账（2026-10-31 修复）。
        // 正常路径按钮已按 status.remote 禁用，这里是绕过时的兜底
        let Some(remote) = remote else {
            return Err(Error::Usage(
                "standalone mode: no remote is bound to sync with. \
                 Bind one on the settings page"
                    .into(),
            ));
        };
        sync::sync(&sh.dirs, &passphrase, &remote, cfg.tombstone_days, &mut cfg)?
    };

    // 同步状态（含 last_sync_at）一律由 core 的 mark_synced 在"达成
    // 一致"时记账，此处不再手写——单机/推送失败等未达成一致的场景
    // 不应留下"刚同步过"的痕迹

    // 重新加载合并后的数据进会话（sync 后 vault 文件已是最新）
    let bytes = std::fs::read(&sh.dirs.vault_path)?;
    let data = vault::decrypt(&bytes, &passphrase)?;
    let alive = data.alive_records().count();
    let mut g = enter(&sh.inner);
    if let Some(s) = &mut g.session {
        s.data = data;
    }
    Ok(SyncView {
        pulled: out.pulled,
        merged: out.merged,
        pushed: out.pushed,
        push_error: out.push_error,
        alive,
    })
}

/// 状态页。
///
/// # Errors
/// git 状态查询失败。
pub fn status_view(sh: &Shared) -> Result<StatusView> {
    let info = sync::status_summary(&sh.dirs, &sh.config())?;
    let alive = enter(&sh.inner)
        .session
        .as_ref()
        .map(|s| s.data.alive_records().count())
        .unwrap_or(0);
    Ok(StatusView {
        remote: info.remote,
        pending: info.pending,
        last_sync: sh.config().last_sync_at,
        alive,
    })
}

// ---------------------------------------------------------------------------
// M5 生态集成：供给 = 写配置文件（D21，2026-10-27）
// ---------------------------------------------------------------------------

/// 拉取可用的模型名列表（list-models 端点，与 key 校验同一个请求；
/// 2026-10-27 增补）。**抽屉保存前调用**：表单里的 base_url/key 直接
/// 传参（key 跨 IPC 在本应用是既有事实——输入路径与 reveal_key 同级，
/// 不是新增暴露面）。失败返回空 vec（前端提示手输，不阻塞保存）。
///
/// 不需要会话（不碰 vault）——拉的是用户正在填写的表单值。
pub fn fetch_models_cmd(protocol: &str, base_url: &str, api_key: &str) -> Vec<String> {
    let tmp = Record::new(
        "fetch".into(),
        Protocol::from_label(protocol),
        base_url.trim().to_string(),
        api_key.trim().to_string(),
    );
    myapikey_core::probe::fetch_models(&tmp).unwrap_or_default()
}

/// 读取设置视图。
pub fn settings_view(sh: &Shared) -> Result<SettingsView> {
    let cfg = sh.config();
    Ok(SettingsView {
        remote: cfg.remote_repo.clone(),
        tombstone_days: cfg.tombstone_days,
        auto_push: cfg.auto_push,
        remember_days: cfg.remember_days,
        auto_lock_minutes: cfg.auto_lock_minutes,
    })
}

/// 保存设置（remote 变化时同步更新 git remote）。
///
/// # Errors
/// 配置写盘 / git remote 设置失败。
pub fn set_settings(sh: &Shared, s: SettingsView) -> Result<()> {
    let mut cfg = sh.config();
    cfg.tombstone_days = s.tombstone_days;
    cfg.auto_push = s.auto_push;
    cfg.remember_days = s.remember_days;
    cfg.auto_lock_minutes = s.auto_lock_minutes;
    // 远端统一存 GitHub 形态（owner/name）；清空 = 解绑（2026-10-26 修复：
    // 此前静默保留旧远端，设置页显示为空但同步仍在跑，UI 与实际背离）。
    // 解析成功后才统一落盘——解析失败时其他设置一并回滚，不留半保存态
    // （2026-10-31 审查修复：save 曾在 remote 赋值之前执行，改动完全不落盘）
    cfg.remote_repo = match s.remote.as_deref().map(str::trim) {
        Some(u) if !u.is_empty() => Some(myapikey_core::remote::parse_github_repo(u)?),
        _ => None,
    };
    cfg.save(&sh.dirs)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 初始化（首次使用）
// ---------------------------------------------------------------------------

/// 初始化第一步：设主密码，可选远端/GitHub 一键。
///
/// 返回 [`InitStage::Done`]（无 GitHub，或 token 已存在直接完成）或
/// [`InitStage::DeviceCode`]（前端展示码，用户授权后调
/// [`init_device_authorized`]）。
///
/// # Errors
/// 已初始化 / GitHub API 失败 / 保存失败。
/// 初始化第一步：选择模式并探测远端（**不收主密码**——密码语义由
/// 远端状态决定，探测完成返回 [`InitStage::ProbeDone`] 后前端才显示
/// 对应的密码表单）。
///
/// - GitHub 模式：有效令牌复用（直接探测）；否则走浏览器 Device Flow
///   （返回 [`InitStage::DeviceCode`]，授权完成调
///   [`init_device_authorized`] 继续探测）。
/// - 手动地址模式：要求本机已有授权令牌。
/// - 均无：返回 `ProbeDone { has_data: false }`（纯单机，finish 时
///   只建本地 vault）。
///
/// # Errors
/// 已初始化 / GitHub API 失败 / 远端地址非法。
pub fn init_start(
    sh: &Shared,
    remote: Option<&str>,
    github_repo: Option<&str>,
) -> Result<InitStage> {
    if sh.dirs.is_initialized() {
        return Err(Error::Usage("already initialized on this machine".into()));
    }
    // token 已存在（例如本机装过 CLI）且**确认仍有效**（失效条目会被清除）→
    // 直接探测（无需浏览器）；GitHub 模式无令牌 → 走设备授权
    if github_repo.is_some() && github::valid_keychain_token()?.is_some() {
        return init_probe(sh, remote, github_repo);
    }
    if let Some(_name) = github_repo {
        let client_id = github::effective_client_id()?;
        let dc = github::start_device_flow(&client_id)?;
        let mut g = enter(&sh.inner);
        g.pending_device = Some(PendingDevice {
            client_id,
            dc: dc.clone(),
        });
        let _ = github::open_browser(&dc.verification_uri);
        return Ok(InitStage::DeviceCode {
            user_code: dc.user_code,
            verification_uri: dc.verification_uri,
        });
    }

    init_probe(sh, remote, github_repo)
}

/// 用户完成浏览器授权后调用：轮询 token → 建仓 → 完成初始化。
///
/// # Errors
/// 无进行中的授权 / 授权被拒或超时 / 后续步骤失败。
pub fn init_device_authorized(sh: &Shared) -> Result<InitStage> {
    let pending = {
        let g = enter(&sh.inner);
        g.pending_device
            .clone()
            .ok_or_else(|| Error::Usage("no GitHub authorization in progress".into()))?
    };
    let tokens = github::poll_device_token(&pending.dc, &pending.client_id)?;
    // refresh token 在 poll 内部已入 keychain（有才写）；这里只存 access
    github::keychain_set_token(&tokens.access_token)?;
    {
        let mut g = enter(&sh.inner);
        g.pending_device = None;
    }
    // 已初始化机器走到这里 = 授权刷新（token 失效自愈）：不重建 vault，
    // 只更新认证并把 remote 重新指向同步仓库（与 CLI init --github 的
    // 刷新分支同语义，2026-10-26 补齐 GUI 侧闭环）
    if sh.dirs.is_initialized() {
        let owner = github::api_login(&tokens.access_token)?;
        let url = github::api_ensure_repo(&tokens.access_token, &owner, github::DEFAULT_REPO_NAME)?;
        // remote_repo 存储形态恒为归一化 owner/name（api_ensure_repo 返回
        // 完整 URL，直接存会让同一字段出现两种形态——设置页预填与
        // status 显示随之不一致，2026-10-31 修复）
        let repo = myapikey_core::remote::parse_github_repo(&url)?;
        let mut cfg = sh.config();
        cfg.remote_repo = Some(repo);
        cfg.save(&sh.dirs)?;
        let records = enter(&sh.inner)
            .session
            .as_ref()
            .map(|s| s.data.alive_records().count())
            .unwrap_or(0);
        return Ok(InitStage::Done {
            joined: false,
            records,
            refresh: true,
        });
    }
    // 新初始化路径：令牌已入库 → 探测远端（有数据则前端要求输入主密码
    // 接入；空仓库则前端要求设置新密码），密码表单由前端按 has_data 显示
    init_probe(sh, None, Some(github::DEFAULT_REPO_NAME))
}

/// 忘记 GitHub 授权：数据层一次写清——清除本机令牌（access/refresh）
/// **并解绑远端**（`unbind_remote`：remote_repo 与同步记账三件套一并
/// 归零，与 `mark_synced` 对称）。vault 与 GitHub 上的仓库都不受影响。
/// 与 CLI `mak remote unset` 同语义。
///
/// 返回是否清到了令牌（false = 本来就没有令牌，解绑仍会执行）。
///
/// # Errors
/// 凭据库 / config 写盘失败。
pub fn forget_github(sh: &Shared) -> Result<bool> {
    let had_token = myapikey_core::github::keychain_delete_token()?;
    let mut cfg = sh.config();
    myapikey_core::sync::unbind_remote(&sh.dirs, &mut cfg);
    // 进行中的授权/绑定中间态一并作废（2026-10-31 审查可选采纳）
    {
        let mut g = enter(&sh.inner);
        g.pending_device = None;
        g.pending_init = None;
    }
    Ok(had_token)
}

/// 重新授权 GitHub（设置页入口，2026-10-26）：起设备授权流并返回码页
/// 数据；授权完成由 [`init_device_authorized`] 的已初始化分流收尾。
/// 适用场景：token 失效（远端报 Invalid username or token）后自愈。
///
/// # Errors
/// 无凭据库 / 设备流发起失败。
pub fn github_reauthorize(sh: &Shared) -> Result<InitStage> {
    let client_id = github::effective_client_id()?;
    let dc = github::start_device_flow(&client_id)?;
    let mut g = enter(&sh.inner);
    g.pending_device = Some(PendingDevice {
        client_id,
        dc: dc.clone(),
    });
    drop(g);
    let _ = github::open_browser(&dc.verification_uri);
    Ok(InitStage::DeviceCode {
        user_code: dc.user_code,
        verification_uri: dc.verification_uri,
    })
}

/// 与 CLI init 相同的收尾：远端接入（下载+验证）或全新建仓 + TTL 记忆。
/// 初始化探测（D25 交互对齐）：授权/绑定后建仓/复用并拉取远端状态，
/// 存入 `pending_init`——密码表单由前端按 `has_data` 决定语义后再调
/// [`init_finish`]。
fn init_probe(sh: &Shared, remote: Option<&str>, github_repo: Option<&str>) -> Result<InitStage> {
    // 纯单机（无远端无 GitHub）：跳过授权与探测，直接返回 has_data=false
    if remote.is_none() && github_repo.is_none() {
        let mut g = enter(&sh.inner);
        g.pending_init = Some(crate::PendingInit {
            repo_id: String::new(),
            token: String::new(),
            head: None,
        });
        return Ok(InitStage::ProbeDone {
            has_data: false,
            repo: String::new(),
        });
    }
    let token = github::keychain_token()?
        .ok_or_else(|| Error::Usage("GitHub authorization is missing; redo the setup".into()))?;
    let login = github::api_login(&token)?;

    // owner 恒等于授权账号（归属校验 fail fast）
    let (owner, name): (String, String) = match (remote, github_repo) {
        (Some(url), _) => match myapikey_core::remote::parse_github_repo(url) {
            Ok(id) => {
                let (o, n) = id.split_once('/').expect("guaranteed one slash");
                (o.to_string(), n.to_string())
            }
            _ => (login.clone(), url.to_string()),
        },
        (_, Some(name)) => (login.clone(), name.to_string()),
        _ => (
            login.clone(),
            myapikey_core::github::DEFAULT_REPO_NAME.to_string(),
        ),
    };
    if owner != login {
        return Err(Error::Usage(format!(
            "repo owner `{owner}` does not match the authorized account `{login}`."
        )));
    }
    let repo_id = format!("{owner}/{name}");

    let _url = github::api_ensure_repo(&token, &owner, &name)?;
    println!("Sync repository ready (private): https://github.com/{repo_id}");

    let gh = sync::open_remote(&repo_id, &token)?;
    let gh_dyn: &dyn myapikey_core::remote::SyncStore = &gh;
    let head = gh_dyn.fetch_head()?;
    let has_data = head.is_some();

    {
        let mut g = enter(&sh.inner);
        g.pending_init = Some(crate::PendingInit {
            repo_id: repo_id.clone(),
            token: token.clone(),
            head: head.map(|h| (h.sha, h.bytes)),
        });
    }
    Ok(InitStage::ProbeDone {
        has_data,
        repo: repo_id,
    })
}

/// 初始化收尾（前端密码表单提交）：按探测结果二选一——
/// - 远端有数据 → 主密码解密接入（错误即报错，前端可重试）；
/// - 远端空 → 以该密码加密空 vault 上传基线；
/// - 单机（无探测）→ 只建本地 vault。
///
/// # Errors
/// 无进行中的初始化 / 主密码不匹配远端数据 / 保存失败。
pub fn init_finish(sh: &Shared, passphrase: &str) -> Result<InitStage> {
    let pending = {
        let g = enter(&sh.inner);
        g.pending_init.clone()
    };
    let Some(p) = pending else {
        return Err(Error::Usage("no initialization in progress".into()));
    };

    match &p.head {
        // 远端有数据 → 主密码解密接入（发现数据才要求解锁）
        Some((_sha, bytes)) => {
            let data = vault::decrypt(bytes, passphrase).map_err(|_| {
                Error::Usage(
                    "cannot decrypt the remote vault with the given passphrase. All \
                     devices must use the same master passphrase"
                        .into(),
                )
            })?;
            let records = data.alive_records().count();
            sync::atomic_write_vault(&sh.dirs, bytes)?;
            let mut cfg = sh.config();
            cfg.remote_repo = Some(p.repo_id.clone());
            cfg.save(&sh.dirs)?;
            // 接入 = 本地与远端一致：统一走 mark_synced 记账（含
            // last_sync_at——此前手写漏了这个字段，绑定即同步却显示 never）
            sync::mark_synced(&sh.dirs, &mut cfg, p.head.as_ref().expect("head").0.clone());
            let mut g = enter(&sh.inner);
            g.session = Some(Session {
                passphrase: passphrase.to_string(),
                data,
            });
            println!("Initialized with {records} records.");
            Ok(InitStage::Done {
                joined: true,
                records,
                refresh: false,
            })
        }
        // 远端空仓库 → 以该密码加密空 vault 上传基线
        None if !p.repo_id.is_empty() => {
            sh.dirs.ensure_dirs()?;
            let bytes = vault::encrypt(&VaultData::empty(), passphrase)?;
            sync::atomic_write_vault(&sh.dirs, &bytes)?;
            let gh = sync::open_remote(&p.repo_id, &p.token)?;
            let gh_dyn: &dyn myapikey_core::remote::SyncStore = &gh;
            let message = format!("sync: {}, 0 records", chrono::Utc::now().to_rfc3339());
            let sha = gh_dyn.put_blob(&message, &bytes, None)?;
            let mut cfg = sh.config();
            cfg.remote_repo = Some(p.repo_id.clone());
            cfg.save(&sh.dirs)?;
            // 基线上传 = 本地与远端一致：统一记账
            sync::mark_synced(&sh.dirs, &mut cfg, sha);
            let mut g = enter(&sh.inner);
            g.session = Some(Session {
                passphrase: passphrase.to_string(),
                data: VaultData::empty(),
            });
            println!("Initialized with an empty vault (0 records).");
            Ok(InitStage::Done {
                joined: false,
                records: 0,
                refresh: false,
            })
        }
        // 单机 → 只建本地 vault
        None => {
            sh.dirs.ensure_dirs()?;
            let bytes = vault::encrypt(&VaultData::empty(), passphrase)?;
            sync::atomic_write_vault(&sh.dirs, &bytes)?;
            let mut cfg = sh.config();
            cfg.push_pending = false;
            cfg.save(&sh.dirs)?;
            let mut g = enter(&sh.inner);
            g.session = Some(Session {
                passphrase: passphrase.to_string(),
                data: VaultData::empty(),
            });
            println!("Initialized (standalone mode, no remote).");
            Ok(InitStage::Done {
                joined: false,
                records: 0,
                refresh: false,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    //! ops 层集成测试：MYAPIKEY_HOME 指向临时目录；remember_days=0、
    //! auto_push=false 避免触碰真实凭据库与网络。
    //! （keychain 的 TTL 行为已由 CLI 冒烟覆盖，这里不重复碰系统凭据库。）

    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// 环境变量临界区互斥锁：`MYAPIKEY_HOME` 是进程级全局，并行测试的
    /// set/remove 会互相踩（一个测试刚 set，另一个恰好 remove，导致
    /// AppDirs 解析到真实目录而写入失败）。set → 构造 → remove 必须原子。
    fn home_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn shared(tag: &str) -> Shared {
        let _guard = home_lock().lock().unwrap_or_else(|p| p.into_inner());
        let home =
            std::env::temp_dir().join(format!("mak-gui-{tag}-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&home).expect("创建临时目录失败");
        std::env::set_var("MYAPIKEY_HOME", &home);
        let sh = Shared::new().expect("Shared 构造失败");
        std::env::remove_var("MYAPIKEY_HOME");
        drop(_guard);
        // 禁用记忆与自动推送，测试不碰全局状态（凭据库/网络）
        AppConfig {
            remember_days: 0,
            auto_push: false,
            ..AppConfig::default()
        }
        .save(&sh.dirs)
        .expect("写测试配置失败");
        sh
    }

    const PASS: &str = "gui-test-passphrase";

    #[test]
    fn init_unlock_crud_roundtrip() {
        let sh = shared("crud");

        // 未初始化时解锁报错
        assert!(
            unlock_with(&sh, PASS).is_err(),
            "vault 不存在时解锁必须失败"
        );

        // 初始化（纯单机）
        match init_start(&sh, None, None).expect("init 失败") {
            InitStage::ProbeDone { has_data, .. } => {
                assert!(!has_data, "单机初始化不应有远端数据");
                init_finish(&sh, PASS).expect("init_finish 失败");
            }
            InitStage::Done {
                joined, records, ..
            } => {
                assert!(!joined);
                assert_eq!(records, 0);
            }
            other => panic!("无 GitHub 时不应出现设备码阶段: {other:?}"),
        }
        assert!(sh.dirs.is_initialized(), "初始化后应可检测到");

        // 锁定状态下操作被拒
        lock(&sh);
        assert!(list_view(&sh).is_err(), "锁定时 list 必须报错");

        // 解锁 + 增改删
        unlock_with(&sh, PASS).expect("解锁失败");
        let created = save_record(
            &sh,
            RecordInput {
                name: "Test".into(),
                protocol: "openai".into(),
                base_url: "https://api.example.com/v1".into(),
                api_key: "sk-gui-test-12345678".into(),
                models: None,
                notes: Some("n".into()),
            },
            None,
        )
        .expect("新增失败");
        assert!(created.masked_key.contains("..."), "列表视图必须是掩码 key");
        // 回归（2026-10-25 反馈）：新建记录的备注曾被整个丢弃
        assert_eq!(
            created.notes.as_deref(),
            Some("n"),
            "新建时填写的备注必须保留"
        );

        let updated = save_record(
            &sh,
            RecordInput {
                name: "Test2".into(),
                protocol: "custom:gateway".into(),
                base_url: "https://gw.example.com".into(),
                api_key: "sk-gui-test-87654321".into(),
                models: None,
                notes: None,
            },
            Some(&created.id),
        )
        .expect("编辑失败");
        assert_eq!(updated.name, "Test2");
        assert_eq!(updated.protocol, "custom:gateway");

        // 回归（2026-10-25 优化）：编辑时 key 传空 = 保留原 key 不变，
        // 只改其他字段（改备注不再被"必须 Reveal"拦截）
        let notes_only = save_record(
            &sh,
            RecordInput {
                name: "Test2".into(),
                protocol: "custom:gateway".into(),
                base_url: "https://gw.example.com".into(),
                api_key: String::new(),
                models: None,
                notes: Some("just a note".into()),
            },
            Some(&created.id),
        )
        .expect("仅改备注失败");
        assert_eq!(notes_only.notes.as_deref(), Some("just a note"));
        // key 未变：reveal 出来的仍是上一步写入的 key
        assert_eq!(
            reveal_key(&sh, &created.id).unwrap(),
            "sk-gui-test-87654321",
            "空 key 编辑必须保留原 key"
        );

        // reveal / copy 走会话数据
        assert_eq!(
            reveal_key(&sh, &created.id).unwrap(),
            "sk-gui-test-87654321"
        );

        delete_record(&sh, &created.id).expect("删除失败");
        assert!(list_view(&sh).unwrap().is_empty());
    }

    #[test]
    fn idle_lock_clears_session() {
        let sh = shared("idle");
        init_start(&sh, None, None).expect("init 失败");
        init_finish(&sh, PASS).expect("init_finish 失败");
        unlock_with(&sh, PASS).expect("解锁失败");

        // 刚操作完不锁
        assert!(!lock_if_idle(&sh, Duration::from_secs(60)));
        // 把活动时间拨回过去再测
        {
            let mut g = sh.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.last_activity = Instant::now() - Duration::from_secs(120);
        }
        assert!(
            lock_if_idle(&sh, Duration::from_secs(60)),
            "空闲超时必须锁定"
        );
        assert!(list_view(&sh).is_err(), "锁定后操作被拒");
        // 无会话时再调不报错也不重复发事件
        assert!(!lock_if_idle(&sh, Duration::from_secs(60)));
    }

    #[test]
    fn lock_clears_remembered_passphrase() {
        let sh = shared("lockrm");
        init_start(&sh, None, None).expect("init 失败");
        init_finish(&sh, PASS).expect("init_finish 失败");
        // 手动记忆一次（remember_days=7）；隔离 service 保证只写测试命名空间
        keychain::set_ttl(&sh.dirs, PASS, 7).expect("记忆密码失败");
        assert!(
            keychain::get_with_expiry(&sh.dirs).unwrap().is_some(),
            "前置：记住的密码应可用"
        );

        lock(&sh);
        assert!(
            keychain::get_with_expiry(&sh.dirs).unwrap().is_none(),
            "锁定后记住的密码必须被清除"
        );
        assert!(list_view(&sh).is_err(), "锁定后操作被拒");

        // 重输一次密码成功后按 TTL 重新记住（用户成本 = 每次锁定后输一次）；
        // 这里把 remember_days 调成 7 模拟默认配置
        AppConfig {
            remember_days: 7,
            auto_push: false,
            ..AppConfig::default()
        }
        .save(&sh.dirs)
        .expect("写测试配置失败");
        unlock_with(&sh, PASS).expect("重输解锁失败");
        assert!(
            keychain::get_with_expiry(&sh.dirs).unwrap().is_some(),
            "成功重输后应重新记住"
        );

        // 空闲自动锁同样清除
        {
            let mut g = sh.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.last_activity = Instant::now() - Duration::from_secs(3600);
        }
        assert!(lock_if_idle(&sh, Duration::from_secs(60)));
        assert!(
            keychain::get_with_expiry(&sh.dirs).unwrap().is_none(),
            "自动锁后记住的密码必须被清除"
        );
    }

    #[test]
    fn sync_standalone_rejected() {
        // 单机 sync：没有远端可对账，必须明确报错而非静默 no-op——
        // 静默成功曾被前端兜底渲染成"已是最新"，暗示对过账
        // （2026-10-31 修复）。报错同时不得动本地数据
        let sh = shared("sync");
        init_start(&sh, None, None).expect("init 失败");
        init_finish(&sh, PASS).expect("init_finish 失败");
        unlock_with(&sh, PASS).expect("解锁失败");
        save_record(
            &sh,
            RecordInput {
                name: "S".into(),
                protocol: "anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
                api_key: "sk-ant-t-1".into(),
                models: None,
                notes: None,
            },
            None,
        )
        .expect("新增失败");
        let err = sync_now(&sh).expect_err("单机 sync 必须报错");
        assert!(
            err.to_string().contains("standalone mode"),
            "错误应说明单机模式无远端：{err}"
        );
        assert_eq!(list_view(&sh).unwrap().len(), 1, "本地数据不受影响");
    }
}
