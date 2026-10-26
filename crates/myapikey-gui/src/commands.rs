//! Tauri command 薄封装：每个命令只做参数传递与错误转字符串，
//! 业务全部在 lib.rs 顶层函数中（显式 `crate::` 路径调用，
//! 避免与同名 command 产生递归歧义）。
//!
//! 约定：所有错误转成英文短句（`Error` 的 Display 已是英文），
//! 前端原样展示。
//!
//! 线程模型（重要）：
//! - Tauri 2 的**同步** command 在主线程（UI 线程）执行——任何耗时的
//!   git 子进程、Argon2 派生、网络请求都会让窗口冻结、点不动；
//! - 因此凡是涉及 git / vault 加解密 / 凭据库 / 网络的命令一律
//!   `async fn` + [`tauri::async_runtime::spawn_blocking`]，在线程池里跑，
//!   UI 保持响应（按钮的 disabled 状态由前端在 await 期间维持）；
//! - 纯内存命令（lock / list / reveal / copy）保持同步，零开销。
//!
//! `State<'_>` 的借用不能逃出 async 函数体（Tauri 的生命周期约束），
//! 统一先 `state.inner().clone()` 克隆 `Arc` 再移动进闭包。

use crate::SharedState;
use tauri::State;

/// ops 错误 → 前端可展示字符串。
fn err_str(e: myapikey_core::Error) -> String {
    e.to_string()
}

/// `spawn_blocking` 的 join 错误（线程池故障等，几乎不可能发生）→ 字符串。
/// 用泛型而非具体类型：JoinError 由运行时提供，Tauri 未稳定导出其路径。
fn join_err(e: impl std::fmt::Display) -> String {
    format!("internal error: {e}")
}

/// 启动状态（读凭据库判断“一键解锁”是否可用，放线程池）。
#[tauri::command]
pub async fn gui_state(state: State<'_, SharedState>) -> Result<crate::GuiState, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::state_view(&shared))
        .await
        .map_err(join_err)?
        .map_err(err_str)
}

/// 手动输入解锁（Argon2 派生约 1 秒，放线程池）。
#[tauri::command]
pub async fn unlock(passphrase: String, state: State<'_, SharedState>) -> Result<(), String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::unlock_with(&shared, &passphrase))
        .await
        .map_err(join_err)?
        .map_err(err_str)
}

/// 一键解锁：用记忆中的密码（读凭据库 + Argon2，放线程池）。
/// false = 无可用记忆，前端回退手动输入。
#[tauri::command]
pub async fn unlock_remembered(state: State<'_, SharedState>) -> Result<bool, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::unlock_remembered(&shared))
        .await
        .map_err(join_err)?
        .map_err(err_str)
}

/// 手动锁定（纯内存，同步即可）。
#[tauri::command]
pub fn lock(state: State<'_, SharedState>) {
    crate::lock(state.inner());
}

/// 记录列表（掩码；会话内存 + 本机缓存文件，同步即可）。
#[tauri::command]
pub fn list_records(state: State<'_, SharedState>) -> Result<Vec<crate::RecordView>, String> {
    crate::list_view(state.inner()).map_err(err_str)
}

/// 新增/编辑记录（重加密 + git commit/push，放线程池）。
#[tauri::command]
pub async fn save_record(
    input: crate::RecordInput,
    editing_id: Option<String>,
    state: State<'_, SharedState>,
) -> Result<crate::RecordView, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::save_record(&shared, input, editing_id.as_deref())
    })
    .await
    .map_err(join_err)?
    .map_err(err_str)
}

/// 删除记录（重加密 + git commit/push，放线程池）。
#[tauri::command]
pub async fn delete_record(id: String, state: State<'_, SharedState>) -> Result<(), String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::delete_record(&shared, &id))
        .await
        .map_err(join_err)?
        .map_err(err_str)
}

/// 显式查看完整 key（reveal 按钮；纯内存，同步即可）。
#[tauri::command]
pub fn reveal_key(id: String, state: State<'_, SharedState>) -> Result<String, String> {
    crate::reveal_key(state.inner(), &id).map_err(err_str)
}

/// 复制 key（30 秒后自动清除；剪贴板写入是毫秒级，同步即可）。
#[tauri::command]
pub fn copy_key(id: String, state: State<'_, SharedState>) -> Result<(), String> {
    crate::copy_key(state.inner(), &id).map_err(err_str)
}

/// 复制 Base URL（非敏感，不自动清除；字段级复制）。
#[tauri::command]
pub fn copy_base_url(id: String, state: State<'_, SharedState>) -> Result<(), String> {
    crate::copy_base_url(state.inner(), &id)
        .map(|_| ())
        .map_err(err_str)
}

/// 复制任意文本（授权码等短期非敏感值，不自动清除）。
#[tauri::command]
pub fn copy_text(text: String) -> Result<(), String> {
    crate::copy_plain(&text).map_err(err_str)
}

/// 校验单条记录（网络请求最长 10 秒，必须放线程池）。
#[tauri::command]
pub async fn check_key(
    id: String,
    state: State<'_, SharedState>,
) -> Result<crate::CheckView, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::check_key(&shared, &id))
        .await
        .map_err(join_err)?
        .map_err(err_str)
}

/// 同步（git fetch/merge/push 网络操作，放线程池）。
#[tauri::command]
pub async fn sync_now(state: State<'_, SharedState>) -> Result<crate::SyncView, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::sync_now(&shared))
        .await
        .map_err(join_err)?
        .map_err(err_str)
}

/// 状态页数据（git 子进程查询，放线程池）。
#[tauri::command]
pub async fn status(state: State<'_, SharedState>) -> Result<crate::StatusView, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::status_view(&shared))
        .await
        .map_err(join_err)?
        .map_err(err_str)
}

/// 读取设置（含一次 git remote 读取，放线程池）。
#[tauri::command]
pub async fn get_settings(state: State<'_, SharedState>) -> Result<crate::SettingsView, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::settings_view(&shared))
        .await
        .map_err(join_err)?
        .map_err(err_str)
}

/// 保存设置（写配置 + 更新 git remote，放线程池）。
#[tauri::command]
pub async fn set_settings(
    settings: crate::SettingsView,
    state: State<'_, SharedState>,
) -> Result<(), String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::set_settings(&shared, settings))
        .await
        .map_err(join_err)?
        .map_err(err_str)
}

/// 初始化第一步（可能触发设备授权网络请求 / git ls-remote / 全套
/// init 的加密与提交，放线程池）。
#[tauri::command]
pub async fn init_start(
    passphrase: String,
    remote: Option<String>,
    github_repo: Option<String>,
    state: State<'_, SharedState>,
) -> Result<crate::InitStage, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::init_start(
            &shared,
            &passphrase,
            remote.as_deref(),
            github_repo.as_deref(),
        )
    })
    .await
    .map_err(join_err)?
    .map_err(err_str)
}

/// 设备授权完成后收尾（阻塞轮询 + 网络 + git，放线程池）。
#[tauri::command]
pub async fn init_device_authorized(
    state: State<'_, SharedState>,
) -> Result<crate::InitStage, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::init_device_authorized(&shared))
        .await
        .map_err(join_err)?
        .map_err(err_str)
}

// ---------------------------------------------------------------------------
// 应用内更新（D16）
// ---------------------------------------------------------------------------

/// 按查询词搜索记录（评分排序；空查询 = 全量按更新时间）。
#[tauri::command]
pub async fn search_records(
    state: tauri::State<'_, crate::SharedState>,
    query: String,
) -> Result<Vec<crate::RecordView>, String> {
    let sh = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::search_view(&sh, &query))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// 重新授权 GitHub（token 失效自愈入口，设置页）。
#[tauri::command]
pub async fn github_reauthorize(
    state: tauri::State<'_, crate::SharedState>,
) -> Result<crate::InitStage, String> {
    let sh = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::github_reauthorize(&sh))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// 当前应用版本号（设置页常态显示，不联网）。
#[tauri::command]
pub fn app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

/// 检查更新：读 GitHub Releases 的 latest.json，返回版本与破坏性标记。
#[tauri::command]
pub async fn check_update(app: tauri::AppHandle) -> Result<crate::update::UpdateView, String> {
    crate::update::check(&app).await
}

/// 下载并安装更新（下载进度回调暂略，NSIS 静默安装）后重启。
#[tauri::command]
pub async fn apply_update(app: tauri::AppHandle) -> Result<(), String> {
    crate::update::apply(&app).await
}
