//! 终端交互辅助：密码解析链、隐藏输入、通用询问。
//!
//! ## 密码解析链（docs/design.md §7、§9.1）
//!
//! 优先级：环境变量 `MYAPIKEY_PASSPHRASE` > 系统凭据库 > 交互输入。
//! 凭据库里的密码若解不开当前 vault（密码已在别的设备改过），
//! 自动删除失效条目并回退到手动输入——这正是“记住，除非不对”的实现。

use myapikey_core::{keychain, vault, Error, Result};
use std::io::{BufRead, Write};

/// 环境变量名：非交互环境（CI/脚本）传入主密码的逃生通道。
pub const PASSPHRASE_ENV: &str = "MYAPIKEY_PASSPHRASE";

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
        eprintln!("Note: the saved passphrase is stale (likely changed on another device); enter it again.");
    }

    // 3) 交互输入：最多两次机会；成功后按 remember_days 续期记住
    for _ in 0..2 {
        let p = prompt_hidden("Master passphrase: ");
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

/// 设置新主密码时的输入：两次一致 + 最短长度校验。
///
/// # Errors
/// - [`Error::Usage`]：两次输入不一致。
pub fn new_passphrase() -> Result<String> {
    println!("Set the master passphrase.");
    println!("  If you forget it, ALL data is unrecoverable — keep it in a password manager.");
    loop {
        let a = prompt_hidden("New master passphrase: ");
        if a.chars().count() < 8 {
            println!("  Warning: shorter than 8 characters — consider a longer one.");
        }
        let b = prompt_hidden("Confirm passphrase: ");
        if a == b {
            if a.is_empty() {
                println!("Passphrase must not be empty; try again.");
                continue;
            }
            return Ok(a);
        }
        println!("Passphrases do not match; try again.");
    }
}

/// 隐藏输入一行敏感信息（不回显）。
pub fn prompt_hidden(label: &str) -> String {
    rpassword::prompt_password(label).unwrap_or_default()
}

/// 普通一行输入（回显），去掉首尾空白。EOF 视为空串。
pub fn prompt_line(label: &str) -> String {
    print!("{label}");
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    let _ = std::io::stdin().lock().read_line(&mut buf);
    buf.trim().to_string()
}

/// 是/否确认，回车取默认值。
pub fn confirm(label: &str, default_yes: bool) -> bool {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    let raw = prompt_line(&format!("{label} {hint} "));
    if raw.is_empty() {
        return default_yes;
    }
    matches!(raw.to_ascii_lowercase().as_str(), "y" | "yes")
}

/// 解析协议标签输入（openai / anthropic / o / a / 其他任意文本 = Custom）。
/// 输入若已带 `custom:` 前缀会被剥掉，避免显示成 `custom:custom:xxx`。
pub fn parse_protocol(input: &str) -> myapikey_core::model::Protocol {
    use myapikey_core::model::Protocol;
    match input.trim().to_ascii_lowercase().as_str() {
        "1" | "o" | "openai" => Protocol::OpenAi,
        "2" | "a" | "anthropic" => Protocol::Anthropic,
        other => Protocol::Custom {
            name: other.strip_prefix("custom:").unwrap_or(other).to_string(),
        },
    }
}

/// 交互式选择协议。
pub fn select_protocol(
    current: Option<&myapikey_core::model::Protocol>,
) -> myapikey_core::model::Protocol {
    use myapikey_core::model::Protocol;
    let cur = current.map(|p| p.to_string()).unwrap_or_default();
    let raw = prompt_line(&format!(
        "Protocol [1]openai [2]anthropic [3]custom text (Enter keeps {cur}): "
    ));
    if raw.is_empty() {
        current.cloned().unwrap_or(Protocol::OpenAi)
    } else {
        parse_protocol(&raw)
    }
}
