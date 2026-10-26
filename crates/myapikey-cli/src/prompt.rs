//! 终端交互辅助：密码解析链、隐藏输入、选择菜单、带默认值/校验的输入。
//!
//! ## 交互体系（2026-10-25 起，dialoguer）
//!
//! 所有交互原语收口在本模块：菜单（Select）、确认（Confirm）、隐藏输入
//! （Password）、带默认值/必填校验的输入（Input）。每个原语都在非 TTY
//! 环境（管道、CI、脚本驱动）自动降级为普通行输入，语义与交互态一致
//! ——脚本可驱动是硬约束，不允许交互组件把自动化通道堵死。
//!
//! ## 密码解析链（docs/design.md §7、§9.1）
//!
//! 优先级：环境变量 `MYAPIKEY_PASSPHRASE` > 系统凭据库 > 交互输入。
//! 凭据库里的密码若解不开当前 vault（密码已在别的设备改过），
//! 自动删除失效条目并回退到手动输入——这正是“记住，除非不对”的实现。

use dialoguer::{Confirm, Input, Password, Select};
use myapikey_core::{keychain, vault, Error, Result};
use std::io::{BufRead, Write};

/// 环境变量名：非交互环境（CI/脚本）传入主密码的逃生通道。
pub const PASSPHRASE_ENV: &str = "MYAPIKEY_PASSPHRASE";

/// dialoguer 组件的统一非 TTY 兜底：stdin/stderr 任一不是终端（管道、
/// CI、脚本驱动）就**完全不碰 dialoguer**，直接走 `fallback` 的行输入
/// 实现——console 后端在非终端流上的行为不可靠，实测 Password 会阻塞
/// 而不是返回错误。终端态下 interact 报错也回退，保证脚本永远可驱动。
fn interact_or<T>(ui: impl FnOnce() -> dialoguer::Result<T>, fallback: impl FnOnce() -> T) -> T {
    if !is_interactive() {
        return fallback();
    }
    ui().unwrap_or_else(|_| fallback())
}

/// stdin 与 stderr 都是终端才走 dialoguer 组件；管道/CI 走行输入降级。
fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// 解锁会话：得到可用密码 + 它的来源（用于提示与后续记住逻辑）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassSource {
    /// 来自环境变量。
    Env,
    /// 来自系统凭据库（被记住的密码）。
    Keychain,
    /// 来自本次交互输入。
    Prompt,
}

/// 按解析链取得能解开 vault 的主密码。
///
/// 解析链：环境变量 > 凭据库中未过期的记住密码 > 交互输入。
/// 交互输入成功后，若 `remember_days > 0` 会自动重新记住并续期
/// （2026-10-24 决议：输入一次默认有效 7 天，见 core::keychain 的 TTL 模型）。
///
/// # Errors
/// - [`Error::NotInitialized`]：vault 文件不存在；
/// - [`Error::Decrypt`]：手动输入的密码错误（连续两次后放弃，避免无限循环）；
/// - [`Error::Io`]：读取 vault 文件失败。
pub fn unlock(dirs: &myapikey_core::paths::AppDirs) -> Result<(String, PassSource)> {
    if !dirs.vault_path.is_file() {
        return Err(Error::NotInitialized);
    }
    let bytes = std::fs::read(&dirs.vault_path)?;
    let remember_days = myapikey_core::config::AppConfig::load(dirs)
        .map(|c| c.remember_days)
        .unwrap_or(7);

    // 1) 环境变量
    if let Ok(p) = std::env::var(PASSPHRASE_ENV) {
        if !p.is_empty() {
            if vault::decrypt(&bytes, &p).is_ok() {
                return Ok((p, PassSource::Env));
            }
            return Err(Error::Decrypt);
        }
    }

    // 2) 凭据库中未过期的记住密码（过期条目已被 get_with_expiry 顺手清理；
    //    凭据库不可用时返回 None，同样降级到输入）
    if let Ok(Some(p)) = keychain::get_with_expiry(dirs) {
        if vault::decrypt(&bytes, &p).is_ok() {
            return Ok((p, PassSource::Keychain));
        }
        // 记住的密码解不开（已在别处改过）：清掉，避免每次都走一遍无效尝试
        let _ = keychain::delete(dirs);
        eprintln!("Note: the saved passphrase is stale, likely changed on another device. Enter it again.");
    }

    // 3) 交互输入：最多两次机会；成功后按 remember_days 续期记住
    for _ in 0..2 {
        let p = prompt_hidden("Master passphrase");
        if vault::decrypt(&bytes, &p).is_ok() {
            if remember_days > 0 {
                // 记不住（无头 Linux 等）不视为错误，只提示一次
                if keychain::set_ttl(dirs, &p, remember_days).is_err() {
                    eprintln!("Note: cannot remember the passphrase on this machine; it will be asked every time.");
                }
            }
            return Ok((p, PassSource::Prompt));
        }
        eprintln!("Wrong passphrase or corrupted data; try again.");
    }
    Err(Error::Decrypt)
}

/// 设置新主密码：隐藏输入 + 二次确认（dialoguer 内建比对重试），
/// 过短只警告不阻止（用户自己权衡）；非 TTY 降级为两次行输入比对。
///
/// # Errors
/// 理论上不失败（空密码由组件原地重问）；保留 `Result` 与调用方契约。
pub fn new_passphrase() -> Result<String> {
    println!("Set the master passphrase.");
    println!("  If you forget it, ALL data is unrecoverable.\n  Keep it in a password manager.");
    let p = if is_interactive() {
        match Password::new()
            .with_prompt("New master passphrase")
            .with_confirmation("Confirm passphrase", "Passphrases do not match, try again")
            .allow_empty_password(false)
            .interact()
        {
            Ok(p) => p,
            // 组件异常（极少见）时降级到行输入而不是失败
            Err(_) => new_passphrase_fallback()?,
        }
    } else {
        new_passphrase_fallback()?
    };
    if p.chars().count() < 8 {
        println!("  Warning: shorter than 8 characters. Consider a longer one.");
    }
    Ok(p)
}

/// 非 TTY 的密码设置：两次行输入比对；EOF（输入耗尽）返回错误退出，
/// 而不是把空值喂给重试循环死循环（2026-10-26 审查修复）。
fn new_passphrase_fallback() -> Result<String> {
    loop {
        let a = prompt_line_opt("New master passphrase: ").ok_or(Error::Usage(
            "input ended before the passphrase was set".into(),
        ))?;
        if a.is_empty() {
            println!("Passphrase must not be empty; try again.");
            continue;
        }
        let b = prompt_line_opt("Confirm passphrase: ").ok_or(Error::Usage(
            "input ended before the passphrase was confirmed".into(),
        ))?;
        if a == b {
            return Ok(a);
        }
        println!("Passphrases do not match; try again.");
    }
}

/// 隐藏输入一行敏感信息（不回显）：主密码、API key。
/// 非 TTY 下降级为回显行输入——脚本场景输入本来就来自管道，可接受。
pub fn prompt_hidden(label: &str) -> String {
    interact_or(
        || Password::new().with_prompt(label).interact(),
        || prompt_line(&format!("{label}: ")),
    )
}

/// 普通一行输入（回显），去掉首尾空白。返回 `None` = stdin 已关闭（EOF）。
/// **重问类循环必须把 None 当"输入结束"错误退出**，当作空值重试会在
/// 输入耗尽（管道、CI 忘传字段）时无限循环（2026-10-26 审查修复）。
pub fn prompt_line_opt(label: &str) -> Option<String> {
    print!("{label}");
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    match std::io::stdin().lock().read_line(&mut buf) {
        Ok(0) => None,
        Ok(_) => Some(buf.trim().to_string()),
        Err(_) => None,
    }
}

/// [`prompt_line_opt`] 的便捷包装：EOF 视为空串。
/// 仅用于"空值 = 合法默认"的场景（如 confirm 回车取默认）；
/// 必填/重问场景一律用 `prompt_line_opt` 自行处理 EOF。
pub fn prompt_line(label: &str) -> String {
    prompt_line_opt(label).unwrap_or_default()
}

/// 带默认值的输入（edit 场景的"回车保留"）：交互态显示 `Label [当前值]:`，
/// 回车直接取默认值；非 TTY 降级为同样的行输入格式。
pub fn prompt_default(label: &str, current: &str) -> String {
    interact_or(
        || {
            Input::<String>::new()
                .with_prompt(label)
                .default(current.to_string())
                .show_default(true)
                .interact_text()
        },
        || {
            let raw = prompt_line(&format!("{label} [{current}]: "));
            if raw.is_empty() {
                current.to_string()
            } else {
                raw
            }
        },
    )
}

/// 必填输入：空值原地重问（组件内建校验），直到非空；
/// EOF（输入结束）返回错误而不是死循环。
///
/// # Errors
/// [`Error::Usage`]：stdin 关闭。
pub fn prompt_required(label: &str) -> Result<String> {
    if !is_interactive() {
        return required_fallback(label);
    }
    match Input::<String>::new().with_prompt(label).interact_text() {
        Ok(v) => Ok(v),
        Err(_) => required_fallback(label),
    }
}

/// 必填输入的非 TTY 降级：空值重问；EOF 返回错误退出（防死循环）。
fn required_fallback(label: &str) -> Result<String> {
    loop {
        let raw = prompt_line_opt(&format!("{label}: "))
            .ok_or(Error::Usage("input ended before a value was given".into()))?;
        if !raw.is_empty() {
            return Ok(raw);
        }
        println!("Must not be empty.");
    }
}

/// 必填 URL 输入：格式不合规则原地重问（2026-10-26 主人反馈：交互
/// 填写中途校验失败直接退出，前面填的内容全部白填）。规则见
/// `myapikey_core::store::is_valid_http_url`（仅 http/https + 主机非空）。
pub fn prompt_http_url(label: &str) -> Result<String> {
    loop {
        let raw = prompt_required(label)?;
        if myapikey_core::store::is_valid_http_url(&raw) {
            return Ok(raw);
        }
        println!("Must start with http:// or https:// followed by a host; try again.");
    }
}

/// 是/否确认，回车取默认值；Esc/中断视为否定（保守侧）。
pub fn confirm(label: &str, default_yes: bool) -> bool {
    interact_or(
        || {
            Confirm::new()
                .with_prompt(label)
                .default(default_yes)
                .interact_opt()
                .map(|v| v.unwrap_or(false))
        },
        || {
            let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
            let raw = prompt_line(&format!("{label} {hint} "));
            if raw.is_empty() {
                return default_yes;
            }
            matches!(raw.to_ascii_lowercase().as_str(), "y" | "yes")
        },
    )
}

/// 解析协议标签输入（openai / anthropic / o / a / 其他任意文本 = Custom）。
/// 输入若已带 `custom:` 前缀会被剥掉，避免显示成 `custom:custom:xxx`。
pub fn parse_protocol(input: &str) -> myapikey_core::model::Protocol {
    // 唯一实现在 core（2026-10-26 去重：这里曾有一份逐行拷贝，漂移
    // 风险高）；--protocol 参数与菜单共走此入口
    myapikey_core::model::Protocol::from_label(input)
}

/// 交互式选择协议：上下键菜单，回车取默认值。
///
/// 默认项 = 当前协议（edit 场景），无当前值时为 OpenAI（add 场景）。
/// 只有选中 Custom label 才追问标签——与 GUI 的下拉行为一致，选内置
/// 项不需要任何额外输入（2026-10-25 需求：上下键选择，填写式太难用）。
///
/// 菜单项同时标注数字，纯数字设备/脚本场景也能按 1/2/3 直接选。
pub fn select_protocol(
    current: Option<&myapikey_core::model::Protocol>,
) -> Result<myapikey_core::model::Protocol> {
    use myapikey_core::model::Protocol;

    let default = current.cloned().unwrap_or(Protocol::OpenAi);
    let items = ["1) OpenAI", "2) Anthropic", "3) Custom label"];
    let default_idx = match default {
        Protocol::OpenAi => 0,
        Protocol::Anthropic => 1,
        Protocol::Custom { .. } => 2,
    };
    // interact_opt：Esc/中断 → None（视为保持默认）；非 TTY → Err 走行输入菜单
    let idx = if is_interactive() {
        match Select::new()
            .with_prompt("Select protocol")
            .items(items)
            .default(default_idx)
            .interact_opt()
        {
            Ok(Some(i)) => i,
            // Esc/中断 = 保持默认；组件异常降级行输入菜单
            Ok(None) => default_idx,
            Err(_) => fallback_select(default_idx)?,
        }
    } else {
        fallback_select(default_idx)?
    };
    Ok(match idx {
        0 => Protocol::OpenAi,
        1 => Protocol::Anthropic,
        _ => loop {
            let name = prompt_line_opt("Custom protocol label: ").ok_or(Error::Usage(
                "input ended before the label was given".into(),
            ))?;
            let name = name.strip_prefix("custom:").unwrap_or(&name).to_string();
            if !name.is_empty() {
                return Ok(Protocol::Custom { name });
            }
            println!("Label must not be empty.");
        },
    })
}

/// 非 TTY 环境的兜底：打印编号菜单并读一行数字，回车取默认；
/// EOF 返回错误（调用方退出，避免死循环）。
fn fallback_select(default_idx: usize) -> Result<usize> {
    println!("Select protocol:");
    println!("  1) OpenAI");
    println!("  2) Anthropic");
    println!("  3) Custom label");
    let default_label = ["1", "2", "3"][default_idx];
    loop {
        let raw = prompt_line_opt(&format!("Choice (Enter for {default_label}): "))
            .ok_or(Error::Usage("input ended before a choice was made".into()))?;
        match raw.as_str() {
            "" => return Ok(default_idx),
            "1" => return Ok(0),
            "2" => return Ok(1),
            "3" => return Ok(2),
            _ => println!("Enter 1, 2 or 3."),
        }
    }
}
