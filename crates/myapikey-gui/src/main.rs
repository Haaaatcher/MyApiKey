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
        // 应用内更新（D16）：endpoints/公钥在 tauri.conf.json 的 plugins.updater
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(shared.clone())
        .setup(move |app| {
            // ---- 自动锁线程：周期检查空闲，超时销毁会话并通知前端 ----
            let handle = app.handle().clone();
            let watch_state = shared.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(LOCK_CHECK_INTERVAL);
                let limit =
                    Duration::from_secs(u64::from(watch_state.config().auto_lock_minutes) * 60);
                if limit.as_secs() > 0 && lock_if_idle(&watch_state, limit) {
                    // 前端监听此事件回到解锁页
                    let _ = handle.emit("locked", ());
                }
            });

            // ---- 系统托盘 ----
            // 菜单文案随语言设置（前端启动时上报，见 set_ui_lang）
            let menu = build_tray_menu(app.handle())?;
            tauri::tray::TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().expect("应用图标缺失").clone())
                .tooltip("MyApiKey")
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
            commands::sync_now,
            commands::status,
            commands::get_settings,
            commands::set_settings,
            commands::init_start,
            commands::init_device_authorized,
            commands::github_reauthorize,
            commands::app_version,
            commands::check_update,
            commands::apply_update,
        ])
        .run(tauri::generate_context!())
        .expect("tauri 应用启动失败");
}
