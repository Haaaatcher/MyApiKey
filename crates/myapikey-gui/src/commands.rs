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
//! 统一先 `Arc::clone(state.inner())` 克隆 `Arc` 再移动进闭包。

use crate::SharedState;
use std::sync::Arc;

use tauri::State;

/// ops 错误 → 前端可展示字符串。**统一日志出口**：完整错误链在此落
/// 文件日志(error 级)——界面只展示简短现象，排查依据在日志里
/// (2026-11-05 用户需求：错误详情不进悬停/弹窗)。
fn err_str(e: &myapikey_core::Error) -> String {
    myapikey_core::log::error("commands", &e.to_string());
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
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::state_view(&shared))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 解锁页钥匙串授权引导（D57）：新构建首次为 true，检测即确认（写回
/// 构建指纹）。文件 IO 放线程池。
#[tauri::command]
pub async fn keychain_hint(state: State<'_, SharedState>) -> Result<bool, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::keychain_hint(&shared))
        .await
        .map_err(|e| join_err(&e))
}

/// 「一键解锁」可用性（读凭据库，可能触发系统授权弹窗——独立于
/// `gui_state`，保证首帧先渲染，2026-11-13 拆分，放线程池）。
#[tauri::command]
pub async fn remembered_status(state: State<'_, SharedState>) -> Result<bool, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::remembered_status(&shared))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 手动输入解锁（Argon2 派生约 1 秒，放线程池）。
#[tauri::command]
pub async fn unlock(passphrase: String, state: State<'_, SharedState>) -> Result<(), String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::unlock_with(&shared, &passphrase))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 一键解锁：用记忆中的密码（读凭据库 + Argon2，放线程池）。
/// false = 无可用记忆，前端回退手动输入。
#[tauri::command]
pub async fn unlock_remembered(state: State<'_, SharedState>) -> Result<bool, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::unlock_remembered(&shared))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 手动锁定（纯内存，同步即可）。
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri 命令签名由 IPC 框架反序列化决定"
)]
pub fn lock(state: State<'_, SharedState>) {
    crate::lock(state.inner());
}

/// 记录列表（掩码；会话内存 + 本机缓存文件，同步即可）。
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri 命令签名由 IPC 框架反序列化决定"
)]
pub fn list_records(state: State<'_, SharedState>) -> Result<Vec<crate::RecordView>, String> {
    crate::list_view(state.inner()).map_err(|e| err_str(&e))
}

/// 新增/编辑记录（重加密 + git commit/push，放线程池）。
#[tauri::command]
pub async fn save_record(
    input: crate::RecordInput,
    editing_id: Option<String>,
    state: State<'_, SharedState>,
) -> Result<crate::RecordView, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || {
        crate::save_record(&shared, input, editing_id.as_deref())
    })
    .await
    .map_err(|e| join_err(&e))?
    .map_err(|e| err_str(&e))
}

/// 删除记录（重加密 + git commit/push，放线程池）。
#[tauri::command]
pub async fn delete_record(id: String, state: State<'_, SharedState>) -> Result<(), String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::delete_record(&shared, &id))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 显式查看完整 key（reveal 按钮；纯内存，同步即可）。
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri 命令签名由 IPC 框架反序列化决定"
)]
pub fn reveal_key(id: String, state: State<'_, SharedState>) -> Result<String, String> {
    crate::reveal_key(state.inner(), &id).map_err(|e| err_str(&e))
}

/// 复制 key（30 秒后自动清除；剪贴板写入是毫秒级，同步即可）。
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri 命令签名由 IPC 框架反序列化决定"
)]
pub fn copy_key(id: String, state: State<'_, SharedState>) -> Result<(), String> {
    crate::copy_key(state.inner(), &id).map_err(|e| err_str(&e))
}

/// 复制 Base URL（非敏感，不自动清除；字段级复制）。
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri 命令签名由 IPC 框架反序列化决定"
)]
pub fn copy_base_url(id: String, state: State<'_, SharedState>) -> Result<(), String> {
    crate::copy_base_url(state.inner(), &id)
        .map(|_| ())
        .map_err(|e| err_str(&e))
}

/// 复制任意文本（授权码等短期非敏感值，不自动清除）。
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri 命令签名由 IPC 框架反序列化决定"
)]
pub fn copy_text(text: String) -> Result<(), String> {
    crate::copy_plain(&text).map_err(|e| err_str(&e))
}

/// 校验单条记录（网络请求最长 10 秒，必须放线程池）。
#[tauri::command]
pub async fn check_key(
    id: String,
    state: State<'_, SharedState>,
) -> Result<crate::CheckView, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::check_key(&shared, &id))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 一键校验全部记录（核心层并发探测，总耗时约最慢一条；放线程池）。
/// `on_progress`：逐条完成的进度通道，前端据此逐条点亮检测结果。
#[tauri::command]
pub async fn check_all_keys(
    on_progress: tauri::ipc::Channel<crate::CheckResultView>,
    state: State<'_, SharedState>,
) -> Result<Vec<crate::CheckResultView>, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::check_all_keys(&shared, &on_progress))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 同步（git fetch/merge/push 网络操作，放线程池）。
#[tauri::command]
pub async fn sync_now(state: State<'_, SharedState>) -> Result<crate::SyncView, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::sync_now(&shared))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 修改主密码（旧密码 Argon2 校验 + 新密码重加密 + 可推送，放线程池）。
#[tauri::command]
pub async fn change_passphrase(
    old_passphrase: String,
    new_passphrase: String,
    state: State<'_, SharedState>,
) -> Result<(), String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || {
        crate::change_passphrase(&shared, &old_passphrase, &new_passphrase)
    })
    .await
    .map_err(|e| join_err(&e))?
    .map_err(|e| err_str(&e))
}

/// GitHub 授权/安装诊断（网络探测，放线程池；无会话要求）。
#[tauri::command]
pub async fn diagnose_github(state: State<'_, SharedState>) -> Result<crate::GhDiagView, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::github_diagnose(&shared))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 安装页 URL（纯内存，同步即可）。前端合成诊断的占位不带 URL 时，
/// 点击安装按钮经此懒取——slug 单一来源在 core（2026-11-05 审查可选 C）。
#[tauri::command]
pub fn github_install_url() -> String {
    myapikey_core::github::app_install_url()
}

/// 状态页数据（git 子进程查询，放线程池）。
#[tauri::command]
pub async fn status(state: State<'_, SharedState>) -> Result<crate::StatusView, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::status_view(&shared))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 明文导出（D49）：写到用户经另存为对话框选定的路径，返回完整路径。
#[tauri::command]
pub async fn export_records(
    state: State<'_, SharedState>,
    path: String,
) -> Result<crate::ExportedView, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::export_records(&shared, &path))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// JSON 导入（D47）：按 id LWW 合并（与 CLI import 同语义），返回
/// 合并计数。
#[tauri::command]
pub async fn import_records(
    state: State<'_, SharedState>,
    json: String,
) -> Result<crate::ImportedView, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::import_records_cmd(&shared, &json))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 读取设置（含一次 git remote 读取，放线程池）。
#[tauri::command]
pub async fn get_settings(state: State<'_, SharedState>) -> Result<crate::SettingsView, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::settings_view(&shared))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 保存设置（写配置 + 更新 git remote，放线程池）。
#[tauri::command]
pub async fn set_settings(
    settings: crate::SettingsView,
    state: State<'_, SharedState>,
) -> Result<(), String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::set_settings(&shared, &settings))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 初始化第一步（可能触发设备授权网络请求 / 远端探测，放线程池）。
/// D25 交互对齐：不再收主密码——密码表单在探测完成后按远端状态显示。
#[tauri::command]
pub async fn init_start(
    remote: Option<String>,
    github_repo: Option<String>,
    state: State<'_, SharedState>,
) -> Result<crate::InitStage, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || {
        crate::init_start(&shared, remote.as_deref(), github_repo.as_deref())
    })
    .await
    .map_err(|e| join_err(&e))?
    .map_err(|e| err_str(&e))
}

/// 忘记 GitHub 授权：清除本机凭据库里的 access/refresh 令牌。
/// 远端仓库与 vault 不受影响；重新授权走 init 或 remote set 流程。
///
/// # Errors
/// 凭据库操作失败。
#[tauri::command]
pub async fn forget_github_token(state: State<'_, SharedState>) -> Result<bool, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::forget_github(&shared))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
}

/// 设备授权完成后收尾（阻塞轮询 + 网络 + git，放线程池）。
#[tauri::command]
pub async fn init_device_authorized(
    state: tauri::State<'_, crate::SharedState>,
) -> Result<crate::InitStage, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::init_device_authorized(&shared))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// 用系统默认浏览器打开链接（WebView 内导航会离开应用，外链一律
/// 交系统浏览器）。
///
/// # Errors
/// 系统打开浏览器失败。
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri 命令签名由 IPC 框架反序列化决定"
)]
pub fn open_external(url: String) -> Result<(), String> {
    crate::open_external(&url).map_err(|e| e.to_string())
}

/// 初始化收尾：主密码表单提交（前端按 ProbeDone.has_data 显示接入/
/// 新设表单后调用）。
///
/// # Errors
/// 无进行中的初始化 / 主密码不匹配远端数据 / 保存失败。
#[tauri::command]
pub async fn init_finish(
    passphrase: String,
    state: State<'_, SharedState>,
) -> Result<crate::InitStage, String> {
    let shared = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::init_finish(&shared, &passphrase))
        .await
        .map_err(|e| join_err(&e))?
        .map_err(|e| err_str(&e))
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
    let sh = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::search_view(&sh, &query))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// 按表单值拉取模型名列表（list-models，2026-10-27 增补）；失败返回空。
#[tauri::command]
pub async fn fetch_models(
    protocol: String,
    base_url: String,
    api_key: String,
) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        crate::fetch_models_cmd(&protocol, &base_url, &api_key)
    })
    .await
    .map_err(|e| e.to_string())
}

/// 重新授权 GitHub（token 失效自愈入口，设置页）。
#[tauri::command]
pub async fn github_reauthorize(
    state: tauri::State<'_, crate::SharedState>,
) -> Result<crate::InitStage, String> {
    let sh = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || crate::github_reauthorize(&sh))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// 当前应用版本号（设置页常态显示，不联网）。
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri 命令签名由 IPC 框架反序列化决定"
)]
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
