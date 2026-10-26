//! 应用内更新（D16，2026-10-25）：官方 updater 插件 + GitHub Releases。
//!
//! 流程：设置页手动检查 → 拉 latest.json 比版本 → 下载安装包 → 验签
//! （Ed25519，公钥编译进 tauri.conf.json）→ 静默安装 → 重启。
//!
//! ## 破坏性升级守卫
//!
//! vault 加密格式变更这类破坏性版本**不自动安装**（见 AGENTS.md 红线：
//! 格式变更需主人批准，且用户应先备份）。判定两条件任一即拦：
//! - Release notes 首行以 `[breaking]` 标记（发布时人工标注，最可靠）；
//! - 主版本号提升（语义化约定）。
//!
//! 拦下后前端只展示版本说明与手动升级指引，不提供一键升级按钮。

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

/// 检查结果视图（前端据此渲染 + 决定是否给出升级按钮）。
#[derive(Debug, Serialize)]
pub struct UpdateView {
    /// 当前运行版本。
    pub current_version: String,
    /// 是否有可用更新。
    pub available: bool,
    /// 新版本号（无更新时为 None）。
    pub version: Option<String>,
    /// 新版本说明（Release notes，无更新时为 None）。
    pub notes: Option<String>,
    /// 破坏性升级标记：为 true 时前端禁止一键升级。
    pub breaking: bool,
}

/// 破坏性判定：notes 首行 `[breaking]` 标记，或主版本号提升。
/// 独立成纯函数便于测试（AGENTS.md：新行为必须配测试）。
pub fn is_breaking(current: &str, target: &str, notes: &str) -> bool {
    if notes.trim_start().starts_with("[breaking]") {
        return true;
    }
    major_of(current) < major_of(target)
}

/// 取 semver 主版本号；解析失败按 0 处理（宁可保守放行普通更新，
/// 破坏性场景另有 notes 标记兜底）。
fn major_of(v: &str) -> u64 {
    v.split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0)
}

/// 检查更新（联网读 GitHub Releases 的 latest.json）。
///
/// # Errors
/// 网络 / endpoint 解析 / 验签配置错误（String 化，直接透传给前端展示）。
pub async fn check(app: &AppHandle) -> Result<UpdateView, String> {
    let updater = app.updater_builder().build().map_err(|e| e.to_string())?;
    match updater.check().await.map_err(|e| e.to_string())? {
        Some(u) => Ok(UpdateView {
            current_version: u.current_version.clone(),
            available: true,
            version: Some(u.version.clone()),
            notes: u.body.clone(),
            breaking: is_breaking(
                &u.current_version,
                &u.version,
                u.body.as_deref().unwrap_or(""),
            ),
        }),
        None => Ok(UpdateView {
            current_version: app.package_info().version.to_string(),
            available: false,
            version: None,
            notes: None,
            breaking: false,
        }),
    }
}

/// 下载并安装已检查到的更新，完成后重启应用。
///
/// # Errors
/// 下载 / 验签失败 / 安装器启动失败（String 化，直接透传给前端展示）。
pub async fn apply(app: &AppHandle) -> Result<(), String> {
    let updater = app.updater_builder().build().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no update available")?;
    // 破坏性守卫必须在执行层复检（2026-10-26 审查）：前端的按钮隐藏只是
    // 第一道展示防线，可被绕过；且"检查→点击"之间远端可能换成破坏性版本，
    // apply 内部重新 check 拿到的才是将要安装的版本
    if is_breaking(
        &update.current_version,
        &update.version,
        update.body.as_deref().unwrap_or(""),
    ) {
        return Err(
            "this is a breaking update; back up your data and follow the manual upgrade guide"
                .into(),
        );
    }
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    // NSIS 静默安装完成后重启进新版本；restart 不返回
    app.restart();
}

#[cfg(test)]
mod tests {
    //! 破坏性守卫判定回归（D16）：notes 标记与主版本提升两路信号。
    use super::is_breaking;

    #[test]
    fn breaking_by_notes_marker_and_major_bump() {
        // notes 首行标记：任意版本跨度都拦
        assert!(is_breaking(
            "0.2.0",
            "0.3.0",
            "[breaking]
vault format v2"
        ));
        assert!(is_breaking("1.0.0", "1.1.0", "[breaking] config migration"));
        // 主版本提升：即使忘标 notes 也拦
        assert!(is_breaking("0.9.0", "1.0.0", "stable release"));
        // 普通更新：放行
        assert!(!is_breaking("0.1.0", "0.2.0", "bug fixes"));
        assert!(!is_breaking("0.1.0", "0.1.1", ""));
        // notes 标记不在首行不生效（约定首行）
        assert!(!is_breaking(
            "0.1.0",
            "0.2.0",
            "fixes
[breaking] buried"
        ));
    }
}
