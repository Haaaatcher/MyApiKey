//! GitHub 集成：Device Flow 授权（RFC 8628）、token 存取与刷新、
//! 私有同步仓库的创建/识别，以及 Contents API 的 base64 编解码。
//!
//! D25（M4）起同步不再走 git——认证以 Bearer header 由 [`crate::remote`]
//! 使用；本模块不再提供 git 凭据注入（原 GitAuth/git_envs 已随系统 git
//! 依赖一并移除）。

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// GitHub OAuth App 的 client_id。
///
/// Device flow 只需要 client_id（client_secret 不参与）。
/// 此值为作者注册的官方应用；特殊部署可用环境变量
/// `MYAPIKEY_GITHUB_CLIENT_ID` 覆盖（开发/测试用）。
pub const CLIENT_ID: &str = "Iv23lili6qGAcDHCvePc";

/// 覆盖 client_id 的环境变量名。
pub const CLIENT_ID_ENV: &str = "MYAPIKEY_GITHUB_CLIENT_ID";

/// 新绑定未指定仓库时的默认仓库名（品牌名收尾,2026-11-10 起为
/// `makara-vault`）;也是 config `default_repo_name` 的缺省值。
pub const DEFAULT_REPO_NAME: &str = "makara-vault";

/// 凭据库中 GitHub token 的 account 名（与解锁密码的 `default` 区分开）。
const KEYCHAIN_ACCOUNT: &str = "github";
/// 刷新令牌的凭据条目账号（与 access token 分开存放；轮换后整体覆盖）。
const KEYCHAIN_REFRESH_ACCOUNT: &str = "github-refresh";
/// access 过期时刻（RFC3339）的凭据条目账号：开启过期模式的授权/续期
/// 写入，供启动时的主动续期判断（2026-11-03 需求 1）。
const KEYCHAIN_EXPIRES_ACCOUNT: &str = "github-expires";
/// GitHub App **client secret** 的凭据条目账号。实测（2026-11-06，假令牌
/// 与全新真令牌双重对照）：刷新端点**必须携带 client_secret**——官方
/// 文档"device flow 生成的令牌可豁免"与实践不符，不带即报
/// `incorrect_client_credentials`。secret 存凭据库（与 refresh token
/// 同级安全），env 可覆盖。
const KEYCHAIN_SECRET_ACCOUNT: &str = "github-client-secret";
/// client secret 的环境变量覆盖名。
pub const CLIENT_SECRET_ENV: &str = "MYAPIKEY_GITHUB_CLIENT_SECRET";
/// 刷新请求的 redirect_uri:实测(2026-11-08)携带 client_secret 的刷新
/// 请求必须附带 redirect_uri 且与 App 注册的 Callback URL 一致,否则
/// 报 redirect_uri_mismatch。device flow 授权本身不经回调,此值仅为
/// 满足刷新校验的占位(localhost 是 GitHub 明确允许的形态)。
pub const REFRESH_REDIRECT_URI: &str = "http://localhost/";
/// 内嵌默认 client secret(build.rs 从根目录 client-secret.txt 注入,
/// 文件 gitignore 永不入库;不存在时为空串)。大众分发形态下 secret
/// 随二进制分发——业界做法(VS Code / GitHub Desktop 同款),风险与
/// 边界见 build.rs 与 AGENTS 发布流程。
const EMBEDDED_CLIENT_SECRET: &str = env!("MYAPIKEY_EMBEDDED_CLIENT_SECRET");

/// 内嵌 secret 的有效性判断:文件不存在(空串)或仍是占位文本 → None。
fn embedded_client_secret() -> Option<String> {
    let s = EMBEDDED_CLIENT_SECRET.trim();
    if s.is_empty() || s.starts_with("REPLACE_WITH") {
        None
    } else {
        Some(s.to_string())
    }
}
/// App 安装页 slug:安装链接形如
/// `https://github.com/apps/{slug}/installations/new`。默认取实际创建的
/// App（MakaraApp），环境变量可覆盖（更换 App 的场景）。前端合成诊断
/// 里的占位安装链接与本值保持一致。
const APP_SLUG_ENV: &str = "MYAPIKEY_GITHUB_APP_SLUG";
const APP_SLUG_DEFAULT: &str = "makaraapp";

/// 安装页 slug（环境变量可覆盖，见 [`APP_SLUG_ENV`]）。
fn app_install_slug() -> String {
    std::env::var(APP_SLUG_ENV).unwrap_or_else(|_| APP_SLUG_DEFAULT.into())
}

/// 安装页 URL（未安装时引导用户跳转）。
/// slug 的**单一来源**（本函数）：GUI 前端经命令取值，不自行硬编码
/// （2026-11-05 审查可选 C——前端曾误写他 App 的 slug）。
pub fn app_install_url() -> String {
    format!(
        "https://github.com/apps/{}/installations/new",
        app_install_slug()
    )
}

/// 设备码请求的 scope：创建私有仓库 + git 推送所需。
const SCOPE: &str = "repo";

/// 设备授权发起后返回的信息。
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    /// 轮询换 token 用的码（保密，不能展示给用户看）。
    pub device_code: String,
    /// 用户在浏览器输入的 8 位码（展示给用户）。
    pub user_code: String,
    /// 用户要打开的地址（固定为 https://github.com/login/device）。
    pub verification_uri: String,
    /// 整个流程的有效秒数（GitHub 默认 900）。
    #[serde(default)]
    pub expires_in: u64,
    /// 轮询间隔秒数（GitHub 默认 5，遇 slow_down 会要求加长）。
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// 取生效的 client_id（环境变量 > 内置官方应用值）。
///
/// # Errors
/// 理论上不失败（内置值恒存在）；保留 `Result` 以稳定调用方签名。
pub fn effective_client_id() -> Result<String> {
    if let Ok(id) = std::env::var(CLIENT_ID_ENV) {
        if !id.trim().is_empty() {
            return Ok(id.trim().to_string());
        }
    }
    Ok(CLIENT_ID.to_string())
}

/// 用系统默认浏览器打开 URL（零依赖实现：Windows `start` / macOS `open` / Linux `xdg-open`）。
///
/// # Errors
/// - [`Error::Usage`]：没有可用的打开命令（如无头 Linux）。失败不致命——
///   调用方应继续显示 verification_uri 让用户手动打开。
pub fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        // start 的第一个引号参数是窗口标题，必须给一个空标题占位
        c.args(["/c", "start", "", url]);
        // GUI 程序里 cmd 会闪一个控制台黑窗（浏览器本身照常打开），消除它
        crate::subprocess::no_window(&mut c);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    match cmd.status() {
        Ok(st) if st.success() => Ok(()),
        // 无头环境等：给调用方可降级的错误
        _ => Err(Error::Usage(
            "cannot open a browser automatically; please open the link manually".into(),
        )),
    }
}

/// 发起设备授权，返回设备码信息。
///
/// # Errors
/// - [`Error::Usage`]：client_id 未配置；
/// - [`Error::Io`] 之外的网络错误统一包装为 [`Error::Usage`] 并带 GitHub 返回的说明。
pub fn start_device_flow(client_id: &str) -> Result<DeviceCode> {
    let resp = http_post_form(
        "https://github.com/login/device/code",
        &[("client_id", client_id), ("scope", SCOPE)],
    )?;
    serde_json::from_str(&resp)
        .map_err(|e| Error::Usage(format!("cannot parse the GitHub device-code response: {e}")))
}

/// 设备流授权结果：access token + 可选刷新令牌（OAuth App 开启
/// "过期 token" 选项时返回；access 约 8 小时过期，refresh 约 6 个月）。
#[derive(Debug, Clone)]
pub struct AuthTokens {
    /// 访问令牌（写入 keychain `github` 条目，git 注入用）。
    pub access_token: String,
    /// 刷新令牌（写入 keychain `github-refresh` 条目，失效时自动换新）。
    pub refresh_token: Option<String>,
    /// 颁发的 scope（GitHub 回传；`(none)` 表示零权限令牌——调用方
    /// 应自行展示，core 不做任何 stdout 输出）。
    pub scope: Option<String>,
}

/// 轮询直到用户完成授权，返回 access token 与可选 refresh token。
///
/// 阻塞直到：成功 / 用户拒绝 / 设备码过期 / 网络持续失败。
///
/// # Errors
/// - [`Error::Usage`]：用户拒绝授权、码过期或 GitHub 返回错误。
pub fn poll_device_token(code: &DeviceCode, client_id: &str) -> Result<AuthTokens> {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(code.expires_in.max(60));
    let mut interval = code.interval.max(1);
    loop {
        // 先查 deadline 再 sleep：过期立即返回，不多等一轮 interval
        if std::time::Instant::now() > deadline {
            return Err(Error::Usage(
                "device code expired; please run the command again".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
        let resp = http_post_form(
            "https://github.com/login/oauth/access_token",
            &[
                ("client_id", client_id),
                ("device_code", code.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                // **必须重复声明 scope**：GitHub 不会从发码请求继承——
                // 缺失时颁发的令牌 scope 为空（能识别身份却读不了私有
                // 仓库，"授权成功但同步 403" 的根因，2026-10-30 修复）
                ("scope", SCOPE),
            ],
        )?;
        // 成功：{"access_token":...,"token_type":"bearer","scope":"repo"
        //        [,"refresh_token":...,"expires_in":...]}（过期模式）
        if let Ok(ok) = serde_json::from_str::<TokenOk>(&resp) {
            if let Some(rt) = &ok.refresh_token {
                // 刷新令牌当场入 keychain：8 小时后 access 过期时自动续期
                // 全靠它。写失败必须上抛——静默丢失会让续期在 8 小时后
                // 永远报 "no refresh token stored"，用户只看得到 401
                // （2026-11-03 修复：此前 let _ = 静默吞掉写失败）
                keychain_set_refresh_token(rt)?;
            } else {
                // 过期模式关闭（App 关掉 "Expire user authorization
                // tokens"）时响应无 refresh_token——access 永久有效、刷新
                // 端点直接拒绝该 App（incorrect_client_credentials）。
                // 清掉此前授权残留的旧刷新令牌：它对当前 App 配置已无意义，
                // 残留只会让续期在错误场景里拿一份注定失败的凭据发请求
                // （2026-11-03 实机案例：刷新链断裂的根因即此配置变更）
                let _ = keychain_delete_refresh_token();
            }
            record_token_expiry(&ok)?;
            return Ok(AuthTokens {
                access_token: ok.access_token,
                refresh_token: ok.refresh_token,
                scope: ok.scope,
            });
        }
        // 失败：{"error":"authorization_pending" | "slow_down" | "expired_token" | "access_denied"}
        match serde_json::from_str::<TokenErr>(&resp) {
            Ok(e) => match e.error.as_str() {
                // 轮询臂走完即本轮结束,无需显式 continue
                "authorization_pending" => {}
                "slow_down" => interval += 5,
                "expired_token" => {
                    return Err(Error::Usage(
                        "device code expired; please run the command again".into(),
                    ))
                }
                "access_denied" => {
                    return Err(Error::Usage(
                        "authorization was denied in the browser".into(),
                    ));
                }
                other => return Err(Error::Usage(format!("GitHub rejected the access: {other}"))),
            },
            Err(_) => {
                return Err(Error::Usage(
                    "unrecognized GitHub authorization response".into(),
                ))
            }
        }
    }
}

/// token 端点响应。OAuth App 开启"过期 token"选项后，响应会携带
/// `refresh_token` 与 `expires_in`（未开启则缺省——两种形状都必须能解析）。
#[derive(Debug, Deserialize)]
struct TokenOk {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    /// access 有效秒数（过期模式开启时存在；`record_token_expiry` 据此
    /// 写入过期时刻供启动主动续期判断）。
    #[serde(default)]
    pub expires_in: Option<u64>,
    /// 令牌实际获得的权限（如 "repo"）——空串/缺失 = 零权限令牌。
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenErr {
    error: String,
    /// GitHub 错误响应可选携带的人类可读描述（刷新失败诊断用；
    /// device flow 轮询的旧响应形状没有该字段，缺省 None 兼容）。
    #[serde(default)]
    error_description: Option<String>,
}

/// 用 token 查当前用户 login 名。
///
/// # Errors
/// - [`Error::Usage`]：token 无效或 API 失败（附响应摘要）。
pub fn api_login(token: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct User {
        login: String,
    }
    let resp = api_get(token, "https://api.github.com/user")?;
    let u: User = serde_json::from_str(&resp)
        .map_err(|e| Error::Usage(format!("cannot parse the GitHub user response: {e}")))?;
    Ok(u.login)
}

/// 检测仓库存在性，不存在则创建私有仓库；返回 https 克隆地址。
///
/// # Errors
/// - [`Error::Usage`]：API 失败（无权限/重名冲突等，附响应摘要）。
pub fn api_ensure_repo(token: &str, owner: &str, name: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Repo {
        clone_url: String,
    }
    let url = format!("https://api.github.com/repos/{owner}/{name}");
    // 200 = 已存在，直接复用（第二台设备接入、误删重建都走这里）
    // 先探测:200 = 复用;403 = 令牌无权访问(App 未安装到该仓库/
    // 未包含它)——直接给出精确出路,不再静默落入创建分支绕一圈
    // (2026-11-04 GitHub App 迁移实测:无权时创建 403 报错语焉不详)
    match api_get_raw(token, &url)? {
        Some(resp) if resp.starts_with("__403__") => {
            let body = resp.trim_start_matches("__403__").to_string();
            crate::log::error(
                "github",
                &format!(
                    "no access to {owner}/{name} (HTTP 403): {}",
                    truncate_str(&body, 200)
                ),
            );
            return Err(Error::Usage(format!(
                "no access to {owner}/{name} with this token: {}.\n  Install the GitHub App on this repo (or include it in the app's repository access), then retry",
                truncate_str(&body, 200)
            )));
        }
        Some(resp) => {
            let r: Repo = serde_json::from_str(&resp).map_err(|e| {
                Error::Usage(format!("cannot parse the GitHub repository response: {e}"))
            })?;
            ensure_repo_readme(token, owner, name);
            return Ok(r.clone_url);
        }
        None => {} // 404:仓库不存在 → 走创建分支
    }

    // 404 → 创建私有仓库
    let body = serde_json::json!({
        "name": name,
        "private": true,
        "description": "Makara encrypted vault (auto-created)",
        "auto_init": false,
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Usage(format!("failed to build the HTTP client: {e}")))?;
    let resp = client
        .post("https://api.github.com/user/repos")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        // GitHub API 强制要求 User-Agent
        .header("User-Agent", "Makara-CLI")
        .json(&body)
        .send()
        .map_err(|e| Error::Usage(format!("repository creation request failed: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    if status != 201 {
        // 建仓需要 App 的「Repository creation」权限（write）。未勾选时
        // user token 建仓 403（2026-11-04 实测）——引导去 App 权限设置
        // 勾选后重试；404 则引导检查仓库 owner/name
        let hint = if status == 403 {
            "\n  Enable the \"Repository creation\" permission (read/write) for this GitHub App in its developer settings, then retry."
        } else if status == 404 {
            "\n  Check the repository owner/name."
        } else {
            ""
        };
        return Err(Error::Usage(format!(
            "failed to create the GitHub repository (HTTP {status}): {}{hint}",
            truncate_str(&text, 200)
        )));
    }
    let r: Repo = serde_json::from_str(&text).map_err(|e| {
        Error::Usage(format!(
            "cannot parse the repository creation response: {e}"
        ))
    })?;
    ensure_repo_readme(token, owner, name);
    Ok(r.clone_url)
}

/// 确保同步仓库有说明 README（D39：仓库只有一个加密数据文件，用户
/// 看到容易诧异误删——README 讲清楚它是什么、为什么别删）。
///
/// 已存在（含用户自己写的版本）则尊重不动；缺失则写入；任何失败
/// （无权 / 网络等）只落日志不阻塞——README 是锦上添花，绑定主流程优先。
fn ensure_repo_readme(token: &str, owner: &str, name: &str) {
    let url = format!("https://api.github.com/repos/{owner}/{name}/contents/README.md");
    // Some = 已有（200 内容 / `__403__` 前缀无权访问）→ 尊重现状跳过；
    // None = 404 缺失 → 写入
    match api_get_raw(token, &url) {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(e) => {
            crate::log::error("github", &format!("readme check failed: {e}"));
            return;
        }
    }
    let body = serde_json::json!({
        "message": "docs: add repository README (auto-generated by Makara)",
        "content": base64_encode(repo_readme_markdown().as_bytes()),
    });
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            crate::log::error("github", &format!("readme client build failed: {e}"));
            return;
        }
    };
    let result = client
        .put(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        // GitHub API 强制要求 User-Agent
        .header("User-Agent", "Makara-CLI")
        .json(&body)
        .send();
    match result {
        Ok(resp) if resp.status().is_success() => {
            crate::log::info("github", "repository README ensured");
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().unwrap_or_default();
            crate::log::error(
                "github",
                &format!(
                    "readme write failed (HTTP {status}): {}",
                    truncate_str(&text, 200)
                ),
            );
        }
        Err(e) => {
            crate::log::error("github", &format!("readme write request failed: {e}"));
        }
    }
}

/// 同步仓库 README 的固定内容（中英双语，简短讲清用途与勿删）。
fn repo_readme_markdown() -> String {
    concat!(
        "# Makara 同步仓库\n",
        "\n",
        "这是 [Makara](https://github.com/Haaaatcher/MyApiKey) 的私有同步仓库，由 Makara 自动创建与维护。\n",
        "\n",
        "- `vault.mkv` 是 AES-256-GCM 加密的凭证库，由 Makara 自动读写，请勿手动编辑\n",
        "- 请勿删除本仓库——删除后设备间将无法同步；本地数据不受影响，重新绑定即可恢复\n",
        "\n",
        "This is Makara's private encrypted vault for cross-device sync. ",
        "Do not edit `vault.mkv` manually, and do not delete this repository.\n",
    )
    .to_string()
}

// ---------------------------------------------------------------------------
// 凭据库中的 GitHub token
// ---------------------------------------------------------------------------

/// 读取已存的 GitHub token（没有则 `None`）。
///
/// # Errors
/// - [`Error::Keyring`]：凭据库本身异常（读不到条目不算异常，返回 `None`）。
pub fn keychain_token() -> Result<Option<String>> {
    let Ok(entry) = keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_ACCOUNT) else {
        // 凭据库打不开 = 记不住,降级手动输入
        return Ok(None);
    };
    match entry.get_password() {
        Ok(p) => Ok(Some(p)),
        // 没记住与后端异常同语义:一律 None,不因凭据库失败阻塞解锁路径
        Err(_) => Ok(None),
    }
}

/// 保存 GitHub token 到凭据库。
///
/// # Errors
/// - [`Error::Keyring`]：写入失败。
pub fn keychain_set_token(token: &str) -> Result<()> {
    let entry = keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open the credential store entry: {e}")))?;
    entry
        .set_password(token)
        .map_err(|e| Error::Keyring(format!("failed to write to the credential store: {e}")))
}

/// 读取记录的 access 过期时刻（RFC3339）：开启过期模式的授权/续期成功
/// 时写入；关闭过期的授权没有该记录（`None` = 无从判断，交给同步路径
/// 的 401 被动续期兜底）。
///
/// # Errors
/// - [`Error::Keyring`]：凭据库本身异常（读不到条目不算异常，返回 `None`）。
pub fn keychain_token_expires() -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    let Some(raw) = keychain_aux_get(KEYCHAIN_EXPIRES_ACCOUNT) else {
        return Ok(None);
    };
    Ok(chrono::DateTime::parse_from_rfc3339(&raw)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc)))
}

/// 记录 access 过期时刻。
///
/// # Errors
/// - [`Error::Keyring`]：写入失败。
pub fn keychain_set_token_expires(at: chrono::DateTime<chrono::Utc>) -> Result<()> {
    keychain_aux_set(KEYCHAIN_EXPIRES_ACCOUNT, &at.to_rfc3339())
}

/// 清除 access 过期时刻记录（宽松：失败静默——残留条目只会让启动续期
/// 多尝试一次注定失败的刷新，无害）。
fn keychain_delete_token_expires() {
    let _ = keychain_aux_delete(KEYCHAIN_EXPIRES_ACCOUNT);
}

/// 凭据库辅助条目（token 主条目之外的少量元数据）的统一读取。
/// 打不开 / 读不到 / 后端异常一律 `None`（元数据缺失只降级，不失败）。
fn keychain_aux_get(account: &str) -> Option<String> {
    let entry = keyring::Entry::new(crate::keychain::SERVICE, account).ok()?;
    entry.get_password().ok()
}

fn keychain_aux_set(account: &str, value: &str) -> Result<()> {
    let entry = keyring::Entry::new(crate::keychain::SERVICE, account)
        .map_err(|e| Error::Keyring(format!("failed to open the credential store entry: {e}")))?;
    entry
        .set_password(value)
        .map_err(|e| Error::Keyring(format!("failed to write to the credential store: {e}")))
}

fn keychain_aux_delete(account: &str) -> Result<bool> {
    let entry = keyring::Entry::new(crate::keychain::SERVICE, account)
        .map_err(|e| Error::Keyring(format!("failed to open the credential store entry: {e}")))?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(Error::Keyring(format!(
            "failed to delete the credential store entry: {e}"
        ))),
    }
}

/// 授权/续期成功后记录 access 过期时刻：开启过期模式（`expires_in`
/// 存在）→ `now + expires_in`；关闭过期 → 清条目（配合 2026-11-03 的
/// 重新授权残留清理）。
fn record_token_expiry(ok: &TokenOk) -> Result<()> {
    let Some(secs) = ok.expires_in else {
        keychain_delete_token_expires();
        return Ok(());
    };
    // u64→i64 防回绕:GitHub 实际回 28800(8h),异常巨值按"永不过期"处理
    let secs = i64::try_from(secs).unwrap_or(i64::MAX);
    keychain_set_token_expires(chrono::Utc::now() + chrono::Duration::seconds(secs))
}

/// 判断 access 是否临近/已过期（10 分钟提前量：启动时刻恰在过期边缘
/// 也来得及在用户第一次同步前换新）。
fn token_needs_refresh(
    now: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> bool {
    now >= expires_at - chrono::Duration::seconds(600)
}

/// 清除 GitHub token（access 与 refresh 两个条目一并删除）。
///
/// 只删 access 会留下 refresh 残留——自动续期（`refresh_access_token`）
/// 可拿它换回有效令牌，"清除授权/解绑"对用户承诺的干净状态就被悄悄
/// 破坏（2026-10-31 修复）。
///
/// 返回是否清到了任一条目。
///
/// # Errors
/// - [`Error::Keyring`]：条目存在但删除失败。
pub fn keychain_delete_token() -> Result<bool> {
    let mut deleted = false;
    for account in [
        KEYCHAIN_ACCOUNT,
        KEYCHAIN_REFRESH_ACCOUNT,
        KEYCHAIN_EXPIRES_ACCOUNT,
        KEYCHAIN_SECRET_ACCOUNT,
    ] {
        let entry = keyring::Entry::new(crate::keychain::SERVICE, account).map_err(|e| {
            Error::Keyring(format!("failed to open the credential store entry: {e}"))
        })?;
        match entry.delete_credential() {
            Ok(()) => deleted = true,
            Err(keyring::Error::NoEntry) => {}
            Err(e) => {
                return Err(Error::Keyring(format!(
                    "failed to delete the credential store entry: {e}"
                )))
            }
        }
    }
    Ok(deleted)
}

/// 读取 GitHub App client secret：环境变量 [`CLIENT_SECRET_ENV`] 优先，
/// 其次凭据库条目（GUI/CLI 保存）。
///
/// # Returns
/// - `Ok(Some(secret))`：有 secret（refresh 请求携带）；
/// - `Ok(None)`：未保存——刷新请求退化为仅 client_id（device flow 豁免
///   路径；对 MakaraApp 实测被拒，见 [`KEYCHAIN_SECRET_ACCOUNT]` 注释）。
///
/// # Errors
/// - [`Error::Keyring`]：凭据库本身异常（读不到条目不算异常，返回 `None`）。
pub fn client_secret() -> Result<Option<String>> {
    // 优先级:env 覆盖 > 设置页/keychain 粘贴 > 内嵌默认(大众分发)
    if let Ok(s) = std::env::var(CLIENT_SECRET_ENV) {
        if !s.trim().is_empty() {
            return Ok(Some(s.trim().to_string()));
        }
    }
    let Ok(entry) = keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_SECRET_ACCOUNT) else {
        return Ok(embedded_client_secret());
    };
    Ok(entry
        .get_password()
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(embedded_client_secret))
}

/// 清除已存的 client secret。
///
/// # Returns
/// - `Ok(true)`：清掉了条目；`Ok(false)`：本来就没有。
///
/// # Errors
/// - [`Error::Keyring`]：条目存在但删除失败。
pub fn keychain_delete_client_secret() -> Result<bool> {
    let entry = keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_SECRET_ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open the credential store entry: {e}")))?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(Error::Keyring(format!(
            "failed to delete the credential store entry: {e}"
        ))),
    }
}

/// 读取已存的刷新令牌（没有则 `None`）。
///
/// # Returns
/// - `Ok(Some(token))`：有刷新令牌；
/// - `Ok(None)`：没记住 / 凭据库打不开或读取异常。
///
/// # Errors
/// 不失败：三种缺失/异常情形统一降级为 `Ok(None)`，调用方以
/// "no refresh token stored" 指引重新授权。保留 `Result` 与同族函数签名一致。
pub fn keychain_refresh_token() -> Result<Option<String>> {
    let Ok(entry) = keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_REFRESH_ACCOUNT) else {
        return Ok(None);
    };
    match entry.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(_) => Ok(None),
    }
}

/// 保存刷新令牌。
///
/// # Errors
/// - [`Error::Keyring`]：写入失败。
pub fn keychain_set_refresh_token(token: &str) -> Result<()> {
    let entry = keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_REFRESH_ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open the credential store entry: {e}")))?;
    entry
        .set_password(token)
        .map_err(|e| Error::Keyring(format!("failed to write to the credential store: {e}")))
}

/// 清除刷新令牌（forget 时与 access token 一起清）。
///
/// # Errors
/// - [`Error::Keyring`]：条目存在但删除失败。
pub fn keychain_delete_refresh_token() -> Result<bool> {
    let entry = keyring::Entry::new(crate::keychain::SERVICE, KEYCHAIN_REFRESH_ACCOUNT)
        .map_err(|e| Error::Keyring(format!("failed to open the credential store entry: {e}")))?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(Error::Keyring(format!(
            "failed to delete the credential store entry: {e}"
        ))),
    }
}

/// 用刷新令牌换新的 access token（GitHub 会**轮换** refresh token——
/// 响应携带新的 refresh token，必须一并覆盖存储，旧刷新令牌作废）。
///
/// # Errors
/// - 无刷新令牌 / 刷新失败（`invalid_grant` 等，通常意味着必须重走
///   设备授权流）。
pub fn refresh_access_token(client_id: &str) -> Result<AuthTokens> {
    // 刷新串行锁：启动主动续期与 401 被动续期并发时，同一 refresh
    // token 被双双提交，后到者必因轮换被拒（2026-11-03 加固）。锁内
    // 每次重新读 keychain——前一个续期写回的新令牌能被后一个直接使用
    static REFRESH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = REFRESH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(rt) = keychain_refresh_token()? else {
        let msg = "no refresh token stored; run `makara init --github` to authorize";
        crate::log::error("github", msg);
        return Err(Error::Usage(msg.into()));
    };
    // client_secret:实测（2026-11-06 全新 device flow 令牌立即刷新被拒）
    // 刷新端点必须携带——文档的 device flow 豁免与行为不符。有则带上；
    // 未保存时仅带 client_id（兼容未来 GitHub 修正文档的情形）
    // grant_type 必须是简单值 "refresh_token"——GitHub App 的刷新端点
    // 不认标准 OAuth 的 urn:ietf:params:oauth:grant-type:refresh_token
    // 长形态(2026-11-08 实测:urn 形态报 unsupported_grant_type,
    // 简单值报 bad_refresh_token = 格式已被接受,仅令牌校验不通过)
    let mut form = vec![
        ("client_id", client_id),
        ("grant_type", "refresh_token"),
        ("refresh_token", rt.as_str()),
    ];
    let secret = client_secret()?;
    if let Some(sec) = &secret {
        form.push(("client_secret", sec.as_str()));
        // 携带 secret 时 GitHub 同时校验 redirect_uri(必须与 App 注册
        // 的 Callback URL 一致,实测缺它即 redirect_uri_mismatch)
        form.push(("redirect_uri", REFRESH_REDIRECT_URI));
    } else {
        crate::log::debug(
            "github",
            "no client secret stored; refresh request sent with client_id only",
        );
    }
    let resp = http_post_form("https://github.com/login/oauth/access_token", &form)?;
    let ok: TokenOk = if let Ok(v) = serde_json::from_str(&resp) {
        v
    } else {
        // 非 2xx/错误形状（缺 access_token）：GitHub 的错误 JSON 里
        // 有具体原因（invalid_grant = 刷新令牌失效/被轮换、
        // incorrect_client = client_id 不匹配），必须透出——只报
        // "missing field access_token" 用户无法对症（2026-11-03
        // 实机案例：刷新链断裂时用户只看到晦涩的 serde 报错）
        let detail = match serde_json::from_str::<TokenErr>(&resp) {
            Ok(e) => match &e.error_description {
                Some(d) => format!("{} ({})", e.error, d),
                None => e.error,
            },
            Err(_) => resp.chars().take(120).collect(),
        };
        let msg = format!("token refresh refused: {detail}");
        crate::log::error("github", &msg);
        return Err(Error::Usage(format!(
            "GitHub refused the token refresh: {detail}"
        )));
    };
    crate::log::info(
        "github",
        &format!(
            "access token refreshed (expires_in = {}s)",
            ok.expires_in.unwrap_or(0)
        ),
    );
    record_token_expiry(&ok)?;
    // 新 access token 必须落库:GitHub 每次刷新都轮换 access token——
    // 响应里的新令牌若不入 keychain,重试时构造的 GitHubStore 仍带旧
    // (已过期)令牌,表现为"刷新成功但紧接着的请求依旧 401"
    // (2026-11-08 日志实测:refreshed 后紧跟 GET 401)
    keychain_set_token(&ok.access_token)?;
    if let Some(new_rt) = &ok.refresh_token {
        keychain_set_refresh_token(new_rt)?;
    }
    Ok(AuthTokens {
        access_token: ok.access_token,
        refresh_token: ok.refresh_token,
        scope: ok.scope,
    })
}

// 自动续期布尔封装（原 try_refresh_and_store）已删除（2026-11-03）：
// 它把失败原因吞成 false，调用方只能维持原 401，用户永远不知道续期
// 为何没救场（无刷新令牌？invalid_grant？）。调用方直接用
// refresh_access_token，把失败原因并入错误展示。

/// 应用启动时的主动续期（2026-11-03 需求）：access 已过期或 10 分钟内
/// 将过期，且本机有刷新令牌 → 静默换新，用户启动应用即恢复可用。
///
/// - `Ok(true)`：已续期；
/// - `Ok(false)`：无需续期或无从判断（无过期记录、未授权等——交给
///   同步路径的 401 被动续期兜底）；
/// - `Err`：续期尝试失败（刷新令牌同样失效等），调用方应提示重新授权。
///
/// # Errors
/// - 凭据库读写失败 / GitHub 拒绝刷新（原因透出）。
pub fn try_refresh_if_expired(client_id: &str) -> Result<bool> {
    let Some(expires_at) = keychain_token_expires()? else {
        return Ok(false);
    };
    if keychain_token()?.is_none() {
        return Ok(false);
    }
    if !token_needs_refresh(chrono::Utc::now(), expires_at) {
        return Ok(false);
    }
    refresh_access_token(client_id)?;
    Ok(true)
}

/// 授权/安装诊断视图（2026-11-04 分步引导）：GUI 据此渲染「哪一步未完成」。
#[derive(Debug, Clone, Serialize)]
pub struct GhAuthDiag {
    /// access token 对 GET /user 有效（device flow 授权已完成）。
    pub token_valid: bool,
    /// 本 App 已安装到用户账号（未安装 = user token 对仓库无权）。
    pub installed: bool,
    /// 未安装时的安装页 URL（引导直达；已安装为 None）。
    pub install_url: Option<String>,
    /// 绑定仓库可访问（安装范围含它且 Contents 可写）。`None` = 未绑定远端。
    pub repo_accessible: Option<bool>,
}

#[derive(Deserialize)]
struct InstallationsResp {
    #[serde(default)]
    installations: Vec<Installation>,
}

#[derive(Deserialize)]
struct Installation {
    #[serde(default)]
    app_slug: Option<String>,
    #[serde(default)]
    app: Option<InstallAppRef>,
}

#[derive(Deserialize)]
struct InstallAppRef {
    #[serde(default)]
    slug: Option<String>,
}

/// 诊断授权与安装状态（2026-11-04 分步引导）。
/// 依次检查 ①令牌有效性（device flow 授权完成）②**本 App** 安装
/// （user-to-server 仓库访问的前提，官方文档：app 只能访问 "resources
/// in an account where it is installed"；必须是 slug 匹配的本 App——
/// 列表里出现其他 App 不算，用户可能装过无关 App，误判 true 会让
/// 引导条被异步诊断撤下，2026-11-04 实测）③绑定仓库可访问。
/// `repo` = 绑定仓库 owner/name（未绑定传 `None`，跳过 ③）。
///
/// 网络失败与令牌失效严格区分：断网时 GET /user 同样失败，把它判成
/// 「令牌失效」会诱导用户做一次毫无意义的重新授权（2026-11-05 审查
/// 修复）——网络类未知一律返回 `Err`，调用方保留既有诊断。
///
/// # Errors
/// - 网络请求失败 / 无法判定令牌状态（诊断失败不等于授权失效，
///   调用方按需展示）。
pub fn diagnose_github_auth(token: &str, repo: Option<(&str, &str)>) -> Result<GhAuthDiag> {
    // ① token 有效性：None = 网络类未知（不能断定失效），上抛交调用方
    // 保留旧诊断；401 才是确证失效
    match token_is_valid(token) {
        Some(true) => {}
        Some(false) => {
            return Ok(GhAuthDiag {
                token_valid: false,
                installed: false,
                install_url: Some(app_install_url()),
                repo_accessible: None,
            });
        }
        None => {
            return Err(Error::Usage(
                "cannot reach GitHub to verify the token (network); diagnosis skipped".into(),
            ));
        }
    }
    // ② 安装:安装列表含本 App(slug 匹配)= 已安装
    let resp = api_get(token, "https://api.github.com/user/installations")?;
    let inst: InstallationsResp = serde_json::from_str(&resp)
        .map_err(|e| Error::Usage(format!("cannot parse the installations response: {e}")))?;
    let installed = installations_include_ours(&inst, &app_install_slug());
    let mut diag = GhAuthDiag {
        token_valid: true,
        installed,
        install_url: if installed {
            None
        } else {
            Some(app_install_url())
        },
        repo_accessible: None,
    };
    // ③ 绑定仓库可达：Err = 网络类未知(None);非 2xx(404/403) =
    // 令牌看不见该仓库 = 不可达(false)。5xx 等瞬时错误也被归入
    // false——下一次诊断自会修正，保守显示引导条无害
    if let (true, Some((owner, name))) = (diag.installed, repo) {
        let url = format!("https://api.github.com/repos/{owner}/{name}");
        diag.repo_accessible = Some(api_get_raw(token, &url).is_ok_and(|r| r.is_some()));
    }
    crate::log::debug(
        "github",
        &format!(
            "diagnosis: token_valid={} installed={} repo_accessible={:?}",
            diag.token_valid, diag.installed, diag.repo_accessible
        ),
    );
    Ok(diag)
}

/// 安装列表中是否存在**本 App**（slug 匹配，大小写不敏感）的条目。
/// `/user/installations` 返回的是该用户授权过的各 App 安装,列表非空
/// 不代表装的是本 App——必须逐条比对 slug。
fn installations_include_ours(inst: &InstallationsResp, ours: &str) -> bool {
    inst.installations.iter().any(|i| {
        i.app_slug
            .as_deref()
            .or_else(|| i.app.as_ref().and_then(|a| a.slug.as_deref()))
            .is_some_and(|s| s.eq_ignore_ascii_case(ours))
    })
}

/// 探测 token 当前是否有效（GET /user，只关心 HTTP 状态码）。
/// - `Some(true)`：有效；
/// - `Some(false)`：401，token 已失效（授权被撤销 / 过期）；
/// - `None`：网络等未知错误——不能断定失效，调用方应保留 token 不动作。
fn token_is_valid(token: &str) -> Option<bool> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "Makara")
        .send()
        .ok()?;
    match resp.status().as_u16() {
        200 => Some(true),
        401 => Some(false),
        _ => None,
    }
}

/// 取"确认仍有效"的 GitHub token。
///
/// 与 [`keychain_token`] 的差别：读出后先验证。已存的 token 失效（401）时
/// **顺手清除条目**并返回 `None`，让调用方回落到重新设备授权——否则每次
/// 都带着坏 token 撞 401，且 git 对私有仓库只报语焉不详的
/// "Repository not found"。网络异常等无法判定的情况按"有效"处理，不误删。
///
/// # Errors
/// - [`Error::Keyring`]：凭据库本身异常。
pub fn valid_keychain_token() -> Result<Option<String>> {
    let Some(token) = keychain_token()? else {
        return Ok(None);
    };
    match token_is_valid(&token) {
        Some(false) => {
            let _ = keychain_delete_token();
            Ok(None)
        }
        // 有效，或网络原因无法判定：交给调用方使用
        _ => Ok(Some(token)),
    }
}

// ---------------------------------------------------------------------------
// 内部小工具
// ---------------------------------------------------------------------------

/// OAuth 端点的表单 POST（GitHub 要求 Accept: application/json 才返回 JSON）。
fn http_post_form(url: &str, form: &[(&str, &str)]) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Usage(format!("failed to build the HTTP client: {e}")))?;
    let resp = client
        .post(url)
        .header("Accept", "application/json")
        .form(form)
        .send()
        .map_err(|e| Error::Usage(format!("request to {url} failed: {e}")))?;
    Ok(resp.text().unwrap_or_default())
}

/// 带 token 的 API GET，非 2xx 不当错误:返回 `Some(响应体)` 供调用方
/// 按状态码区分处理（如 ensure_repo 区分 403 无权与 404 不存在）。
/// 形态约定：2xx → `Some(体)`；403 → `Some("__403__" + 体)`（令牌无权，
/// 体里带精确原因）；其余非 2xx（404 等）→ `None`（仓库不存在）。
/// 网络失败仍为 `Err`——调用方须区分「无权」与「未知」。
fn api_get_raw(token: &str, url: &str) -> Result<Option<String>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Usage(format!("failed to build the HTTP client: {e}")))?;
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "Makara-CLI")
        .send()
        .map_err(|e| Error::Usage(format!("request to {url} failed: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    if (200..300).contains(&status) {
        Ok(Some(text))
    } else if status == 403 {
        // 403 = 令牌无权(App 未安装到该仓库/未包含它):体里带精确原因
        Ok(Some(format!("__403__{text}")))
    } else {
        Ok(None) // 404 等 = 仓库不存在,调用方走创建分支
    }
}

/// 带 token 的 API GET，非 2xx 返回 Err（错误体带摘要）。
///
/// # Errors
/// - 网络请求失败；非 2xx 响应（错误体截断至 200 字符入消息）。
fn api_get(token: &str, url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Usage(format!("failed to build the HTTP client: {e}")))?;
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "Makara-CLI")
        .send()
        .map_err(|e| Error::Usage(format!("request to {url} failed: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(Error::Usage(format!(
            "GitHub API returned HTTP {status}: {}",
            truncate_str(&text, 200)
        )));
    }
    Ok(text)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// 标准 base64 编码（含填充，RFC 4648）。
///
/// 自实现而非引入 base64 crate：全项目只为这一处 26 行逻辑，
/// 不值得新增一个依赖（AGENTS.md：新依赖需征得同意）。
pub fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 标准 base64 解码（含填充；容忍换行折行，GitHub API 返回的
/// content 字段按 60 字符折行）。非法字符/长度报错。
///
/// 自实现理由同 [`base64_encode`]：不引入 base64 crate。
///
/// # Panics
/// 不 panic：输入字节按 6-bit 表值重组，掩码后必落在 u8 域——
/// `try_from` 的 `expect` 只表达该不可失败不变式（见函数体内的注释）。
///
/// # Errors
/// 非法 base64（非表字符、长度不对齐、padding 位置错误）。
pub fn base64_decode(s: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        if chunk.len() == 1 {
            return Err(Error::Usage("invalid base64: dangling single char".into()));
        }
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = val(c).ok_or_else(|| {
                Error::Usage(format!("invalid base64 character: {:?}", c as char))
            })?;
            n |= u32::from(v) << (18 - 6 * i);
        }
        // n 由至多 3 个 6-bit 段拼成(≤18 bit),掩码后必在 u8 域——
        // try_from 表达不可失败的不变式
        out.push(u8::try_from((n >> 16) & 0xFF).expect("18-bit 值低 8 位必在 u8 范围"));
        if chunk.len() > 2 {
            out.push(u8::try_from((n >> 8) & 0xFF).expect("同上"));
        }
        if chunk.len() > 3 {
            out.push(u8::try_from(n & 0xFF).expect("同上"));
        }
    }
    Ok(out)
}

/// 源码仓库的 Releases API（版本检查用，公开端点无需认证）。
pub const SOURCE_RELEASES_API: &str =
    "https://api.github.com/repos/Haaaatcher/MyApiKey/releases/latest";

/// 查询源码仓库的最新 Release tag（如 `v0.2.0`）。
///
/// 匿名调用 GitHub API 的限额是 60 次/小时/IP——频繁检查很快耗尽
/// （2026-11-10 用户白天实测 403 rate limit）；传入令牌则升到
/// 5000/小时（用户的授权令牌本来就在凭据库里）。
///
/// 返回 `Ok(None)` = 还没有发布过任何 Release（404）。CLI 的 `makara version`
/// 用它提示新版本；查询失败由调用方降级为"无法检查"，不阻塞命令本身。
///
/// # Errors
/// 网络 / 超时（15 秒）/ 响应解析失败。
pub fn latest_release_tag(token: Option<&str>) -> Result<Option<String>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Usage(format!("failed to build the HTTP client: {e}")))?;
    let mut req = client
        .get(SOURCE_RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "Makara-CLI");
    // 有令牌就带上：匿名限额对版本检查太紧
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .map_err(|e| Error::Usage(format!("release check request failed: {e}")))?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    let text = resp
        .error_for_status()
        .map_err(|e| Error::Usage(e.to_string()))?
        .text()
        .map_err(|e| Error::Usage(format!("failed to read the release info: {e}")))?;
    let tag = serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|e| Error::Usage(format!("failed to parse the release info: {e}")))?
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(tag)
}

/// 下载 Release 附件（直链）到本地文件：逐块落盘并把进度回调出去
/// （`(已下载字节, 总字节)`，总长未知时为 0）——CLI 据此打印行内
/// 百分比，core 不做任何输出。带令牌时私有仓库也可下载。
///
/// # Errors
/// 网络 / 超时（120 秒总量，包体积远小于 API 响应）/ 写盘失败。
pub fn download_asset(
    token: Option<&str>,
    url: &str,
    dest: &std::path::Path,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<()> {
    use std::io::{Read as _, Write};
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| Error::Usage(format!("failed to build the HTTP client: {e}")))?;
    let mut req = client.get(url).header("User-Agent", "Makara-CLI");
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let mut resp = req
        .send()
        .map_err(|e| Error::Usage(format!("download failed: {e}")))?
        .error_for_status()
        .map_err(|e| Error::Usage(format!("download failed: {e}")))?;
    let total = resp.content_length();
    let mut file = std::io::BufWriter::new(
        std::fs::File::create(dest)
            .map_err(|e| Error::Usage(format!("cannot create {}: {e}", dest.display())))?,
    );
    // blocking Response 自带 io::Read 实现——固定块循环读取即可逐块
    // 回调进度，无需 reqwest 的 `stream` feature（feature 开关同样是
    // 依赖面变化，能不用就不用）。16KB 块对进度回调粒度足够平滑。
    let mut done: u64 = 0;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| Error::Usage(format!("download interrupted: {e}")))?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n])?;
        done += n as u64;
        on_progress(done, total);
    }
    file.flush()
        .map_err(|e| Error::Usage(format!("cannot write the downloaded file: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! 纯函数测试：base64 向量（RFC 4648）、设备码响应解析。
    //! 真实 OAuth 流程无法在单测中覆盖，由手动冒烟验证。

    use super::*;

    #[test]
    fn base64_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// 仓库 README 讲清三件事：这是什么软件、数据文件是什么、为什么
    /// 别删（D39 防误删的核心信息，缺一不可）。
    #[test]
    fn repo_readme_covers_key_facts() {
        let md = repo_readme_markdown();
        assert!(md.contains("Makara"), "README 必须点明软件名");
        assert!(md.contains("vault.mkv"), "README 必须说明数据文件");
        assert!(
            md.contains("https://github.com/Haaaatcher/MyApiKey"),
            "README 必须链接项目主页"
        );
        assert!(md.contains("请勿删除"), "README 必须明示勿删");
    }

    /// InstallationsResp 构造辅助（顶层 app_slug 形态）。
    fn inst(slug: Option<&str>) -> InstallationsResp {
        InstallationsResp {
            installations: vec![Installation {
                app_slug: slug.map(str::to_string),
                app: None,
            }],
        }
    }

    #[test]
    fn installations_include_ours_matches_only_our_slug() {
        // 本 App 在列表(两种响应形态:顶层 app_slug / 嵌套 app.slug)
        assert!(installations_include_ours(
            &inst(Some("makaraapp")),
            "makaraapp"
        ));
        let nested = InstallationsResp {
            installations: vec![Installation {
                app_slug: None,
                app: Some(InstallAppRef {
                    slug: Some("makaraapp".into()),
                }),
            }],
        };
        assert!(installations_include_ours(&nested, "makaraapp"));
        // slug 比对大小写不敏感(GitHub slug 实际全小写,防御未来变更)
        assert!(installations_include_ours(
            &inst(Some("MakaraApp")),
            "makaraapp"
        ));
        // 其他 App 在列表 ≠ 本 App 已安装(2026-11-04 误判实测回归)
        assert!(!installations_include_ours(
            &inst(Some("myapikey")),
            "makaraapp"
        ));
        // 空列表 / 缺 slug 字段
        assert!(!installations_include_ours(
            &InstallationsResp {
                installations: vec![]
            },
            "makaraapp"
        ));
        assert!(!installations_include_ours(&inst(None), "makaraapp"));
    }

    #[test]
    fn device_code_response_parses() {
        let raw = r#"{
            "device_code": "dc_abc",
            "user_code": "AB12-CD34",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        }"#;
        let dc: DeviceCode = serde_json::from_str(raw).expect("解析失败");
        assert_eq!(dc.user_code, "AB12-CD34");
        assert_eq!(dc.interval, 5);
    }

    #[test]
    fn token_error_response_parses() {
        let raw = r#"{"error":"authorization_pending"}"#;
        let e: TokenErr = serde_json::from_str(raw).expect("解析失败");
        assert_eq!(e.error, "authorization_pending");
    }

    #[test]
    fn token_error_with_description_parses() {
        let raw = r#"{"error":"invalid_grant","error_description":"expired"}"#;
        let e: TokenErr = serde_json::from_str(raw).expect("解析失败");
        assert_eq!(e.error, "invalid_grant");
        assert_eq!(e.error_description.as_deref(), Some("expired"));
    }

    #[test]
    fn token_needs_refresh_boundaries() {
        use chrono::Duration;
        let now = chrono::Utc::now();
        // 还有 1 小时 → 未到提前量,不刷
        assert!(!token_needs_refresh(now, now + Duration::hours(1)));
        // 恰好剩 10 分钟 → 达到提前量边界,刷
        assert!(token_needs_refresh(now, now + Duration::seconds(600)));
        // 已过期 → 刷
        assert!(token_needs_refresh(now, now - Duration::hours(1)));
    }

    #[test]
    fn refresh_response_parses_with_expiry() {
        // 开启过期模式的刷新响应:access + 轮换的 refresh + expires_in
        let raw = r#"{
            "access_token": "gho_new",
            "refresh_token": "ghr_new",
            "expires_in": 28800,
            "token_type": "bearer",
            "scope": "repo"
        }"#;
        let ok: TokenOk = serde_json::from_str(raw).expect("解析失败");
        assert_eq!(ok.expires_in, Some(28800));
        assert_eq!(ok.refresh_token.as_deref(), Some("ghr_new"));
    }
}
