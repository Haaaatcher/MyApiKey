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

use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, FuzzySelect, Input, MultiSelect, Password, Select};
use myapikey_core::{keychain, vault, Error, Result};
use std::io::{BufRead, Write};

/// 环境变量名：非交互环境（CI/脚本）传入解锁密码的逃生通道。
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
/// stdin/stderr 是否都是终端（管道/CI = false）。公开给 commands
/// 做"交互分支 vs 脚本分支"的显式分派（如 edit 的 Name 重问防挂）。
pub fn is_interactive() -> bool {
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

/// 按解析链取得能解开 vault 的解锁密码（passphrase）。
///
/// 解析链：环境变量 > 凭据库中未过期的记住密码 > 交互输入。
/// 交互输入成功后按自动锁定时长记住并续期
/// （D32 语义统一：记住副本时长 = `auto_lock_minutes`，见 core::keychain）。
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
    let minutes = myapikey_core::config::AppConfig::load(dirs).map_or(10, |c| c.auto_lock_minutes);

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
            // D32 语义补齐（2026-11-11）：免输解锁同样滚动续期——此前只有
            // 手动输入路径续期，持续使用时免输窗口死板地停在第一次写入的
            // TTL，一到点就要重输；GUI 侧常驻 tick 是持续滚动的，用户实测
            // 「CLI 锁定时间比 GUI 短」正是此处缺失
            let _ = keychain::set_ttl(dirs, &p, minutes);
            return Ok((p, PassSource::Keychain));
        }
        // 记住的密码解不开（已在别处改过）：清掉，避免每次都走一遍无效尝试
        let _ = keychain::delete(dirs);
        eprintln!("Note: the saved passphrase is stale, likely changed on another device. Enter it again.");
    }

    // 3) 交互输入：最多两次机会；成功后按自动锁定时长续期记住
    for _ in 0..2 {
        let p = prompt_hidden("Passphrase");
        if vault::decrypt(&bytes, &p).is_ok() {
            // 记不住（无头 Linux 等）不视为错误，只提示一次
            if keychain::set_ttl(dirs, &p, minutes).is_err() {
                eprintln!("Note: cannot remember the passphrase on this machine; it will be asked every time.");
            }
            return Ok((p, PassSource::Prompt));
        }
        eprintln!("Wrong passphrase or corrupted data; try again.");
    }
    Err(Error::Decrypt)
}

/// sync 专用的密码解析（D55）：与 [`unlock`] 相同的三级来源（环境变量、
/// 凭据库中未过期的记住密码、交互输入），但**全程不用本地 vault
/// 预校验**——换绑到「主密码不同的远端」后，本地旧 vault 解不开远端
/// 密码，预校验会把正确的远端密码挡在门外（2026-11-12 用户实测：
/// Windows 主密码在提示循环里被本地旧 vault 判错，永远到不了远端）。
/// 密码正确性由 `sync::sync` 状态机按分支如实报错：拉取路径报远端
/// 不匹配，dirty 路径（合并/基线上传）报本地不匹配。
///
/// 记住副本与交互输入的 TTL 续期由调用方在 sync **成功后**执行——
/// 失败时续期等于把错误密码再留一个锁定周期（旧实现在来源解析时
/// 就续期，与「远端密码不同」场景冲突）。
///
/// # Errors
/// - [`Error::NotInitialized`]：vault 文件不存在。
pub fn passphrase_for_sync(dirs: &myapikey_core::paths::AppDirs) -> Result<(String, PassSource)> {
    if !dirs.vault_path.is_file() {
        return Err(Error::NotInitialized);
    }
    // 1) 环境变量：显式意图，原样采用（脚本/自动化场景，校验交给 sync）
    if let Ok(p) = std::env::var(PASSPHRASE_ENV) {
        if !p.is_empty() {
            return Ok((p, PassSource::Env));
        }
    }
    // 2) 凭据库中未过期的记住密码（过期条目已被 get_with_expiry 顺手清理）
    if let Ok(Some(p)) = keychain::get_with_expiry(dirs) {
        return Ok((p, PassSource::Keychain));
    }
    // 3) 交互输入：单次采用，失败由 sync 的分支错误如实呈现
    let p = prompt_hidden("Passphrase");
    Ok((p, PassSource::Prompt))
}

/// 设置新解锁密码：隐藏输入 + 二次确认（dialoguer 内建比对重试）；
/// 长度硬性限制（core `vault::validate_passphrase` 同规则：8–128 字符，
/// 越界原地重问，2026-11-04 用户要求）；非 TTY 降级为两次行输入比对。
///
/// # Errors
/// 理论上不失败（空密码/不匹配/越界均原地重问）；保留 `Result` 与调用方契约。
pub fn new_passphrase() -> Result<String> {
    println!("Set the passphrase.");
    println!("  If you forget it, ALL data is unrecoverable.\n  Keep it in a password manager.");
    loop {
        let p = if is_interactive() {
            match Password::with_theme(theme())
                .with_prompt("New passphrase")
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
        // 长度硬性限制:越界原地重问(与 GUI/后端同一规则)
        let n = p.chars().count();
        if n < myapikey_core::vault::PASSPHRASE_MIN_CHARS {
            println!("  Passphrase must be at least 8 characters; try again.");
            continue;
        }
        if n > myapikey_core::vault::PASSPHRASE_MAX_CHARS {
            println!("  Passphrase must be at most 128 characters; try again.");
            continue;
        }
        return Ok(p);
    }
}

/// 非 TTY 的密码设置：两次行输入比对；EOF（输入耗尽）返回错误退出，
/// 而不是把空值喂给重试循环死循环（2026-10-26 审查修复）。
fn new_passphrase_fallback() -> Result<String> {
    loop {
        let a = prompt_line_opt("New passphrase: ").ok_or(Error::Usage(
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

/// 隐藏输入一行敏感信息（不回显）：解锁密码、API key。
/// 非 TTY 下降级为回显行输入——脚本场景输入本来就来自管道，可接受。
pub fn prompt_hidden(label: &str) -> String {
    interact_or(
        || Password::with_theme(theme()).with_prompt(label).interact(),
        || prompt_line(&format!("{label}: ")),
    )
}

/// 隐藏输入的**可空变体**：回车返回空串而非原地重问——供"Enter keeps
/// current"语义使用（如 `makara edit` 的 key 保留现值）。dialoguer 的
/// `Password` 默认拒绝空输入并静默重问，直接复用 [`prompt_hidden`] 会让
/// "回车保留"永远无法提交（2026-10-27 用户实测反馈）。
/// 非 TTY 下降级为回显行输入（回车 = 空串，语义一致）。
pub fn prompt_hidden_optional(label: &str) -> String {
    interact_or(
        || {
            Password::with_theme(theme())
                .with_prompt(label)
                .allow_empty_password(true)
                .interact()
        },
        || prompt_line(&format!("{label}: ")),
    )
}

// ---------------------------------------------------------------------------
// 网络等待指示器（2026-10-27 用户反馈：网络操作卡住无感知）
// ---------------------------------------------------------------------------

/// 网络等待的转轮守卫：构建时在 stderr 起一个后台线程旋转
/// `| / - \` + 描述文字，`Drop` 时停线程并清行。纯 std 实现（自绘
/// ASCII 帧加回车转义，不用 ANSI/转义序列）——Windows 老控制台无需
/// VT 支持也不花屏；不引入新依赖。
///
/// 仅 TTY 下生效：stderr 非终端（管道/CI）时返回 `None`、什么都不
/// 打印——输出保持干净可解析。
///
/// 用法：`let _spin = wait_spinner("Syncing…");` 绑定到变量存活至
/// 操作结束。
pub struct WaitSpinner {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WaitSpinner {
    /// 启动转轮。非 TTY 返回 `None`（不产生任何输出）。
    pub fn start(message: &str) -> Option<Self> {
        use std::io::IsTerminal;
        if !std::io::stderr().is_terminal() {
            return None;
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = std::sync::Arc::clone(&stop);
        let msg = message.to_string();
        let handle = std::thread::spawn(move || {
            const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
            let mut i = 0usize;
            let mut stderr = std::io::stderr();
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = write!(stderr, "\r{} {}", FRAMES[i % FRAMES.len()], msg);
                let _ = stderr.flush();
                i += 1;
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            // 清行：按字节数放大的空白覆盖（中文宽字符宁可多擦不可残留）
            let _ = write!(stderr, "\r{}\r", " ".repeat(msg.len() * 2 + 8));
            let _ = stderr.flush();
        });
        Some(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for WaitSpinner {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// [`WaitSpinner::start`] 的便捷入口。
pub fn wait_spinner(message: &str) -> Option<WaitSpinner> {
    WaitSpinner::start(message)
}

/// 普通一行输入（回显），去掉首尾空白。返回 `None` = stdin 已关闭（EOF）。
/// **重问类循环必须把 None 当"输入结束"错误退出**，当作空值重试会在
/// 输入耗尽（管道、CI 忘传字段）时无限循环（2026-10-26 审查修复）。
///
/// 交互态走 dialoguer（`Input` + 允许空提交）：dialoguer 组件自己管理
/// 终端模式，组件结束后紧接裸 `read_line` 在 Windows 终端上会收不到
/// 提交（apply 询问项"无法输入"的根因，2026-10-29 修复）——所有交互
/// 输入必须同走一条终端管理路径。Esc/组件异常降级为行读取。
pub fn prompt_line_opt(label: &str) -> Option<String> {
    if is_interactive() {
        if let Ok(v) = Input::with_theme(theme())
            .with_prompt(label)
            .allow_empty(true)
            .interact_text()
        {
            return Some(v);
        }
    }
    print!("{label}");
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    // EOF(0 行)与读错误同语义:视为无输入
    match std::io::stdin().lock().read_line(&mut buf) {
        Ok(n) if n > 0 => Some(buf.trim().to_string()),
        _ => None,
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
/// 编辑态预填充输入（WYSIWYG，2026-10-28 取代"[current] 回车保留 / -
/// 清空"约定）：输入框直接预填当前值，用户原地编辑——所见即所提交，
/// 删光回车 = 提交空值（可选字段由此清空；必填字段由调用方重问）。
/// 非 TTY：空输入 = 保留现值（脚本场景的安全默认）。
pub fn prompt_default(label: &str, current: &str) -> String {
    interact_or(
        || {
            Input::with_theme(theme())
                .with_prompt(label)
                .with_initial_text(current)
                // 允许空提交：空回车 = 清空（可选字段语义），而不是被
                // 组件静默重问（"回车没反应"的根因，2026-10-28 修复）
                .allow_empty(true)
                .interact_text()
        },
        || {
            print!("{label} [{current}]: ");
            let _ = std::io::stdout().flush();
            let mut buf = String::new();
            // EOF(0 行)与读错误同语义:保留原值
            match std::io::stdin().lock().read_line(&mut buf) {
                Ok(n) if n > 0 => buf.trim().to_string(),
                _ => current.to_string(),
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

/// 全局交互主题：所有 dialoguer 组件共用一份 ColorfulTheme——统一的
/// `? … ›` 进行时样式与 `✔ 字段 · 值` 完成痕迹，让逐项询问呈现连续的
/// 填表视觉（字段名 · 填入值 一行一条）。每次新建主题等价但为一致性与
/// 未来定制留单一入口。
fn theme() -> &'static ColorfulTheme {
    use std::sync::OnceLock;
    static THEME: OnceLock<ColorfulTheme> = OnceLock::new();
    THEME.get_or_init(ColorfulTheme::default)
}

/// 辅助说明行样式：交互态 dim（弱于字段名与值的视觉层级），非交互态
/// 原样（管道日志可读）。用于 apply 表单里每个字段上方的含义说明。
pub fn dim(text: &str) -> String {
    if is_interactive() {
        format!("\x1b[2m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// 成功行标记：绿色 `✔`（与 dialoguer ColorfulTheme 的完成痕迹同色
/// 同符），非交互态退化为无色 `✓`（管道/重定向不含 ANSI 码）。
///
/// 与 [`warn`]、组件主题共同构成三层视觉语言：绿 ✔ = 成功/完成，
/// 黄 ⚠ = 警告，红 ✗ = 失败。
pub fn ok(msg: &str) {
    let mark = if is_interactive() {
        "\x1b[32m\u{2714}\x1b[0m"
    } else {
        "✓"
    };
    println!("{mark} {msg}");
}

/// 警告行标记：黄色 `⚠`（ColorfulTheme 的警告色系），非交互态无色。
/// 警告与错误同走 stderr（clig.dev：数据/结果归 stdout，诊断归
/// stderr——管道重定向时日志不被警告污染）。
pub fn warn(msg: &str) {
    let mark = if is_interactive() {
        "\x1b[33m\u{26a0}\x1b[0m"
    } else {
        "⚠"
    };
    eprintln!("{mark} {msg}");
}

/// 是/否确认，回车取默认值；Esc/中断视为否定（保守侧）。
pub fn confirm(label: &str, default_yes: bool) -> bool {
    interact_or(
        || {
            Confirm::with_theme(theme())
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
    let items = [
        "1) OpenAI Responses (recommended)",
        "2) OpenAI Chat (widely compatible)",
        "3) Anthropic Messages (standard)",
        "4) Anthropic Complete (deprecated)",
        "5) Custom label",
    ];
    let default_idx = match default {
        Protocol::OpenAi => 0,
        Protocol::OpenAiChat => 1,
        Protocol::Anthropic => 2,
        Protocol::AnthropicComplete => 3,
        Protocol::Custom { .. } => 4,
    };
    // interact_opt：Esc/中断 → None（视为保持默认）；非 TTY → Err 走行输入菜单
    let idx = if is_interactive() {
        match Select::with_theme(theme())
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
        1 => Protocol::OpenAiChat,
        2 => Protocol::Anthropic,
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
    println!("  1) OpenAI Responses (recommended)");
    println!("  2) OpenAI Chat (widely compatible)");
    println!("  3) Anthropic Messages (standard)");
    println!("  4) Anthropic Complete (deprecated)");
    println!("  5) Custom label");
    let default_label = ["1", "2", "3", "4", "5"][default_idx];
    loop {
        let raw = prompt_line_opt(&format!("Choice (Enter for {default_label}): "))
            .ok_or(Error::Usage("input ended before a choice was made".into()))?;
        match raw.as_str() {
            "" => return Ok(default_idx),
            "1" => return Ok(0),
            "2" => return Ok(1),
            "3" => return Ok(2),
            "4" => return Ok(3),
            "5" => return Ok(4),
            _ => println!("Enter 1, 2, 3, 4 or 5."),
        }
    }
}

/// 交互式单选（通用）：菜单项由调用方给定，返回选中下标。
/// 非 TTY 降级为编号行输入菜单（与其他原语一致，脚本可驱动）。
///
/// # Errors
/// [`Error::Usage`]：降级行输入遇 EOF。
pub fn select_from(labels: &[String], prompt: &str) -> Result<usize> {
    select_from_with_default(labels, prompt, 0)
}

/// [`select_from`] 带默认项的变体：`default_idx` 高亮为回车默认。
pub fn select_from_with_default(
    labels: &[String],
    prompt: &str,
    default_idx: usize,
) -> Result<usize> {
    if is_interactive() {
        match Select::with_theme(theme())
            .with_prompt(prompt)
            .items(labels)
            .default(default_idx)
            .interact_opt()
        {
            Ok(Some(i)) => return Ok(i),
            // Esc/中断 = 取默认项
            Ok(None) => return Ok(default_idx),
            Err(_) => {} // 组件异常 → 行输入兜底
        }
    }
    println!("{prompt}:");
    for (i, it) in labels.iter().enumerate() {
        let mark = if i == default_idx { "*" } else { " " };
        println!("  {mark} {:<2} {it}", i + 1);
    }
    loop {
        let raw = prompt_line_opt(&format!("Choice (Enter for {}): ", default_idx + 1))
            .ok_or(Error::Usage("input ended before a choice was made".into()))?;
        match raw.as_str() {
            "" => return Ok(default_idx),
            other => {
                if let Ok(n) = other.parse::<usize>() {
                    if (1..=labels.len()).contains(&n) {
                        return Ok(n - 1);
                    }
                }
                println!("Enter a number between 1 and {}.", labels.len());
            }
        }
    }
}

/// 交互式选择一条记录（`makara apply` 无 target 时用）：菜单项 =
/// `name  (protocol · 掩码 key)`，按最近更新倒序（与 `makara list` 一致，
/// 最常用的记录排最前）。返回选中记录的 **id**——调用方用 id 在自己的
/// 记录集合中定位，绝不返回下标：传入切片与调用方集合的索引基準不同
/// （调用方可能含墓碑），下标跨集合使用会写错记录（2026-10-27 实锤
/// 事故，修复方式 = 跨集合只传 id 不传下标）。
///
/// # Errors
/// [`Error::Usage`]：没有记录 / 降级行输入遇 EOF。
pub fn select_record(records: &[myapikey_core::model::Record]) -> Result<uuid::Uuid> {
    use myapikey_core::model::Record;
    if records.is_empty() {
        return Err(Error::Usage(
            "no records yet; add one first with `makara add`".into(),
        ));
    }
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(records[i].updated_at));
    let items: Vec<String> = order
        .iter()
        .map(|&i| {
            let r: &Record = &records[i];
            format!(
                "{}  ({} · {})",
                r.name,
                r.protocol.display_label(),
                r.masked_key()
            )
        })
        .collect();
    let pick = |idx: usize| records[order[idx]].id;
    if is_interactive() {
        match Select::with_theme(theme())
            .with_prompt("Apply which record")
            .items(&items)
            .default(0)
            .interact_opt()
        {
            Ok(Some(i)) => return Ok(pick(i)),
            Ok(None) => return Ok(pick(0)),
            Err(_) => {}
        }
    }
    println!("Apply which record:");
    for (i, it) in items.iter().enumerate() {
        println!("  {:<2} {it}", i + 1);
    }
    loop {
        let raw = prompt_line_opt("Choice (Enter for 1): ")
            .ok_or(Error::Usage("input ended before a choice was made".into()))?;
        match raw.as_str() {
            "" => return Ok(pick(0)),
            other => {
                if let Ok(n) = other.parse::<usize>() {
                    if (1..=items.len()).contains(&n) {
                        return Ok(pick(n - 1));
                    }
                }
                println!("Enter a number between 1 and {}.", items.len());
            }
        }
    }
}

/// 交互式多选（通用）：返回选中的项（按给定顺序）。
/// `precheck_all` = 是否默认全勾；非 TTY：全勾时回车收下全部，否则
/// 空输入返回空（安全默认）。占位 unknown 由调用方过滤后再传入。
pub fn multiselect_from(items: &[String], prompt: &str, precheck_all: bool) -> Vec<String> {
    if items.is_empty() {
        return vec![];
    }
    let defaults: Vec<bool> = vec![precheck_all; items.len()];
    if is_interactive() {
        // ColorfulTheme：勾选渲染为 ✔（绿）/ ⬚，替代歧义的 [x]/[ ]
        if let Ok(sel) = MultiSelect::with_theme(theme())
            .with_prompt(format!("{prompt} (space toggles, Enter confirms)"))
            .items(items)
            .defaults(&defaults)
            .interact()
        {
            return sel.into_iter().map(|i| items[i].clone()).collect();
        }
        return vec![]; // 组件异常：按取消处理（安全侧）
    }
    let hint = if precheck_all {
        "enter numbers to keep (e.g. 1,3; Enter keeps all)"
    } else {
        "enter numbers to include (e.g. 1,3; empty = none)"
    };
    println!("{prompt} — {hint}:");
    for (i, it) in items.iter().enumerate() {
        println!("  {:<2} {it}", i + 1);
    }
    let raw = prompt_line("");
    if raw.trim().is_empty() {
        return if precheck_all { items.to_vec() } else { vec![] };
    }
    let picked: std::collections::BTreeSet<usize> = raw
        .split([',', '，'])
        .filter_map(|t| t.trim().parse::<usize>().ok())
        .filter(|&n| (1..=items.len()).contains(&n))
        .map(|n| n - 1)
        .collect();
    picked.into_iter().map(|i| items[i].clone()).collect()
}

/// 模型清单非交互态的返回上限（管道/CI 宁多勿缺，用户可后续裁剪）。
pub const MODEL_LIST_SMALL_MAX: usize = 12;

/// 拉取到的模型清单 → 用户选定的模型列表（通用入口，add/edit/apply
/// 共用）。交互统一为**勾选式**（2026-11-11 用户定稿）：↑↓ 移动、
/// 空格选中/取消（可反复切换）、回车提交——取代此前「小清单勾选 +
/// 大清单模糊循环」的双模式：模糊循环选中即加入、无法反悔，用户实测
/// 不便；长列表滚动浏览，勾错一个空格即可撤回。
///
/// 全不选回车 = 可能要填清单之外的私有型号 → 手动输入兜底。
///
/// 非 TTY（管道/CI）：直接返回**前 [`MODEL_LIST_SMALL_MAX`] 项**——
/// 自动化场景没有交互，宁多勿缺，用户可后续 edit 裁剪。
pub fn pick_models(fetched: &[String]) -> Vec<String> {
    if fetched.is_empty() {
        return vec![];
    }
    if !is_interactive() {
        let n = fetched.len().min(MODEL_LIST_SMALL_MAX);
        return fetched[..n].to_vec();
    }
    // 全不选回车 = 清单里没有想要的 → 手动输入兜底（私有型号也能填）
    let picked = multiselect_from(fetched, "Models to apply", false);
    if picked.is_empty() {
        return manual_models_fallback();
    }
    picked
}

/// 手动输入兜底（交互选择后一无所获时）：逗号分隔填写；空输入返回空
/// 列表（models 可空，应用侧走默认模型）。
fn manual_models_fallback() -> Vec<String> {
    let raw = prompt_line("Models (comma-separated, Enter = not set): ");
    parse_comma_list(&raw)
}

/// 逗号分隔解析（模型清单覆盖输入用）。
fn parse_comma_list(raw: &str) -> Vec<String> {
    raw.split([',', '，'])
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect()
}

/// 模糊搜索单选（apply 槽位映射在大清单上使用）：Esc = 取默认项。
pub fn fuzzy_select_one(labels: &[String], prompt: &str, default_idx: usize) -> Result<usize> {
    if is_interactive() && labels.len() > MODEL_LIST_SMALL_MAX {
        match FuzzySelect::with_theme(theme())
            .with_prompt(prompt)
            .items(labels)
            .default(default_idx)
            .interact_opt()
        {
            Ok(Some(i)) => return Ok(i),
            Ok(None) => return Ok(default_idx),
            Err(_) => {} // 组件异常 → 行输入兜底
        }
    }
    select_from_with_default(labels, prompt, default_idx)
}

#[cfg(test)]
mod model_picker_tests {
    use super::*;

    /// 非 TTY：pick_models 返回前 N 项（宁多勿缺，可后续裁剪）。
    #[test]
    fn non_tty_returns_capped_head() {
        let fetched: Vec<String> = (0..30).map(|i| format!("m-{i}")).collect();
        let picked = pick_models(&fetched);
        assert_eq!(picked.len(), MODEL_LIST_SMALL_MAX);
        assert_eq!(picked[0], "m-0");
        // 小清单原样返回
        let small: Vec<String> = vec!["a".into(), "b".into()];
        assert_eq!(pick_models(&small), small);
    }
}
