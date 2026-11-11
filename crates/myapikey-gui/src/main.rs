//! myapikey-gui 应用入口：Tauri 窗口、系统托盘、单实例、状态注入、
//! 自动锁后台线程。
//!
//! 发布版隐藏控制台（`windows_subsystem`），开发版保留便于看日志。
//!
//! 窗口与托盘策略（2026-10-25 需求）：
//! - **关闭与最小化都收进系统托盘**，进程与解锁会话保活——从托盘
//!   恢复窗口时仍是离开时的状态，不必重输主密码；
//! - 托盘菜单提供「显示主窗口 / 退出」，只有显式退出才结束进程；
//! - **单实例**：二次启动不创建新进程，而是把已有窗口调到前台
//!   （tauri-plugin-single-instance，官方插件）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use myapikey_gui::commands;
use myapikey_gui::{lock_if_idle, Shared, SharedState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::{Emitter, Manager, WindowEvent};

/// 自动锁检查周期（比最小可配置粒度 1 分钟更细，保证及时性）。
const LOCK_CHECK_INTERVAL: Duration = Duration::from_secs(15);

/// 界面是否为中文（托盘菜单据此本地化）。
/// 前端在启动与每次语言切换时调 [`set_ui_lang`] 上报；默认英文，
/// 前端 JS 就绪后毫秒级纠正。
static UI_LANG_ZH: AtomicBool = AtomicBool::new(false);

/// 托盘菜单文案（随语言设置变化，2026-10-25 需求：不再写死双语）。
fn tray_texts() -> (&'static str, &'static str) {
    if UI_LANG_ZH.load(Ordering::Relaxed) {
        ("显示主窗口", "退出")
    } else {
        ("Show", "Quit")
    }
}

/// 按当前语言构建托盘菜单（id 固定为 show/quit，语言切换只换标签）。
fn build_tray_menu(app: &tauri::AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let (show_text, quit_text) = tray_texts();
    let show = MenuItem::with_id(app, "show", show_text, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", quit_text, true, None::<&str>)?;
    Menu::with_items(app, &[&show, &quit])
}

/// 前端上报界面语言并即时重建托盘菜单。
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri 命令签名由 IPC 框架决定"
)]
fn set_ui_lang(app: tauri::AppHandle, lang: String) {
    UI_LANG_ZH.store(lang.starts_with("zh"), Ordering::Relaxed);
    if let Some(tray) = app.tray_by_id("main-tray") {
        if let Ok(menu) = build_tray_menu(&app) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

/// 把主窗口调到前台并恢复显示。托盘菜单/二次启动/托盘图标点击共用。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "启动装配(插件/线程/托盘)线性编排,拆分反而掩盖初始化顺序"
)]
fn main() {
    let shared: SharedState = match Shared::new() {
        Ok(s) => std::sync::Arc::new(s),
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    tauri::Builder::default()
        // 单实例必须最先注册：二次启动不会走到这里，而是触发下面的回调
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        // 文件保存对话框（D49：GUI 导出让用户自选路径）
        .plugin(tauri_plugin_dialog::init())
        // 应用内更新（D16）：endpoints/公钥在 tauri.conf.json 的 plugins.updater
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(std::sync::Arc::clone(&shared))
        .setup(move |app| {
            // ---- 启动主动续期(2026-11-03):令牌过期/临近过期时静默
            // 换新,用户启动应用即恢复可用,不必等第一次同步撞 401;
            // 凭据库独立于 vault 会话,无需解锁。
            // 仅「GitHub 明确拒绝」(令牌被吊销/卸载 App 即全令牌撤销、
            // 或本机凭据链残缺)才 emit auth-invalid 显示重新授权引导;
            // 网络抖动类失败不打扰——断网启动曾误导用户重授权
            // (2026-11-05 审查:与 diagnose 的断网误判同类),下次点
            // 同步的被动续期会如实报错。
            // 文件日志:启动注入目录 + 按 config 设级别(设置页变更即时生效)
            myapikey_core::log::init(&shared.dirs.config_dir);
            myapikey_core::log::set_level(&shared.config().log_level);
            myapikey_core::log::info("app", "startup");

            let refresh_handle = app.handle().clone();
            std::thread::spawn(move || {
                // 错开启动 IO 尖峰;晚几秒续期无影响(提前量 10 分钟)
                std::thread::sleep(Duration::from_secs(2));
                let Ok(client_id) = myapikey_core::github::effective_client_id() else {
                    return;
                };
                if let Err(e) = myapikey_core::github::try_refresh_if_expired(&client_id) {
                    // 完整原因落文件日志;网络抖动不再 emit(2026-11-05):
                    // 只有 GitHub 明确拒绝才弹「授权已失效」引导
                    let msg = e.to_string();
                    myapikey_core::log::error("startup", &format!("token refresh failed: {msg}"));
                    let revoked =
                        msg.contains("GitHub refused") || msg.contains("no refresh token stored");
                    if revoked {
                        let _ = refresh_handle.emit("auth-invalid", msg);
                    }
                }
            });

            // ---- 自动锁线程：周期检查空闲，超时销毁会话并通知前端 ----
            // D32：会话存活期间滚动续期记住副本（TTL 与自动锁定同一时长）——
            // 持续使用的会话跨重启也保持免输；锁定/空闲到期由 lock_if_idle
            // 清副本。auto_lock_minutes = 0（永不锁定）时副本不过期，跳过续写。
            let handle = app.handle().clone();
            let watch_state = shared;
            std::thread::spawn(move || loop {
                std::thread::sleep(LOCK_CHECK_INTERVAL);
                let cfg = watch_state.config();
                let limit = Duration::from_secs(u64::from(cfg.auto_lock_minutes) * 60);
                if cfg.auto_lock_minutes > 0 {
                    let g = watch_state
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(s) = &g.session {
                        // 会话存活（未被上一轮锁定）即视为活跃：滚动续期副本。
                        // 节流（2026-11-05 审查可选 D）：距过期还剩半程以上
                        // 时跳过重写——15s tick 全量重写凭据库 + remember.json
                        // 各一次 IO 毫无意义。读不到过期时刻（无文件/损坏）
                        // 视为需重写；用户改短时长由 set_settings 即时重写
                        // 新副本，半程判定天然兼容
                        let half = limit / 2;
                        let remaining = myapikey_core::keychain::expires_at(&watch_state.dirs)
                            .ok()
                            .flatten()
                            .and_then(|at| {
                                at.signed_duration_since(chrono::Utc::now()).to_std().ok()
                            });
                        if remaining.is_none_or(|r| r < half) {
                            let _ = myapikey_core::keychain::set_ttl(
                                &watch_state.dirs,
                                &s.passphrase,
                                cfg.auto_lock_minutes,
                            );
                        }
                    }
                }
                if limit.as_secs() > 0 && lock_if_idle(&watch_state, limit) {
                    myapikey_core::log::info("lock", "auto-locked after idle");
                    // 前端监听此事件回到解锁页
                    let _ = handle.emit("locked", ());
                }
            });

            // ---- 系统托盘 ----
            // 菜单文案随语言设置（前端启动时上报，见 set_ui_lang）
            let menu = build_tray_menu(app.handle())?;
            tauri::tray::TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().expect("应用图标缺失").clone())
                .tooltip("Makara")
                .menu(&menu)
                // 左键单击托盘图标 = 显示主窗口（右键出菜单）
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;
            // 菜单动作：显示 / 真正退出（唯一的正常退出路径）
            app.on_menu_event(|app, event| match event.id().as_ref() {
                "show" => show_main_window(app),
                "quit" => app.exit(0),
                _ => {}
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if cfg!(debug_assertions) {
                eprintln!("[window-event] {event:?}");
            }
            match event {
                // 关闭按钮：不退出，收进托盘（进程与会话保活）
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                // 最小化：直接藏进托盘，任务栏不占位（用户 2026-10-25 要求）
                WindowEvent::Resized(_) if window.is_minimized().unwrap_or(false) => {
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            set_ui_lang,
            commands::gui_state,
            commands::unlock,
            commands::unlock_remembered,
            commands::lock,
            commands::list_records,
            commands::search_records,
            commands::save_record,
            commands::delete_record,
            commands::reveal_key,
            commands::copy_key,
            commands::copy_base_url,
            commands::copy_text,
            commands::check_key,
            commands::check_all_keys,
            commands::sync_now,
            commands::change_passphrase,
            commands::status,
            commands::diagnose_github,
            commands::github_install_url,
            commands::export_records,
            commands::import_records,
            commands::get_settings,
            commands::set_settings,
            commands::init_start,
            commands::forget_github_token,
            commands::init_device_authorized,
            commands::open_external,
            commands::init_finish,
            commands::github_reauthorize,
            commands::fetch_models,
            commands::app_version,
            commands::check_update,
            commands::apply_update,
        ])
        .run(tauri::generate_context!())
        .expect("tauri 应用启动失败");
}
