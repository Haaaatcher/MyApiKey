//! # myapikey-gui 核心
//!
//! 会话状态与全部业务操作。`commands.rs` 里的 `#[tauri::command]`
//! 只是薄封装，业务逻辑全部在本模块——因此可以脱离 Tauri 运行时做
//! 集成测试（见文末 tests）。
//!
//! 安全约束（与 CLI 一致，见 AGENTS.md）：
//! - `api_key` 明文只存在于：内存中的会话、`reveal_key`（用户显式点击）
//!   与剪贴板（`copy_key`，30 秒后尽力清除）；发给前端的列表数据一律掩码；
//! - 解锁密码只在内存（会话）与凭据库（TTL 记忆）；
//! - 错误信息一律英文且不包含任何凭据。

use myapikey_core::build_id;
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
    /// 中间态——远端已探明，等前端收解锁密码完成接入/建基线）。
    pub pending_init: Option<PendingInit>,
}

/// 初始化探测中间态：token 已就绪、远端已探明，等解锁密码完成收尾。
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

/// 解锁会话：明文解锁密码与解密后的数据。
pub struct Session {
    /// 解锁密码（保存/同步时重加密用）。
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
    let Some(url) = sh.config().remote_repo else {
        return Ok(None);
    };
    // 绑定失效折叠为未绑定(2026-11-10 用户拍板:用户只感知绑定与未绑定
    // 两态):凭据条目不在即按单机处理,写操作跳过推送(本地照常保存,
    // 重绑后 sync 一次带上去,数据无损)
    let Some(token) = myapikey_core::github::keychain_token(&sh.dirs)? else {
        return Ok(None);
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
/// token（+ 轮换后的刷新令牌）。`Ok(())` = 已续期，调用方重试一次写
/// 操作（重试会现读新凭据）；`Err` = 续期失败，调用方必须把原因并入
/// 错误展示——用户只看到 401 而不知道续期为何没救场，等于诊断线索
/// 被吞（2026-11-03 修复：原布尔封装吞掉了失败原因）。
fn try_refresh_and_retry_setup(sh: &Shared) -> Result<()> {
    // client_id 取环境变量覆盖（2026-10-31 审查采纳：原硬编码会让覆盖
    // 部署下的刷新必败）
    let client_id = github::effective_client_id()?;
    myapikey_core::github::refresh_access_token(&sh.dirs, &client_id).map(|_| ())
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
///
/// 不含「记住密码可用性」（原 `has_remembered`，2026-11-13 拆到
/// [`remembered_status`]）——钥匙串读取可能触发系统授权弹窗，留在
/// 启动状态里会阻塞前端首帧渲染（窗口空白）。
#[derive(Debug, Serialize)]
pub struct GuiState {
    pub initialized: bool,
    pub unlocked: bool,
}

/// `makara sync` 的前端回显。
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
    pub auto_lock_minutes: u32,
    /// 文件日志级别(off/error/info/debug)——设置页可控,即时生效。
    pub log_level: String,
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
    /// —— `has_data = true` 显示"输入解锁密码接入"；false 显示"设置新的
    /// 解锁密码"。用户提交后调 `init_finish` 收尾。
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
    let mut g = inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    // 1.0.0 → 1.1.0 升级迁移（幂等，进程内一次）：repo/.git 里的旧
    // 绑定抢救到应用 config——必须在读取绑定状态之前完成，否则升级后
    // 设置页显示未绑定（2026-10-31 v1.1.0 升级反馈）。gui_state 是
    // GUI 启动的第一个命令，迁移挂这里。
    static LEGACY_MIGRATED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    if !LEGACY_MIGRATED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let mut cfg = sh.config();
        myapikey_core::sync::migrate_legacy_git(&sh.dirs, &mut cfg);
    }
    let unlocked = enter(&sh.inner).session.is_some();
    Ok(GuiState {
        initialized: sh.dirs.is_initialized(),
        unlocked,
    })
}

/// 「一键解锁」按钮的可用性（凭据库是否有未过期的记住副本）。
///
/// 2026-11-13 从 [`state_view`] 拆出：钥匙串读取可能触发系统授权弹窗
/// （新构建身份首次访问条目），阻塞在弹窗上会让 `gui_state` 迟迟不
/// 返回、前端 boot 挂起、**窗口空白**（实测）。拆为独立命令后前端先
/// 渲染锁定页、再异步补齐此状态——弹窗叠加在已渲染的 UI 上，不再
/// 出现空白窗口。
///
/// # Errors
/// 不失败：凭据库不可用/条目缺失一律降级为 `false`。保留 `Result`
/// 与命令层错误通道一致。
pub fn remembered_status(sh: &Shared) -> Result<bool> {
    Ok(keychain::get_with_expiry(&sh.dirs).ok().flatten().is_some())
}

/// 解锁页是否应显示钥匙串授权引导（D57）：仅新构建后的首次调用为
/// true——检测内部会写回构建指纹，确认一次后不再提示。一次性事件
/// 故独立于 [`GuiState`] 状态快照。
pub fn keychain_hint(sh: &Shared) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    build_id::is_new_build(&sh.dirs.config_dir, &exe)
}

/// 手动输入解锁密码解锁；成功后按 TTL 记忆。
///
/// # Errors
/// - [`Error::NotInitialized`] / [`Error::Decrypt`]：与 CLI 相同语义。
pub fn unlock_with(sh: &Shared, passphrase: &str) -> Result<()> {
    if !sh.dirs.vault_path.is_file() {
        return Err(Error::NotInitialized);
    }
    let bytes = std::fs::read(&sh.dirs.vault_path)?;
    let data = vault::decrypt(&bytes, passphrase).map_err(|e| {
        myapikey_core::log::error("gui", &format!("unlock failed: {e}"));
        e
    })?;
    // 记住副本 TTL = 自动锁定时长（D32 统一）：锁定时长内重启免输；
    // 记不住（无头环境等）不阻塞解锁，前端可提示
    let _ = keychain::set_ttl(&sh.dirs, passphrase, sh.config().auto_lock_minutes);
    let mut g = enter(&sh.inner);
    g.session = Some(Session {
        passphrase: passphrase.to_string(),
        data,
    });
    myapikey_core::log::info("gui", "unlocked (manual passphrase)");
    Ok(())
}

/// 一键解锁：尝试记忆中的解锁密码。
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
    if let Ok(data) = vault::decrypt(&bytes, &p) {
        let mut g = enter(&sh.inner);
        g.session = Some(Session {
            passphrase: p,
            data,
        });
        myapikey_core::log::info("gui", "unlocked (remembered passphrase)");
        Ok(true)
    } else {
        // 记忆已失效（他机改密）：清理后要求手动输入
        myapikey_core::log::info("gui", "remembered passphrase stale; cleared");
        let _ = keychain::delete(&sh.dirs);
        Ok(false)
    }
}

/// 手动锁定（Lock 按钮）。
///
/// 2026-10-24 决议（主人）：**锁定 = 必须重新输入解锁密码**。除销毁会话外
/// 还要清除"记住的密码"（等价 `makara forget`）——否则一键解锁会让锁定
/// 形同虚设：任何能碰到键盘的人点一下就能重新进入。
/// 仅当本机确实记过密码（`remember.json` 存在）才动凭据库——既避免无谓
/// 写操作，也保证从不记忆的测试环境不触碰凭据库。
pub fn lock(sh: &Shared) {
    myapikey_core::log::info("gui", "locked manually (session + remembered copy cleared)");
    clear_remembered_if_any(sh);
    let mut g = enter(&sh.inner);
    g.session = None;
    g.pending_device = None;
    // 初始化中间态（含令牌与远端意图）同样随锁定作废（2026-10-31
    // 审查可选采纳：此前驻留内存到进程结束）
    g.pending_init = None;
}

/// 清除记住的解锁密码（remember.json + 凭据库条目）。
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
        let g = sh
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.session.is_some() && g.last_activity.elapsed() >= idle_limit
    };
    if !should_lock {
        return false;
    }
    // 凭据库清理在互斥锁外做（可能涉及系统调用），锁内只动内存
    clear_remembered_if_any(sh);
    let mut g = sh
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
#[expect(
    clippy::too_many_lines,
    reason = "保存+认证重试编排是线性流程,拆子函数反而让凭据传递碎片化"
)]
pub fn save_record(
    sh: &Shared,
    input: RecordInput,
    editing_id: Option<&str>,
) -> Result<RecordView> {
    let name = input.name.trim().to_string();
    let base_url = input.base_url.trim().to_string();
    let api_key = input.api_key.trim().to_string();
    myapikey_core::log::info(
        "gui",
        &format!(
            "save record ({}): \"{}\"",
            if editing_id.is_some() { "edit" } else { "add" },
            name
        ),
    );
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
    let _write_guard = sh
        .write
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // 在会话数据的**克隆**上应用修改（2026-10-26 审查修复：此前直接改
    // session.data，落盘失败后 UI 显示新值而磁盘是旧值，且下次保存会把
    // 未确认的数据静默写出）。落盘成功才回写会话，失败则会话保持旧值
    let (passphrase, data, updated_id) = {
        let mut g = enter(&sh.inner);
        let session = session_of(&mut g)?;
        let mut data = session.data.clone();
        let updated = if let Some(id_str) = editing_id {
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
        } else {
            let mut r = Record::new(name, protocol, base_url, api_key);
            // Record::new 不接收备注——新建时显式补上（曾在此整个丢弃
            // notes，新建记录的备注保存后消失，2026-10-25 反馈）
            r.notes = notes;
            r.models = models.unwrap_or_default();
            let id = r.id;
            data.records.push(r);
            id
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
            Err(e) if myapikey_core::sync::is_auth_error(&e.to_string()) => {
                match try_refresh_and_retry_setup(sh) {
                    Ok(()) => {
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
                    Err(re) => {
                        myapikey_core::log::error("gui", &format!("token refresh failed: {re}"));
                        return Err(Error::Usage(format!(
                            "{e} — re-bind from the banner on the main page"
                        )));
                    }
                }
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
    myapikey_core::log::info("gui", &format!("delete record: {id_str}"));
    // 写闸门 + 克隆改 + 实例锁内落盘（与 save_record 同模式，防并发
    // 覆盖与跨进程写冲突）
    let _write_guard = sh
        .write
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            Err(e) if myapikey_core::sync::is_auth_error(&e.to_string()) => {
                match try_refresh_and_retry_setup(sh) {
                    Ok(()) => {
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
                    Err(re) => {
                        myapikey_core::log::error("gui", &format!("token refresh failed: {re}"));
                        return Err(Error::Usage(format!(
                            "{e} — re-bind from the banner on the main page"
                        )));
                    }
                }
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

// ---------------------------------------------------------------------------
// 备份与迁移：明文导出 / JSON 导入（D47，与 CLI export/import 同语义）
// ---------------------------------------------------------------------------

/// 导出结果：落盘完整路径与记录数（前端 toast 呈现路径；明文泄露警示
/// 由确认流程先行给出）。
#[derive(Serialize)]
pub struct ExportedView {
    pub path: String,
    pub count: usize,
}

/// 导入结果：合并计数 + 导入前快照路径（被覆盖的本地旧版本写
/// Downloads，误导入时数据有保底；均为 0 = 幂等导入，未落盘未推送）。
#[derive(Serialize)]
pub struct ImportedView {
    pub added: usize,
    pub updated: usize,
    pub backup_path: Option<String>,
}

/// 导出文件权限收紧（unix 0600；Windows 无此语义）——GUI 适配 macOS
/// （D26）时导出文件同样不应对其他用户可读。
fn restrict_permissions(path: &std::path::Path) {
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

/// 明文导出：会话存活记录序列化为裸数组（与 CLI `makara export` 同
/// 格式，可互相导入），写到用户经另存为对话框选定的路径（D49），
/// 权限收紧后返回完整路径。需解锁会话（导出的是解密后的数据）。
///
/// # Errors
/// 会话未解锁 / 序列化或写盘失败。
pub fn export_records(sh: &Shared, path: &str) -> Result<ExportedView> {
    let data = {
        let mut g = enter(&sh.inner);
        session_of(&mut g)?.data.clone()
    };
    let alive: Vec<&Record> = data.alive_records().collect();
    let json = serde_json::to_string_pretty(&alive)
        .map_err(|e| Error::Usage(format!("cannot serialize records: {e}")))?;
    let path = std::path::PathBuf::from(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, json)?;
    restrict_permissions(&path);
    let path = path.display().to_string();
    myapikey_core::log::info(
        "gui",
        &format!("exported {} records -> {path}", alive.len()),
    );
    Ok(ExportedView {
        path,
        count: alive.len(),
    })
}

/// JSON 导入：按 id LWW 合并（core `import_records`，与 CLI `makara
/// import` 同语义）。零变化（幂等导入）不落盘不推送；有变化走与
/// delete_record 同一编排（实例锁 + `sync::save` + 认证失败静默续期
/// 重试）并回写会话。
///
/// # Errors
/// 会话未解锁 / JSON 解析失败 / 落盘或推送失败。
pub fn import_records_cmd(sh: &Shared, json: &str) -> Result<ImportedView> {
    // 两种形状：[{...}] 裸数组（CLI export 格式）或 {"records":[...]}
    let imported: Vec<Record> = if json.trim_start().starts_with('[') {
        serde_json::from_str(json)
            .map_err(|e| Error::Usage(format!("import file is not a valid record array: {e}")))?
    } else {
        serde_json::from_str::<VaultData>(json)
            .map_err(|e| Error::Usage(format!("unrecognized import file format: {e}")))?
            .records
    };

    let _write_guard = sh
        .write
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (passphrase, mut data) = {
        let mut g = enter(&sh.inner);
        let session = session_of(&mut g)?;
        (session.passphrase.clone(), session.data.clone())
    };
    let report = myapikey_core::merge::import_records(&mut data, imported, chrono::Utc::now());
    if report.added + report.updated == 0 {
        // 幂等导入（D46 语义同 CLI）：零变化不落盘不推送
        return Ok(ImportedView {
            added: 0,
            updated: 0,
            backup_path: None,
        });
    }
    // 导入前快照（D52）：被覆盖的本地旧版本写 Downloads——误导入时
    // 数据有保底（快照的 updated_at 早于现数据，直接再导入会被 LWW
    // 跳过——它是数据保底而非一键回滚）
    let backup_path = if report.overwritten.is_empty() {
        None
    } else {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        let mut dir = std::path::PathBuf::from(home);
        dir.push("Downloads");
        std::fs::create_dir_all(&dir)?;
        let snap = dir.join(format!("makara-import-backup-{ts}.json"));
        let json = serde_json::to_string_pretty(&report.overwritten)
            .map_err(|e| Error::Usage(format!("cannot serialize the backup: {e}")))?;
        std::fs::write(&snap, json)?;
        restrict_permissions(&snap);
        let shown = snap.display().to_string();
        myapikey_core::log::info(
            "gui",
            &format!(
                "import backup: {} overwritten record(s) -> {shown}",
                report.overwritten.len()
            ),
        );
        Some(shown)
    };
    let _instance = myapikey_core::lock::acquire(&sh.dirs)?;
    let mut cfg = sh.config();
    let remote = remote_of(sh)?;
    let remote_dyn: Option<&dyn myapikey_core::remote::SyncStore> = remote
        .as_ref()
        .map(|r| r as &dyn myapikey_core::remote::SyncStore);
    match sync::save(
        &sh.dirs,
        &passphrase,
        &data,
        remote_dyn,
        cfg.auto_push,
        &mut cfg,
    ) {
        Err(e) if myapikey_core::sync::is_auth_error(&e.to_string()) => {
            match try_refresh_and_retry_setup(sh) {
                Ok(()) => {
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
                Err(re) => {
                    myapikey_core::log::error("gui", &format!("token refresh failed: {re}"));
                    return Err(Error::Usage(format!(
                        "{e} — re-bind from the banner on the main page"
                    )));
                }
            }
        }
        other => {
            other?;
        }
    }
    let mut g = enter(&sh.inner);
    session_of(&mut g)?.data = data;
    Ok(ImportedView {
        added: report.added,
        updated: report.updated,
        backup_path,
    })
}
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
    let _write_guard = sh
        .write
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (passphrase, ()) = {
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
        // 认证失败 → 刷新令牌静默续期 → 重建远端重试一次（与写路径
        // save、CLI sync 同策略）。重建不可省：GitHubStore 构造时把
        // token 缓存进结构体，续期后旧对象里还是过期凭据。缺这套包装
        // 时 access token 过期（约 8 小时）后 GUI 手动同步必然 401，
        // 而 CLI 同操作却能静默恢复（2026-11-03 用户实测反馈）
        match sync::sync(&sh.dirs, &passphrase, &remote, cfg.tombstone_days, &mut cfg) {
            Err(e) if myapikey_core::sync::is_auth_error(&e.to_string()) => {
                match try_refresh_and_retry_setup(sh) {
                    Ok(()) => {
                        let mut cfg = sh.config();
                        let remote = remote_of(sh)?;
                        let Some(remote) = remote else {
                            return Err(Error::Usage(
                                "standalone mode: no remote is bound to sync with. \
                                 Bind one on the settings page"
                                    .into(),
                            ));
                        };
                        sync::sync(&sh.dirs, &passphrase, &remote, cfg.tombstone_days, &mut cfg)?
                    }
                    Err(re) => {
                        myapikey_core::log::error("gui", &format!("token refresh failed: {re}"));
                        return Err(Error::Usage(format!(
                            "{e} — re-bind from the banner on the main page"
                        )));
                    }
                }
            }
            other => other?,
        }
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

/// 修改解锁密码：验旧密码 → 新密码重加密落盘 → 绑定远端时以新密文覆盖
/// 远端 → 更新会话与记住的密码副本。
///
/// 语义与 CLI `makara passwd set` 一致（2026-11-03 GUI 补齐入口）：远端
/// vault 会被新密码密文覆盖，其他设备下次同步必须输入新密码（旧密码
/// 解远端会失败，core 给出"所有设备必须同密码"的指引）。强度提示在
/// 前端展示，后端不做硬性强度要求（2026-11-03 决议），只兜底非空。
///
/// # Errors
/// 未解锁 / 旧密码不正确（本地 vault 解密失败）/ 新密码长度越界 /
/// 落盘或推送失败（推送 401 时静默续期重试一次，与写路径同策略）。
pub fn change_passphrase(sh: &Shared, old_passphrase: &str, new_passphrase: &str) -> Result<()> {
    // 长度硬性限制(8–128 字符;2026-11-04 用户要求)
    vault::validate_passphrase(new_passphrase)?;
    // 写闸门：改密码 = 重写 vault + 回写会话，与写命令互斥
    let _write_guard = sh
        .write
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // 旧密码验证 = 用它解密本地 vault，成功顺带拿到待重加密的数据；
    // 先校验会话已解锁（未解锁先报未解锁，而非暴露"密码错误"）
    let data = {
        let mut g = enter(&sh.inner);
        session_of(&mut g)?;
        let bytes = std::fs::read(&sh.dirs.vault_path)?;
        // 加密学上无法区分"密码错"与"vault 损坏"，密码错占绝对多数
        // （vault 落盘走原子写，损坏概率极低），文案按密码错误给出
        vault::decrypt(&bytes, old_passphrase)
            .map_err(|_| Error::Usage("the current passphrase is incorrect".into()))?
    };
    // 记住副本随新密码按统一锁定时长重写（0 = 永不锁定 = 副本不过期）：
    // 改密码后"锁定时长内重启免输"的体验得以延续
    let remember_minutes = sh.config().auto_lock_minutes;

    // 实例锁（与 CLI 互斥）内落盘 + 推送；认证失败 → 刷新令牌静默
    // 续期 → 重建远端（旧对象缓存着过期 token）重试一次——与
    // sync_now / save 同策略
    {
        let _instance = myapikey_core::lock::acquire(&sh.dirs)?;
        let mut cfg = sh.config();
        let remote = remote_of(sh)?;
        let remote_dyn: Option<&dyn myapikey_core::remote::SyncStore> = remote
            .as_ref()
            .map(|r| r as &dyn myapikey_core::remote::SyncStore);
        match sync::save(
            &sh.dirs,
            new_passphrase,
            &data,
            remote_dyn,
            cfg.auto_push,
            &mut cfg,
        ) {
            Err(e) if myapikey_core::sync::is_auth_error(&e.to_string()) => {
                match try_refresh_and_retry_setup(sh) {
                    Ok(()) => {
                        let mut cfg = sh.config();
                        let remote = remote_of(sh)?;
                        let remote_dyn: Option<&dyn myapikey_core::remote::SyncStore> = remote
                            .as_ref()
                            .map(|r| r as &dyn myapikey_core::remote::SyncStore);
                        sync::save(
                            &sh.dirs,
                            new_passphrase,
                            &data,
                            remote_dyn,
                            cfg.auto_push,
                            &mut cfg,
                        )?;
                    }
                    Err(re) => {
                        myapikey_core::log::error("gui", &format!("token refresh failed: {re}"));
                        return Err(Error::Usage(format!(
                            "{e} — re-bind from the banner on the main page"
                        )));
                    }
                }
            }
            other => {
                other?;
            }
        }
    }

    // 会话密码替换：后续写命令直接以新密码工作，无需重新解锁
    {
        let mut g = enter(&sh.inner);
        if let Some(s) = &mut g.session {
            s.passphrase = new_passphrase.to_string();
        }
    }
    // 记住的密码副本同步：按统一锁定时长重写新密码副本（0 = 永不过期），
    // 保持改密码后「锁定时长内重启免输」的行为不变
    let _ = keychain::set_ttl(&sh.dirs, new_passphrase, remember_minutes);
    myapikey_core::log::info(
        "gui",
        "passphrase changed (vault re-encrypted, remote updated)",
    );
    Ok(())
}

/// GitHub 授权/安装诊断视图（2026-11-04 分步引导）。
#[derive(Debug, Clone, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "诊断视图天然是逐项布尔状态集合,前端按布尔渲染引导分支"
)]
pub struct GhDiagView {
    /// 凭据库有 GitHub 令牌（device flow 授权完成过）。
    pub authorized: bool,
    /// 令牌对 GET /user 有效（未过期/未被撤销）。
    pub token_valid: bool,
    /// GitHub App 已安装到用户账号（未安装 = 令牌对仓库无权）。
    pub installed: bool,
    /// 未安装时的安装页 URL（引导直达）。
    pub install_url: Option<String>,
    /// 已绑定远端仓库（单机模式无同步，不引导）。
    pub remote_bound: bool,
    /// 绑定仓库可访问（安装范围含它且 Contents 可写）。
    /// `None` = 未绑定或未安装，无从判断。
    pub repo_accessible: Option<bool>,
}

/// GitHub 授权/安装诊断（网络探测 GET /user、/user/installations、
/// /repos/{owner}/{name}，放线程池；**无会话要求**——凭据库与配置
/// 独立于解锁状态，锁屏时引导条也能给出正确指引）。
///
/// # Errors
/// 网络请求失败（诊断失败不等于授权失效，前端保留现状引导）。
pub fn github_diagnose(sh: &Shared) -> Result<GhDiagView> {
    let Some(token) = github::keychain_token(&sh.dirs)? else {
        // 绑定失效折叠为未绑定(2026-11-10 用户拍板:用户只感知绑定与
        // 未绑定两态):凭据条目不在即 remote_bound=false,前端按未绑定
        // 渲染——不出现"绑定已失效"字样与失效引导条
        return Ok(GhDiagView {
            authorized: false,
            token_valid: false,
            installed: false,
            install_url: None,
            remote_bound: false,
            repo_accessible: None,
        });
    };
    let cfg = sh.config();
    let repo: Option<(String, String)> = cfg
        .remote_repo
        .as_deref()
        .and_then(|r| myapikey_core::remote::parse_github_repo(r).ok())
        .map(|id| {
            id.split_once('/').map_or_else(
                || (id.clone(), String::new()),
                |(o, n)| (o.to_string(), n.to_string()),
            )
        });
    let repo_ref = repo.as_ref().map(|(o, n)| (o.as_str(), n.as_str()));
    let diag = github::diagnose_github_auth(&token, repo_ref)?;
    Ok(GhDiagView {
        authorized: true,
        token_valid: diag.token_valid,
        installed: diag.installed,
        install_url: diag.install_url,
        remote_bound: cfg.remote_repo.is_some(),
        repo_accessible: diag.repo_accessible,
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
        .map_or(0, |s| s.data.alive_records().count());
    // 绑定失效折叠为未绑定:凭据条目读不到令牌,remote 显示 none——主界面
    // chip 显示「单机模式」,设置页按未绑定渲染(2026-11-10 用户拍板)。
    // 判断必须与 remote_of 同语义(令牌**读得到**才算绑定):曾写成
    // `is_ok()`(只表示后端没坏,条目不存在也 ok),导致 chip 显示已绑定
    // 而 sync 按 standalone 拒绝——两处矛盾(2026-11-13 Windows 实测)。
    let remote = info.remote.filter(|_| {
        myapikey_core::github::keychain_token(&sh.dirs)
            .ok()
            .flatten()
            .is_some()
    });
    Ok(StatusView {
        remote,
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
        auto_lock_minutes: cfg.auto_lock_minutes,
        log_level: cfg.log_level,
    })
}

/// 保存设置。
///
/// # Errors
/// 配置写盘失败。
pub fn set_settings(sh: &Shared, s: &SettingsView) -> Result<()> {
    let mut cfg = sh.config();
    cfg.tombstone_days = s.tombstone_days;
    cfg.auto_push = s.auto_push;
    cfg.auto_lock_minutes = s.auto_lock_minutes;
    // 日志级别即时生效(文件日志器是全局原子量,2026-11-05 用户需求)
    cfg.log_level.clone_from(&s.log_level);
    myapikey_core::log::set_level(&cfg.log_level);
    // 远端统一存 GitHub 形态（owner/name）；清空 = 解绑（2026-10-26 修复：
    // 此前静默保留旧远端，设置页显示为空但同步仍在跑，UI 与实际背离）。
    // 解析成功后才统一落盘——解析失败时其他设置一并回滚，不留半保存态
    // （2026-10-31 审查修复：save 曾在 remote 赋值之前执行，改动完全不落盘）
    cfg.remote_repo = match s.remote.as_deref().map(str::trim) {
        Some(u) if !u.is_empty() => Some(myapikey_core::remote::parse_github_repo(u)?),
        _ => None,
    };
    cfg.save(&sh.dirs)?;
    // 解锁会话中按新时长立即重写记住副本（2026-11-04 用户真机命中）：
    // 只改配置不动副本的话，改「永不」后旧副本（如默认 10 分钟 TTL）仍
    // 在倒计时，重启即撞密码要求——设置形同虚设。未解锁时无副本可写，
    // 下次解锁自然按新时长写入
    {
        let g = sh
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sess) = &g.session {
            let _ = keychain::set_ttl(&sh.dirs, &sess.passphrase, s.auto_lock_minutes);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 初始化（首次使用）
// ---------------------------------------------------------------------------

/// 初始化第一步：设解锁密码，可选远端/GitHub 一键。
///
/// 返回 [`InitStage::Done`]（无 GitHub，或 token 已存在直接完成）或
/// [`InitStage::DeviceCode`]（前端展示码，用户授权后调
/// [`init_device_authorized`]）。
///
/// # Errors
/// 已初始化 / GitHub API 失败 / 保存失败。
/// 初始化第一步：选择模式并探测远端（**不收解锁密码**——密码语义由
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
    if github_repo.is_some() && github::valid_keychain_token(&sh.dirs)?.is_some() {
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

/// 绑定后的立即对账（D55）：以会话主密码跑完整 sync 状态机（拉取/
/// 合并/推送/记账），并把合并结果重载进会话，返回同步后的活跃记录数。
///
/// 仅在绑定已成立后调用；对账失败不回滚绑定，错误原样上抛，由调用
/// 方落日志、由下一次手动同步以同样语义再次暴露（如远端主密码不一致）。
///
/// # Errors
/// - 无会话（锁定态）：正常入口到不了这里，防御性拒绝；
/// - sync 状态机错误（网络 / 远端主密码不一致等）；
/// - 同步后 vault 重读/解密失败（理论上不可能：文件由本函数刚写）。
fn reconcile_after_bind(
    sh: &Shared,
    remote: &dyn myapikey_core::remote::SyncStore,
) -> Result<usize> {
    let passphrase = {
        let g = enter(&sh.inner);
        g.session
            .as_ref()
            .map(|s| s.passphrase.clone())
            .ok_or_else(|| Error::Usage("vault is locked".into()))?
    };
    let mut cfg = sh.config();
    let out =
        myapikey_core::sync::sync(&sh.dirs, &passphrase, remote, cfg.tombstone_days, &mut cfg)?;
    if let Some(pe) = out.push_error {
        return Err(Error::Usage(pe));
    }
    // 与 sync_now 同法：vault 文件已是合并后的最新，重载进会话
    let bytes = std::fs::read(&sh.dirs.vault_path)?;
    let data = vault::decrypt(&bytes, &passphrase)?;
    let alive = data.alive_records().count();
    {
        let mut g = enter(&sh.inner);
        if let Some(s) = &mut g.session {
            s.data = data;
        }
    }
    Ok(alive)
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
            .ok_or_else(|| Error::Usage("no binding in progress".into()))?
    };
    let tokens = github::poll_device_token(&sh.dirs, &pending.dc, &pending.client_id)?;
    // refresh token 在 poll 内部已入存储（有才写）；这里只存 access
    github::keychain_set_token(&sh.dirs, &tokens.access_token)?;
    {
        let mut g = enter(&sh.inner);
        g.pending_device = None;
    }
    // 已初始化机器走到这里 = 授权刷新（token 失效自愈）：不重建 vault，
    // 只更新认证并把 remote 重新指向同步仓库（与 CLI init --github 的
    // 刷新分支同语义，2026-10-26 补齐 GUI 侧闭环）
    if sh.dirs.is_initialized() {
        let owner = github::api_login(&tokens.access_token)?;
        let repo_name = sh.config().default_repo_name;
        let url = github::api_ensure_repo(&tokens.access_token, &owner, &repo_name)?;
        // remote_repo 存储形态恒为归一化 owner/name（api_ensure_repo 返回
        // 完整 URL，直接存会让同一字段出现两种形态——设置页预填与
        // status 显示随之不一致，2026-10-31 修复）
        let repo = myapikey_core::remote::parse_github_repo(&url)?;
        let mut cfg = sh.config();
        cfg.remote_repo = Some(repo);
        cfg.save(&sh.dirs)?;
        // 绑定即对账（D55）：接管分支不再裸绑定——历史上绑定后本地与
        // 远端可能各自漂移，残留快照会在下次同步时以基线形态推上远端
        // （2026-11-12 真实踩中：双端默认仓名分叉后，Mac 把 10-23 的
        // 冒烟残留推进了新建仓库，用户误读为远端被覆盖）。无会话
        // （锁定态，正常入口到不了设置页，防御分支）或对账失败都不
        // 回滚绑定：前者留待解锁后同步，后者下一次同步如实暴露
        let mut records = enter(&sh.inner)
            .session
            .as_ref()
            .map_or(0, |s| s.data.alive_records().count());
        if let Ok(Some(remote)) = remote_of(sh) {
            match reconcile_after_bind(sh, remote.as_ref()) {
                Ok(alive) => records = alive,
                Err(e) => myapikey_core::log::error("bind", &format!("post-bind sync failed: {e}")),
            }
        }
        return Ok(InitStage::Done {
            joined: false,
            records,
            refresh: true,
        });
    }
    // 新初始化路径：令牌已入库 → 探测远端（有数据则前端要求输入解锁密码
    // 接入；空仓库则前端要求设置新密码），密码表单由前端按 has_data 显示
    let repo_name = sh.config().default_repo_name;
    init_probe(sh, None, Some(&repo_name))
}

/// 忘记 GitHub 授权：数据层一次写清——清除本机令牌（access/refresh）
/// **并解绑远端**（`unbind_remote`：remote_repo 与同步记账三件套一并
/// 归零，与 `mark_synced` 对称）。vault 与 GitHub 上的仓库都不受影响。
/// 与 CLI `makara remote unset` 同语义。
///
/// 返回是否清到了令牌（false = 本来就没有令牌，解绑仍会执行）。
///
/// # Errors
/// 凭据库 / config 写盘失败。
pub fn forget_github(sh: &Shared) -> Result<bool> {
    let had_token = myapikey_core::github::keychain_delete_token(&sh.dirs)?;
    myapikey_core::log::info(
        "gui",
        &format!("github authorization forgotten (had token = {had_token})"),
    );
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
    myapikey_core::log::info("gui", "re-authorize requested (device flow starting)");
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
    let token = github::keychain_token(&sh.dirs)?
        .ok_or_else(|| Error::Usage("Binding is missing; redo the setup".into()))?;
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
        _ => (login.clone(), sh.config().default_repo_name),
    };
    if owner != login {
        return Err(Error::Usage(format!(
            "repo owner `{owner}` does not match your GitHub account (`{login}`)."
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
/// - 远端有数据 → 解锁密码解密接入（错误即报错，前端可重试）；
/// - 远端空 → 以该密码加密空 vault 上传基线；
/// - 单机（无探测）→ 只建本地 vault。
///
/// # Errors
/// 无进行中的初始化 / 解锁密码不匹配远端数据 / 保存失败。
pub fn init_finish(sh: &Shared, passphrase: &str) -> Result<InitStage> {
    let pending = {
        let g = enter(&sh.inner);
        g.pending_init.clone()
    };
    let Some(p) = pending else {
        return Err(Error::Usage("no initialization in progress".into()));
    };
    // 新设密码(远端空仓库/单机)硬性校验长度;接入分支的密码是既有
    // 事实(旧密码解密远端),不做此校验
    if p.head.is_none() {
        vault::validate_passphrase(passphrase)?;
    }

    match &p.head {
        // 远端有数据 → 解锁密码解密接入（发现数据才要求解锁）
        Some((_sha, bytes)) => {
            let data = vault::decrypt(bytes, passphrase).map_err(|_| {
                Error::Usage(
                    "cannot decrypt the remote vault with the given passphrase. All \
                     devices must use the same passphrase"
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
    //! ops 层集成测试：MYAPIKEY_HOME 指向临时目录；auto_lock_minutes=0、
    //! auto_push=false 避免触碰真实凭据库与网络。
    //! （keychain 的 TTL 行为已由 CLI 冒烟覆盖，这里不重复碰系统凭据库。）

    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// 内存远端（D55 对账测试专用）：单文件 CAS 的最小 SyncStore 实现。
    /// sha 按 push 次数确定性递增（初始 sha-1，首次推送产出 sha-2），
    /// 断言不依赖随机值。
    struct MemRemote {
        head: Mutex<Option<myapikey_core::remote::RemoteHead>>,
        pushes: Mutex<usize>,
    }

    impl MemRemote {
        fn with_head(sha: &str, bytes: Vec<u8>) -> Self {
            Self {
                head: Mutex::new(Some(myapikey_core::remote::RemoteHead {
                    sha: sha.into(),
                    bytes,
                })),
                pushes: Mutex::new(1),
            }
        }
    }

    impl myapikey_core::remote::SyncStore for MemRemote {
        fn fetch_head(&self) -> Result<Option<myapikey_core::remote::RemoteHead>> {
            Ok(self
                .head
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone())
        }

        fn put_blob(
            &self,
            _message: &str,
            bytes: &[u8],
            expect_sha: Option<&str>,
        ) -> Result<String> {
            let mut head = self
                .head
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cas_ok = match (&*head, expect_sha) {
                (Some(cur), Some(expect)) => cur.sha == expect,
                (None, None) => true,
                _ => false,
            };
            assert!(
                cas_ok,
                "CAS 前置不符: 远端 {:?} vs 期望 {expect_sha:?}",
                head.as_ref().map(|h| &h.sha)
            );
            let n = {
                let mut p = self
                    .pushes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *p += 1;
                *p
            };
            let sha = format!("sha-{n}");
            *head = Some(myapikey_core::remote::RemoteHead {
                sha: sha.clone(),
                bytes: bytes.to_vec(),
            });
            Ok(sha)
        }

        fn download_ref(&self, ref_sha: &str) -> Result<Vec<u8>> {
            let head = self
                .head
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            head.as_ref()
                .filter(|h| h.sha == ref_sha)
                .map(|h| h.bytes.clone())
                .ok_or_else(|| Error::Usage("ref not found".into()))
        }
    }

    /// D55：绑定后的立即对账——拉取远端、LWW 合并、推送并把合并结果
    /// 重载进会话（远端记录对界面立即可见），记账指向本次推送的 sha。
    #[test]
    fn reconcile_after_bind_pulls_merges_and_reloads_session() {
        let sh = shared("bindrec");
        let _guard = home_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // 单机初始化（空 vault + 会话解锁），本地放一条记录
        init_start(&sh, None, None).expect("init 失败");
        init_finish(&sh, PASS).expect("init_finish 失败");
        save_record(
            &sh,
            RecordInput {
                name: "Local".into(),
                protocol: "openai".into(),
                base_url: "https://local.example.com/v1".into(),
                api_key: "sk-local-12345678".into(),
                models: None,
                notes: None,
            },
            None,
        )
        .expect("本地记录失败");

        // 远端已有另一条记录（不同 id，合并应取并集）
        let remote_rec = Record::new(
            "Remote".into(),
            Protocol::OpenAi,
            "https://remote.example.com/v1".into(),
            "sk-remote-12345678".into(),
        );
        let remote_bytes = vault::encrypt(
            &VaultData {
                records: vec![remote_rec],
            },
            PASS,
        )
        .expect("远端密文加密失败");
        let mem = MemRemote::with_head("sha-1", remote_bytes);

        let alive = reconcile_after_bind(&sh, &mem).expect("对账失败");
        assert_eq!(alive, 2, "合并后 = 本地 1 条 + 远端 1 条");

        // 会话已重载：远端记录对界面可见
        let listed = list_view(&sh).expect("列表失败");
        assert!(
            listed.iter().any(|v| v.name == "Remote"),
            "远端记录必须进入会话"
        );

        // vault 文件已重写为合并结果
        let bytes = std::fs::read(&sh.dirs.vault_path).expect("读 vault 失败");
        let merged = vault::decrypt(&bytes, PASS).expect("解密失败");
        assert_eq!(
            merged.alive_records().count(),
            2,
            "落盘 vault 应含合并后的两条记录"
        );

        // 记账指向本次推送产出的 sha（初始 sha-1 被消费）
        assert_eq!(
            sh.config().last_sync_sha.as_deref(),
            Some("sha-2"),
            "last_sync_sha 应指向本次推送产出的版本"
        );
        assert_eq!(
            *mem.pushes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            2,
            "对账应恰好推送一次（计数含初始 head 的 1）"
        );
    }

    /// 环境变量临界区互斥锁：`MYAPIKEY_HOME` 是进程级全局，并行测试的
    /// set/remove 会互相踩（一个测试刚 set，另一个恰好 remove，导致
    /// AppDirs 解析到真实目录而写入失败）。set → 构造 → remove 必须原子。
    fn home_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn shared(tag: &str) -> Shared {
        let guard = home_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let home =
            std::env::temp_dir().join(format!("mak-gui-{tag}-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&home).expect("创建临时目录失败");
        std::env::set_var("MYAPIKEY_HOME", &home);
        let sh = Shared::new().expect("Shared 构造失败");
        std::env::remove_var("MYAPIKEY_HOME");
        drop(guard);
        // 禁用记忆与自动推送，测试不碰全局状态（凭据库/网络）
        AppConfig {
            auto_lock_minutes: 0,
            auto_push: false,
            ..AppConfig::default()
        }
        .save(&sh.dirs)
        .expect("写测试配置失败");
        sh
    }

    const PASS: &str = "gui-test-passphrase";

    #[test]
    fn set_settings_rewrites_remember_ttl_immediately() {
        // D32 修复回归(2026-11-04 用户真机命中):解锁中把锁定时长改为
        // 「永不」,记住副本必须立即按新时长重写——否则旧 TTL(如 10 分钟)
        // 仍在倒计时,重启撞密码要求,设置形同虚设
        let sh = shared("settings-ttl");
        // isolated keyring 只有一个条目名,并行测试互踩——全程持锁互斥
        // (guard 有意持有到测试结束:_ 前缀表达"不显式 drop")
        let _guard = home_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match init_start(&sh, None, None).expect("init 失败") {
            InitStage::ProbeDone { .. } => {
                init_finish(&sh, PASS).expect("init_finish 失败");
            }
            InitStage::DeviceCode { .. } => {
                panic!("无 GitHub 时不应出现设备码阶段")
            }
            InitStage::Done { .. } => {
                panic!("无既有令牌时不应直接完成初始化")
            }
        }
        // 先解锁(按旧时长 10 分钟写入副本),再改设置为「永不」
        unlock_with(&sh, PASS).expect("解锁失败");
        let mut view = settings_view(&sh).expect("读设置失败");
        view.auto_lock_minutes = 0;
        set_settings(&sh, &view).expect("保存设置失败");

        // 会话解锁中:副本必须立即变为不过期
        let remembered = keychain::get_with_expiry(&sh.dirs).expect("读副本失败");
        assert!(
            remembered.as_deref() == Some(PASS),
            "改「永不」后副本应立即不过期"
        );

        // 模拟长时间闲置后重启:副本仍未过期 → 免输恢复解锁
        lock_if_idle(&sh, Duration::from_hours(24));
        assert!(
            unlock_remembered(&sh).expect("unlock_remembered 失败"),
            "「永不」下重启应免输恢复解锁"
        );

        // 反向:改回 1 分钟后,模拟副本到期(remember.json 时刻改为过去)
        // → get_with_expiry 应清理并返回 None(TTL 自然到期的路径;测试
        // 无法真等 1 分钟,时钟不可注入)
        let mut view = settings_view(&sh).expect("读设置失败");
        view.auto_lock_minutes = 1;
        set_settings(&sh, &view).expect("保存设置失败");
        unlock_with(&sh, PASS).expect("解锁失败");
        let expiry_path = sh.dirs.config_dir.join("remember.json");
        let expired = r#"{"expires_at":"2000-01-01T00:00:00Z"}"#;
        std::fs::write(&expiry_path, expired).expect("改写过期时刻失败");
        assert!(
            keychain::get_with_expiry(&sh.dirs)
                .expect("读副本失败")
                .is_none(),
            "TTL 到期后副本应被清理"
        );
    }

    #[test]
    fn init_unlock_crud_roundtrip() {
        let sh = shared("crud");
        let _guard = home_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

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
            InitStage::DeviceCode { .. } => {
                panic!("无 GitHub 时不应出现设备码阶段")
            }
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
            let mut g = sh
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.last_activity = Instant::now()
                .checked_sub(Duration::from_secs(120))
                .unwrap();
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
        let _guard = home_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        init_start(&sh, None, None).expect("init 失败");
        init_finish(&sh, PASS).expect("init_finish 失败");
        // 手动记忆一次（auto_lock_minutes=7 分钟）；隔离 service 保证只写测试命名空间
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
        // 这里把 auto_lock_minutes 调成 7 分钟模拟默认配置
        AppConfig {
            auto_lock_minutes: 7,
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
            let mut g = sh
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.last_activity = Instant::now()
                .checked_sub(Duration::from_secs(3600))
                .unwrap();
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
