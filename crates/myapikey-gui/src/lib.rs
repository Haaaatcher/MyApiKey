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
use myapikey_core::github::{self, GitAuth};
use myapikey_core::keychain;
use myapikey_core::model::{Protocol, Record, VaultData};
use myapikey_core::paths::AppDirs;
use myapikey_core::store::Store;
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
    /// 本地 git 仓库句柄（repo 路径 + 可选认证）。git 的真实状态全在磁盘
    /// 仓库里，这里只是一层轻量包装，克隆无副作用。
    ///
    /// 为什么放互斥锁里可变：应用是长驻进程，Store **不能只在启动时构造一次**——
    /// GitHub 设备授权完成后会拿到新 token，必须立即回写（[`Shared::set_auth`]），
    /// 否则同一进程内后续的保存/同步仍然没有认证，私有仓库 fetch/push 全部失败，
    /// 表现为"浏览器授权成功了但应用里同步无效"，直到重启应用才恢复。
    /// （CLI 无此问题：每条命令都重新读凭据库构造 Store。）
    pub store: Mutex<Store>,
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
    /// 待完成的初始化主密码。
    pub passphrase: String,
}

impl Shared {
    /// 组装共享状态：路径 + 配置 + 已存 GitHub token 的 git 认证。
    ///
    /// # Errors
    /// 路径解析失败（如 MYAPIKEY_HOME 非法）。
    pub fn new() -> Result<Self> {
        let dirs = AppDirs::load()?;
        let mut store = Store::new(&dirs.repo_dir);
        if let Ok(Some(token)) = github::keychain_token() {
            store = store.with_auth(GitAuth::github_token(&token));
        }
        Ok(Self {
            store: Mutex::new(store),
            write: Mutex::new(()),
            inner: Mutex::new(Inner {
                session: None,
                last_activity: Instant::now(),
                pending_device: None,
            }),
            dirs,
        })
    }

    /// 取当前 Store 的快照（含最新认证）。互斥锁只持有克隆的一瞬，
    /// 后续 git/网络操作在锁外进行，不与自动锁线程互卡。
    pub fn store(&self) -> Store {
        self.store.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// 用新拿到的 GitHub token 更新共享 Store 的认证。
    /// 设备授权/初始化完成时必须调用；见字段 [`Shared::store`] 的文档。
    fn set_auth(&self, token: &str) {
        let mut g = self.store.lock().unwrap_or_else(|p| p.into_inner());
        *g = g.clone().with_auth(GitAuth::github_token(token));
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

/// git 认证保鲜（2026-10-26 用户反馈修复）：凭据库里的 GitHub token 可能
/// 被另一端刷新（CLI 跑 `mak init --github` 重新授权、或 token 失效自愈），
/// 而本进程内存里的 Store 仍持旧 token，之后所有 git 操作都会被远端以
/// "Invalid username or token" 拒绝。写路径入口一律重读凭据库覆写内存
/// 认证——本地凭据库调用，代价可忽略，换来"刷了就能用"；
/// 无 token（None）时不动现状。
fn refresh_github_auth(sh: &Shared) {
    if let Ok(Some(token)) = myapikey_core::github::keychain_token() {
        sh.set_auth(&token);
    }
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
    /// git 是否可用（启动预检：缺失时前端进安装引导页而非让首次
    /// 保存/同步报英文错误）
    pub git_ok: bool,
}

/// `mak sync` 的前端回显。
#[derive(Debug, Serialize)]
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
    pub ahead: usize,
    pub behind: usize,
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
}

/// 记录编辑表单（前端提交）。
#[derive(Debug, Deserialize)]
pub struct RecordInput {
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub api_key: String,
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
        git_ok: myapikey_core::subprocess::git_available(),
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

    // 写命令串行闸门：全函数持有，防并发保存互相覆盖会话
    let _write_guard = sh.write.lock().unwrap_or_else(|p| p.into_inner());
    refresh_github_auth(sh);

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
                // 改备注/名称等不再强制 Reveal）——保留原 key，只更新其他字段
                if !api_key.is_empty() {
                    r.api_key = api_key;
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
                let id = r.id;
                data.records.push(r);
                id
            }
        };
        (session.passphrase.clone(), data, updated)
    };

    // 跨进程实例锁（与 CLI 互斥）内完成落盘
    let cfg = sh.config();
    {
        let _instance = myapikey_core::lock::acquire(&sh.dirs)?;
        sync::save(&sh.store(), &sh.dirs, &passphrase, &data, cfg.auto_push)?;
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
    refresh_github_auth(sh);
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
    let cfg = sh.config();
    {
        let _instance = myapikey_core::lock::acquire(&sh.dirs)?;
        sync::save(&sh.store(), &sh.dirs, &passphrase, &data, cfg.auto_push)?;
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

/// 同步（pull + 合并 + push），成功后更新 last_sync_at 与会话数据。
///
/// # Errors
/// 未解锁 / git 失败 / 远端密码不一致。
pub fn sync_now(sh: &Shared) -> Result<SyncView> {
    // 写闸门：sync 会重写 vault 并回写会话，与其他写命令互斥
    let _write_guard = sh.write.lock().unwrap_or_else(|p| p.into_inner());
    refresh_github_auth(sh);
    let (passphrase, _) = {
        let mut g = enter(&sh.inner);
        let session = session_of(&mut g)?;
        (session.passphrase.clone(), ())
    };
    let cfg = sh.config();
    // 跨进程实例锁内完成整个 fetch/merge/push（网络耗时段也持锁——
    // CLI 侧同一时段的写会等待而非交错）
    let out: SyncOutcome = {
        let _instance = myapikey_core::lock::acquire(&sh.dirs)?;
        sync::sync(&sh.store(), &sh.dirs, &passphrase, cfg.tombstone_days)?
    };

    let mut new_cfg = cfg.clone();
    new_cfg.last_sync_at = Some(chrono::Utc::now().to_rfc3339());
    let _ = new_cfg.save(&sh.dirs);

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
    let info = sync::status_summary(&sh.store())?;
    let alive = enter(&sh.inner)
        .session
        .as_ref()
        .map(|s| s.data.alive_records().count())
        .unwrap_or(0);
    Ok(StatusView {
        remote: info.remote,
        ahead: info.ahead,
        behind: info.behind,
        last_sync: sh.config().last_sync_at,
        alive,
    })
}

/// 读取设置视图。
pub fn settings_view(sh: &Shared) -> Result<SettingsView> {
    let cfg = sh.config();
    Ok(SettingsView {
        remote: sh.store().remote_url()?,
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
    cfg.save(&sh.dirs)?;
    match &s.remote {
        Some(url) if !url.trim().is_empty() => {
            sh.store().set_remote(url.trim())?;
        }
        // 清空 = 解绑（2026-10-26 修复：此前静默保留旧远端，设置页显示
        // 为空但同步仍在跑，UI 与实际状态背离）
        _ => {
            sh.store().remove_remote()?;
        }
    }
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
pub fn init_start(
    sh: &Shared,
    passphrase: &str,
    remote: Option<&str>,
    github_repo: Option<&str>,
) -> Result<InitStage> {
    if sh.dirs.is_initialized() {
        return Err(Error::Usage("already initialized on this machine".into()));
    }
    // token 已存在（例如本机装过 CLI）且**确认仍有效**（失效条目会被清除）→
    // 无需浏览器，直接走完成路径；失效则继续走设备授权重新拿 token
    if github_repo.is_some() && github::valid_keychain_token()?.is_some() {
        let done = github_finish(sh, passphrase)?;
        return Ok(InitStage::Done {
            joined: done.0,
            records: done.1,
            refresh: false,
        });
    }

    if let Some(_repo) = github_repo {
        // 发起设备授权，把机密留在 Rust 侧
        let client_id = github::effective_client_id()?;
        let dc = github::start_device_flow(&client_id)?;
        let mut g = enter(&sh.inner);
        g.pending_device = Some(PendingDevice {
            client_id,
            dc: dc.clone(),
            passphrase: passphrase.to_string(),
        });
        let _ = github::open_browser(&dc.verification_uri);
        return Ok(InitStage::DeviceCode {
            user_code: dc.user_code,
            verification_uri: dc.verification_uri,
        });
    }

    let (joined, records) = plain_init(sh, passphrase, remote)?;
    Ok(InitStage::Done {
        joined,
        records,
        refresh: false,
    })
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
    let token = github::poll_device_token(&pending.dc, &pending.client_id)?;
    github::keychain_set_token(&token)?;
    {
        let mut g = enter(&sh.inner);
        g.pending_device = None;
    }
    // 已初始化机器走到这里 = 授权刷新（token 失效自愈）：不重建 vault，
    // 只更新认证并把 remote 重新指向同步仓库（与 CLI init --github 的
    // 刷新分支同语义，2026-10-26 补齐 GUI 侧闭环）
    if sh.dirs.is_initialized() {
        sh.set_auth(&token);
        let owner = github::api_login(&token)?;
        let url = github::api_ensure_repo(&token, &owner, github::DEFAULT_REPO_NAME)?;
        sh.store().set_remote(&url)?;
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
    let (joined, records) = github_finish_with_token(sh, &pending.passphrase, &token)?;
    Ok(InitStage::Done {
        joined,
        records,
        refresh: false,
    })
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
    // 刷新场景不重建 vault，passphrase 留空（authorized 分支按已初始化分流，不消费它）
    g.pending_device = Some(PendingDevice {
        client_id,
        dc: dc.clone(),
        passphrase: String::new(),
    });
    drop(g);
    let _ = github::open_browser(&dc.verification_uri);
    Ok(InitStage::DeviceCode {
        user_code: dc.user_code,
        verification_uri: dc.verification_uri,
    })
}

/// token 已就绪时：查身份 → 建/识别仓库 → 复用已有 token 的 Store 完成初始化。
fn github_finish(sh: &Shared, passphrase: &str) -> Result<(bool, usize)> {
    let token = github::keychain_token()?.ok_or_else(|| Error::Usage("no GitHub token".into()))?;
    github_finish_with_token(sh, passphrase, &token)
}

fn github_finish_with_token(sh: &Shared, passphrase: &str, token: &str) -> Result<(bool, usize)> {
    let owner = github::api_login(token)?;
    let name = myapikey_core::github::DEFAULT_REPO_NAME;
    let url = github::api_ensure_repo(token, &owner, name)?;
    // 关键：token 回写共享 Store——否则本次会话里后续保存/同步都没有认证，
    // 私有仓库操作全部失败（CLI 无此问题，它每条命令都重新读凭据库）
    sh.set_auth(token);
    let store = sh.store();
    finish_init(sh, &store, passphrase, Some(url.as_str()))
}

/// 无 GitHub 的初始化（可选手动 remote）。
fn plain_init(sh: &Shared, passphrase: &str, remote: Option<&str>) -> Result<(bool, usize)> {
    let store = sh.store();
    finish_init(sh, &store, passphrase, remote)
}

/// 与 CLI init 相同的收尾：克隆接入或全新建仓 + 首次提交 + TTL 记忆。
fn finish_init(
    sh: &Shared,
    store: &Store,
    passphrase: &str,
    remote: Option<&str>,
) -> Result<(bool, usize)> {
    let mut joined = false;
    let mut records = 0usize;

    if let Some(url) = remote {
        if myapikey_core::store::remote_has_main(url, store.auth())? {
            std::fs::create_dir_all(&sh.dirs.config_dir)?;
            myapikey_core::store::clone_into(url, &sh.dirs.repo_dir, store.auth())?;
            let bytes = std::fs::read(&sh.dirs.vault_path)?;
            let data = vault::decrypt(&bytes, passphrase).map_err(|_| {
                Error::Usage(
                    "cannot decrypt the remote vault, all devices must share one master \
                     passphrase"
                        .into(),
                )
            })?;
            joined = true;
            records = data.alive_records().count();
            let mut g = enter(&sh.inner);
            g.session = Some(Session {
                passphrase: passphrase.to_string(),
                data,
            });
        }
    }
    if !joined {
        sh.dirs.ensure_dirs()?;
        store.ensure_init()?;
        sync::save(store, &sh.dirs, passphrase, &VaultData::empty(), false)?;
        if let Some(url) = remote {
            store.set_remote(url)?;
            let _ = store.push();
        }
        let mut g = enter(&sh.inner);
        g.session = Some(Session {
            passphrase: passphrase.to_string(),
            data: VaultData::empty(),
        });
    }

    let cfg = sh.config();
    if cfg.remember_days > 0 {
        let _ = keychain::set_ttl(&sh.dirs, passphrase, cfg.remember_days);
    }
    Ok((joined, records))
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
        match init_start(&sh, PASS, None, None).expect("init 失败") {
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
        init_start(&sh, PASS, None, None).expect("init 失败");
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

    /// 回归（2026-10-24 决议）：锁定 = 必须重输密码——手动 Lock 与空闲自动锁
    /// 都要清掉"记住的密码"（= mak forget 语义），一键解锁不允许存在。
    /// 依赖 keychain 的 MYAPIKEY_HOME 隔离命名空间，不会触碰真实凭据库。
    /// 回归（2026-10-26 审查修复）：落盘失败时会话必须保持旧值——
    /// 否则 UI 显示新记录而磁盘没有，下次保存还会把未确认数据写出。
    #[test]
    fn save_failure_keeps_session_unchanged() {
        let sh = shared("savefail");
        init_start(&sh, PASS, None, None).expect("init 失败");
        unlock_with(&sh, PASS).expect("解锁失败");
        // 拆掉 .git 使落盘（commit）必然失败
        std::fs::remove_dir_all(sh.dirs.repo_dir.join(".git")).expect("拆 .git 失败");
        let err = save_record(
            &sh,
            RecordInput {
                name: "Ghost".into(),
                protocol: "openai".into(),
                base_url: "https://ghost.example.com".into(),
                api_key: "sk-ghost-000000000000".into(),
                notes: None,
            },
            None,
        );
        assert!(err.is_err(), "破坏 git 后保存必须失败");
        // 会话回放：list 读取的是会话数据，不能出现幽灵记录
        let views = list_view(&sh).expect("list 失败");
        assert!(
            views.is_empty(),
            "落盘失败的记录不得进入会话，实际: {views:?}"
        );
    }

    #[test]
    fn lock_clears_remembered_passphrase() {
        let sh = shared("lockrm");
        init_start(&sh, PASS, None, None).expect("init 失败");
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
    fn sync_updates_session_data() {
        // 单机 sync：remote 为空应成功且不改数据
        let sh = shared("sync");
        init_start(&sh, PASS, None, None).expect("init 失败");
        unlock_with(&sh, PASS).expect("解锁失败");
        save_record(
            &sh,
            RecordInput {
                name: "S".into(),
                protocol: "anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
                api_key: "sk-ant-t-1".into(),
                notes: None,
            },
            None,
        )
        .expect("新增失败");
        let out = sync_now(&sh).expect("单机 sync 失败");
        assert_eq!(out.alive, 1);
        assert_eq!(list_view(&sh).unwrap().len(), 1);
    }

    /// 回归：GUI 内同步必须真正拉到其他设备的新数据并反映到列表。
    /// 场景（用户报告的"应用里同步无效"）：设备 A 推送后，设备 B 在
    /// 回归（2026-10-26 用户反馈"GUI 同步有问题"）：本地提交后 sync_now
    /// 必须完成 push 并清零未推送计数（本地裸仓库作远端，覆盖全链路）。
    #[test]
    fn sync_now_pushes_and_clears_ahead() {
        use std::process::Command;

        let sh = shared("sync-push");
        init_start(&sh, PASS, None, None).expect("init 失败");
        unlock_with(&sh, PASS).expect("解锁失败");
        save_record(
            &sh,
            RecordInput {
                name: "P1".into(),
                protocol: "openai".into(),
                base_url: "https://p.example.com".into(),
                api_key: "sk-p-1111".into(),
                notes: None,
            },
            None,
        )
        .expect("保存失败");

        let bare =
            std::env::temp_dir().join(format!("mak-gui-bare-{}", uuid::Uuid::now_v7().simple()));
        let st = Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(&bare)
            .status()
            .expect("git 失败");
        assert!(st.success());
        set_settings(
            &sh,
            SettingsView {
                remote: Some(bare.to_str().expect("路径非法").to_string()),
                tombstone_days: 30,
                auto_push: false,
                remember_days: 0,
                auto_lock_minutes: 10,
            },
        )
        .expect("绑定远端失败");

        // auto_push=false：保存只 commit 不推；sync_now 应推上去
        let view = sync_now(&sh).expect("sync_now 失败");
        assert!(view.pushed, "sync_now 必须完成推送，结果: {view:?}");
        assert!(
            view.push_error.is_none(),
            "推送不得报错: {:?}",
            view.push_error
        );
        // 推送后未推送计数归零（chips 的 ahead 来源）
        let status = status_view(&sh).expect("status 失败");
        assert_eq!(status.ahead, 0, "推送后 ahead 必须归零");

        let _ = std::fs::remove_dir_all(&bare);
    }

    /// **已初始化的运行中进程**里执行 sync_now，应看到 A 的新记录——
    /// 数据流经过 fetch → 合并 → 会话重载 → list_view 全链路。
    #[test]
    fn sync_now_pulls_other_device_changes() {
        use std::process::Command;

        let a = shared("sync-a");
        init_start(&a, PASS, None, None).expect("A init 失败");
        unlock_with(&a, PASS).expect("A 解锁失败");
        save_record(
            &a,
            RecordInput {
                name: "A1".into(),
                protocol: "openai".into(),
                base_url: "https://a.example.com".into(),
                api_key: "sk-a-1111".into(),
                notes: None,
            },
            None,
        )
        .expect("A 第一条保存失败");

        // bare 仓库作为远端（本地路径，无需网络/凭据）
        let bare =
            std::env::temp_dir().join(format!("mak-gui-bare-{}", uuid::Uuid::now_v7().simple()));
        let st = Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(&bare)
            .status()
            .expect("git init --bare 执行失败");
        assert!(st.success(), "git init --bare 必须成功");

        set_settings(
            &a,
            SettingsView {
                remote: Some(bare.to_str().expect("路径非法").to_string()),
                tombstone_days: 30,
                auto_push: false,
                remember_days: 0,
                auto_lock_minutes: 10,
            },
        )
        .expect("A 绑定远端失败");
        let out = sync_now(&a).expect("A 首次 sync 失败");
        assert!(out.pushed, "A 首次 sync 应完成推送");

        // 设备 B：运行中进程，以 remote 方式接入（克隆），然后 A 再推一条
        let b = shared("sync-b");
        match init_start(&b, PASS, Some(bare.to_str().expect("路径非法")), None)
            .expect("B init 失败")
        {
            InitStage::Done {
                joined, records, ..
            } => {
                assert!(joined, "B 应走克隆接入路径");
                assert_eq!(records, 1);
            }
            other => panic!("B 不应出现设备码阶段: {other:?}"),
        }
        save_record(
            &a,
            RecordInput {
                name: "A2".into(),
                protocol: "openai".into(),
                base_url: "https://a2.example.com".into(),
                api_key: "sk-a-2222".into(),
                notes: None,
            },
            None,
        )
        .expect("A 第二条保存失败");
        sync_now(&a).expect("A 二次 sync 失败");

        // B 在运行中同步：必须拉到 A2 并反映在列表里
        let out_b = sync_now(&b).expect("B sync 失败");
        assert!(out_b.pulled, "B 应拉到远端新数据");
        let names: Vec<String> = list_view(&b).unwrap().into_iter().map(|r| r.name).collect();
        assert!(
            names.contains(&"A1".to_string()) && names.contains(&"A2".to_string()),
            "B 应看到 A 的全部记录: {names:?}"
        );
    }
}
