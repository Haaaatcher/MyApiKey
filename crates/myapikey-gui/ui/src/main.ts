/**
 * GUI 主体：视图路由（初始化 / 解锁 / 主界面 / 设置）与全部交互。
 *
 * 语言规范（AGENTS.md）：面向用户的文案一律英文。
 * 界面层不包含任何加密/同步逻辑——全部经 bridge 调后端 command。
 */

import { invoke, onAuthInvalid, onLocked, ProgressChannel, saveDialog } from "./bridge";

// zxcvbn 懒加载实例（模块级缓存，首次评强度时才拉取）：语料 + 匹配器
// 配置一次成型。动态 import 把 ~300KB 语料拆出主 chunk——改密码抽屉
// 是低频场景，不该拖累首屏（2026-11-05 审查可选 B）。
// 纯 zxcvbn 标准（2026-11-04 用户定调），不注入本地语料——拼音组合
// 等 zxcvbn 覆盖不到的模式按其原判输出，不做本地化加权。
interface ZxcvbnChecker {
  check(pw: string): { score: number };
}
let zxcvbnChecker: ZxcvbnChecker | null = null;
async function ensureZxcvbnChecker(): Promise<ZxcvbnChecker> {
  if (zxcvbnChecker === null) {
    const [{ ZxcvbnFactory }, { dictionary }] = await Promise.all([
      import("@zxcvbn-ts/core"),
      import("@zxcvbn-ts/language-common"),
    ]);
    zxcvbnChecker = new ZxcvbnFactory({ dictionary: { ...dictionary } });
  }
  return zxcvbnChecker;
}

// ---------------------------------------------------------------------------
// 与 Rust 侧对应的数据形状
// ---------------------------------------------------------------------------

/** GitHub 授权/安装诊断(后端 diagnose_github,2026-11-04 分步引导)。 */
interface GhDiag {
  /** 凭据库有 GitHub 令牌(device flow 授权完成过)。 */
  authorized: boolean;
  /** 令牌对 GET /user 有效(未过期/未被撤销)。 */
  tokenValid: boolean;
  /** GitHub App 已安装(未安装 = 令牌对仓库无权)。 */
  installed: boolean;
  /** 未安装时的安装页 URL(引导直达)。 */
  installUrl: string | null;
  /** 已绑定远端仓库(单机模式不引导)。 */
  remoteBound: boolean;
  /** 绑定仓库可访问;null = 未绑定或未安装,无从判断。 */
  repoAccessible: boolean | null;
}

interface CheckView {
  status: string;
  label: string;
  http: number | null;
}

/** 应用内更新检查结果（Rust update::UpdateView 的前端镜像）。 */
interface UpdateView {
  current_version: string;
  available: boolean;
  version: string | null;
  notes: string | null;
  breaking: boolean;
}


interface RecordView {
  id: string;
  name: string;
  protocol: string;
  base_url: string;
  masked_key: string;
  models: string[];
  notes: string | null;
  updated_at: string;
  check: CheckView | null;
}

interface GuiState {
  initialized: boolean;
  unlocked: boolean;
}

interface SyncView {
  pulled: boolean;
  merged: boolean;
  pushed: boolean;
  push_error: string | null;
  alive: number;
}

interface StatusView {
  remote: string | null;
  pending: boolean;
  last_sync: string | null;
  alive: number;
}

interface SettingsView {
  remote: string | null;
  tombstone_days: number;
  auto_push: boolean;
  auto_lock_minutes: number;
  /** 文件日志级别(off/error/info/debug)——设置页可控,即时生效。 */
  log_level: string;
}

type InitStage =
  | { stage: "done"; joined: boolean; records: number; refresh?: boolean }
  | { stage: "device_code"; user_code: string; verification_uri: string }
  | { stage: "probe_done"; has_data: boolean; repo: string };

interface RecordInput {
  name: string;
  protocol: string;
  base_url: string;
  api_key: string;
  models: string[] | null;
  notes: string | null;
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

const $app = document.getElementById("app")!;

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  cls?: string,
  text?: string | Node,
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (typeof text === "string") e.textContent = text;
  else if (text) e.append(text);
  return e;
}

/**
 * 内联 SVG 图标（feather 风格：24 viewBox、2px 描边、颜色随 currentColor）。
 * 零 npm 依赖约束下的图标方案——比文本按钮更省空间、更符合凭证管理器的
 * 行业惯例（1Password / Bitwarden 的行内操作均为图标）。
 * 复制自 Feather Icons（MIT）的路径并做了核对。
 */
const ICONS: Record<string, string> = {
  key: '<path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4"/>',
  copy: '<rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>',
  shield: '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/><polyline points="9 11.5 11 13.5 15 9.5"/>',
  edit: '<path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/>',
  trash: '<polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>',
  eye: '<path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/><circle cx="12" cy="12" r="3"/>',
  eyeOff: '<path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19m-6.72-1.07a3 3 0 1 1-4.24-4.24"/><line x1="1" y1="1" x2="23" y2="23"/>',
  plus: '<line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/>',
  sync: '<polyline points="23 4 23 10 17 10"/><polyline points="1 20 1 14 7 14"/><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"/>',
  lock: '<rect x="3" y="11" width="18" height="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>',
  gear: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"/>',
  inbox: '<polyline points="22 12 16 12 14 15 10 15 8 12 2 12"/><path d="M5.45 5.11L2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/>',
  search: '<circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/>',
  globe: '<circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/>',
  back: '<line x1="19" y1="12" x2="5" y2="12"/><polyline points="12 19 5 12 12 5"/>',
  chevron: '<polyline points="6 9 12 15 18 9"/>',
};

/** 与 core::store::is_valid_http_url 同规则的 TS 版（前端即时反馈）。
 * 只拒明显非法（缺 scheme / 空主机 / 含空白），不做归一化。
 */
function isValidHttpUrl(s: string): boolean {
  const t = s.trim();
  const rest = t.startsWith("https://") ? t.slice(8) : t.startsWith("http://") ? t.slice(7) : null;
  if (rest === null) return false;
  if (/\s/.test(t)) return false;
  const hostEnd = rest.search(/[/?#:]/);
  const host = hostEnd < 0 ? rest : rest.slice(0, hostEnd);
  return host.length > 0;
}

/** 后端校验状态英文标签 → 双语展示（valid→有效 等）。未知标签原样，
 * 兼容后端将来新增的状态。 */
function checkLabel(raw: string): string {
  const map: Record<string, string> = {
    valid: t("checkValid"),
    invalid: t("checkInvalid"),
    "no permission": t("checkNoPerm"),
    "rate limited": t("checkRateLimited"),
    unverifiable: t("checkUnverifiable"),
    checking: t("checking"),
  };
  return map[raw] ?? raw;
}

/** 在输入框正下方显示行内错误并红框聚焦该框；再次输入自动清除。
 * 提示行的插入位置：字段在 field-row 里（如自定义协议输入）则贴整行
 * 之后，裸字段则紧跟其后——总之视觉上紧贴出错的那个框。
 */
function fieldError(input: HTMLInputElement | HTMLSelectElement, msg: string): void {
  flagInvalid(input);
  const host = input.parentElement ?? input;
  const anchor = host.classList.contains("field-row") ? host : input;
  anchor.parentElement?.querySelector(`.field-err[data-for="${input.dataset.errId ?? ""}"]`)?.remove();
  input.dataset.errId ??= String(Math.random()).slice(2);
  const err = document.createElement("p");
  err.className = "field-err";
  err.dataset.for = input.dataset.errId;
  err.textContent = msg;
  anchor.after(err);
  input.addEventListener("input", () => err.remove(), { once: true });
}

/** 标记校验失败的输入框：红边 + 聚焦定位，用户一修改立即恢复。 */
function flagInvalid(input: HTMLInputElement | HTMLSelectElement): void {
  input.classList.add("input-invalid");
  input.addEventListener(
    "input",
    () => input.classList.remove("input-invalid"),
    { once: true },
  );
  input.focus();
}

/** 与 core::store::is_valid_remote_url 同规则的 TS 版：任何显式 scheme
 * （https/ssh/git/file…）或本地绝对路径（/ 、盘符、\\）放行，只拒
 * 裸域名笔误。
 */
/** 与 core::remote::parse_github_repo 同接受集的前置校验（前端即时反馈，
 * 后端 set_remote 兜底）：github.com URL 或 owner/name 两段。2026-11-03
 * 修复：此前接受集含本地路径、缺 owner/name，裸仓库名被误拒——与后端
 * 实际接受集（只认 GitHub 形态）背离。 */
function isValidRemoteUrl(t: string): boolean {
  const v = t
    .trim()
    .replace(/\/+$/, "")
    .replace(/(?:\.git)+$/, "");
  if (!v || /\s/.test(v)) return false;
  const tail = v.startsWith("https://github.com/")
    ? v.slice("https://github.com/".length)
    : v.startsWith("http://github.com/")
      ? v.slice("http://github.com/".length)
      : v.includes("://")
        ? null // 其他 scheme 后端同样不接受，整串当 owner/name 必败
        : v;
  if (tail === null || tail === "") return false;
  if (tail.includes(":")) return false; // SSH 形态（git@…）与后端同拒绝
  const parts = tail.split("/");
  return parts.length === 2 && parts.every((p) => p !== "");
}

/** 按名字生成一个内联 SVG 图标元素。 */
function icon(name: keyof typeof ICONS | string): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "2");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  svg.innerHTML = ICONS[name] ?? "";
  return svg;
}

// ---------------------------------------------------------------------------
// 外观偏好（D14，2026-10-24）：主题 + 界面语言。
// 本机 localStorage、不参与 git 同步——主题/语言是设备级个人偏好，
// 随仓库同步会强加给其他设备。
// ---------------------------------------------------------------------------

type ThemePref = "light" | "dark" | "system";
type Lang = "en" | "zh";

interface UiPrefs {
  theme: ThemePref;
  lang: Lang;
}

const PREFS_KEY = "myapikey-ui-prefs";

/** 系统语言检测：中文环境默认中文，其余默认英文。 */
function detectLang(): Lang {
  return navigator.language.toLowerCase().startsWith("zh") ? "zh" : "en";
}

function loadPrefs(): UiPrefs {
  try {
    const raw = JSON.parse(localStorage.getItem(PREFS_KEY) ?? "{}") as Partial<UiPrefs>;
    return {
      theme: raw.theme === "light" || raw.theme === "dark" ? raw.theme : "system",
      lang: raw.lang === "zh" || raw.lang === "en" ? raw.lang : detectLang(),
    };
  } catch {
    return { theme: "system", lang: detectLang() };
  }
}

let prefs: UiPrefs = loadPrefs();

function savePrefs(): void {
  localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
}

/**
 * 解析主题偏好并落到 `<html data-theme>`（CSS 变量的双主题开关）。
 * "跟随系统"在这里即时解析 prefers-color-scheme——CSS 不用 media 查询，
 * 保证用户显式选择永远优先于系统值。
 */
function applyTheme(): void {
  const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  const actual = prefs.theme === "system" ? (dark ? "dark" : "light") : prefs.theme;
  document.documentElement.dataset.theme = actual;
}

/** 语言应用：同步 <html lang>（无障碍/输入法依据），并上报 Rust 侧
 * ——托盘菜单据此本地化并即时重建（浏览器 mock 模式下为 no-op）。 */
function applyLang(): void {
  document.documentElement.lang = prefs.lang === "zh" ? "zh-CN" : "en";
  void invoke("set_ui_lang", { lang: prefs.lang }).catch(() => {});
}

// "跟随系统"期间监听系统主题切换（Windows 明暗切换即刻生效）
window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  if (prefs.theme === "system") applyTheme();
});
applyTheme();
applyLang();

/**
 * 文案字典：[英文, 中文]。`t(key, ...args)` 按 `{0}` 占位符插值。
 * 注意：后端返回的错误字符串（git/加密等）保持英文原样透传，
 * 不在此翻译——后端是单语（CLI 规范），翻译面仅覆盖前端自有文案。
 */
const STRINGS: Record<string, [string, string]> = {
  // 通用
  unlock: ["Unlock", "解锁"],
  unlocking: ["Unlocking…", "正在解锁…"],
  unlockPlaceholder: ["Passphrase", "解锁密码"],
  lockedHint: ["Locked. Enter the passphrase to continue.", "已锁定，输入解锁密码继续。"],
  keychainGuideTitle: ["Keychain access requested", "即将请求钥匙串授权"],
  keychainGuideBody: [
    'Click "Always Allow" in the system dialog.',
    "系统弹窗中点「始终允许」。",
  ],
  keychainGuideDeny: ["Deny", "拒绝"],
  keychainGuideAllow: ["Allow", "允许"],
  keychainGuideAlways: ["Always Allow", "始终允许"],
  cancel: ["Cancel", "取消"],
  save: ["Save", "保存"],
  backupSection: ["Backup & migration", "备份与迁移"],
  exportLabel: ["Export records", "导出记录"],
  exportBtn: ["Export", "导出"],
  exportConfirm: [
    "Export writes all records as plaintext JSON — delete the file when done.",
    "导出将以明文 JSON 写入全部记录，用完即删。",
  ],
  exportedPath: ["Exported: {0}", "已导出：{0}"],
  importLabel: ["Import records", "导入记录"],
  importBtn: ["Import", "导入"],
  importedSummary: [
    "Import done: {0} added, {1} updated",
    "导入完成：新增 {0} 条，更新 {1} 条",
  ],
  importedBackup: ["Pre-import snapshot: {0}", "导入前快照：{0}"],
  importedNone: ["File matches current records.", "文件与当前记录一致。"],
  syncInstallFailed: [
    "GitHub App is not installed — sync needs it. Follow the banner above.",
    "GitHub App 未安装，同步需要它——按上方引导条操作。",
  ],
  saving: ["Saving…", "正在保存…"],
  settings: ["Settings", "设置"],
  back: ["Back", "返回"],
  // 初始化
  initSubtitle: ["Set up this device. Choose how to create your sync repository.", "初始化本设备，选择同步仓库的创建方式。"],
  confirmPass: ["Confirm passphrase", "确认解锁密码"],
  modeGithub: ["GitHub, auto-create a private repo", "GitHub，自动创建私有仓库"],
  modeRemote: ["Existing repository URL", "已有仓库地址"],
  modeSolo: ["Standalone", "单机使用"],
  initWarn: [
    "If you forget the passphrase, all data is unrecoverable. All devices must use the same passphrase.",
    "忘记解锁密码将无法恢复数据。所有设备必须使用同一解锁密码。",
  ],
  createVault: ["Create vault", "创建保险库"],
  creating: ["Creating…", "正在创建…"],
  passMismatch: ["Passphrases do not match", "两次输入的解锁密码不一致"],
  passTooShort: ["At least 8 characters", "至少 8 个字符"],
  passTooLong: ["At most 128 characters", "至多 128 个字符"],
  installNeededBanner: [
    "GitHub App is not installed — repo sync unavailable.",
    "GitHub App 尚未安装，仓库同步不可用。",
  ],
  installApp: ["Install GitHub App", "安装 GitHub App"],
  // 已安装但安装范围不含当前绑定的同步仓库:装了≠装全,GitHub App 按
  // 仓库授权,范围缺失时 Contents API 依旧 404(2026-11-04 用户实测:
  // installed=true 使引导条消失,同步却仍 404,无任何提示)
  installScopeBanner: [
    "GitHub App is installed, but the sync repo is not in its repository access — sync unavailable.",
    "GitHub App 已安装，但未包含同步仓库，同步不可用。",
  ],
  installScopeBtn: ["Adjust repository access", "调整仓库范围"],
  // 日志区（2026-11-05 用户需求:错误详情落日志,不进悬停/弹窗）
  logSection: ["Logs", "日志"],
  logLevel: ["Log level", "日志级别"],
  logOff: ["Off", "关闭"],
  logError: ["Errors only", "仅错误"],
  logInfo: ["Info (default)", "信息（默认）"],
  logDebug: ["Debug", "调试"],
  savedLogLevel: ["Log level set to {0}", "日志级别已改为 {0}"],


  strength0: ["Weak", "弱"],
  strength1: ["Fair", "中"],
  strength2: ["Strong", "强"],
  strength3: ["Very strong", "极强"],
  remoteRequired: ["Remote URL is required", "请填写仓库地址"],
  vaultCreated: ["Vault created", "保险库已创建"],
  joinedVaultPlain: ["Joined the existing vault", "已接入现有保险库"],
  // 设备授权
  githubAuth: ["Finish binding", "完成绑定"],
  stepOpen: ["Open the page below in your browser", "在浏览器打开下方链接"],
  stepEnter: ["Enter the code below", "在页面中输入下方代码"],
  deviceNote: [
    "Code copied to clipboard · expires in 15 minutes · this page continues automatically once you finish in the browser.",
    "授权码已自动复制到剪贴板，15 分钟内有效；在浏览器完成后本页面会自动继续。",
  ],
  iAuthorized: ["I have authorized", "我已完成授权"],
  retryAuth: ["Retry", "重试"],
  copyCode: ["Copy code", "复制授权码"],
  codeCopied: ["Code copied", "已复制授权码"],
  waitingGithub: ["Waiting for GitHub…", "等待 GitHub 确认…"],
  authFailedHint: ["Authorization failed, try again after authorizing in the browser.", "授权失败，请在浏览器完成授权后重试。"],
  // 主界面
  syncNow: ["Sync now", "立即同步"],
  checkAll: ["Check all", "检测全部"],
  checkingAll: ["Checking all…", "正在检测全部…"],
  syncing: ["Syncing…", "正在同步…"],
  lock: ["Lock", "锁定"],
  searchPlaceholder: ["Search records…", "搜索记录…"],
  addRecord: ["Add record", "添加记录"],
  autoLocked: ["Auto-locked after inactivity", "长时间无操作，已自动锁定"],
  syncPushFailed: ["Push failed: {0}", "推送失败：{0}"],
  syncBoth: ["Synced, pulled and pushed · {0} records", "已同步，拉取并推送 · {0} 条记录"],
  syncPulled: ["Pulled remote changes · {0} records", "已拉取远端变更 · {0} 条记录"],
  syncPushed: ["Pushed local changes · {0} records", "已推送本地变更 · {0} 条记录"],
  syncUpToDate: ["Up to date · {0} records", "已是最新 · {0} 条记录"],
  syncStandaloneHint: [
    "Standalone mode: bind a remote to enable sync",
    "单机模式：绑定远端后才能同步",
  ],
  // 顶栏状态标签（曾长期硬编码英文，中文界面里格外突兀，2026-10-25 修复）
  remoteBound: ["Remote bound", "已绑定远端"],
  standaloneMode: ["Standalone", "单机模式"],
  nRecords: ["{0} records", "{0} 条记录"],
  emptyNoRecords: ["No records yet", "还没有记录"],
  emptyAddFirst: ["Add your first record — stored encrypted on this device.", "添加第一条，数据在本机加密存储。"],
  emptyNoMatch: ["No records match your search", "没有匹配的记录"],
  emptyTryOther: ["Try a different keyword.", "换个关键词试试。"],
  copyKey: ["Copy key", "复制 key"],
  copyUrl: ["Copy base URL", "复制 Base URL"],
  urlCopied: ["Base URL copied", "已复制 Base URL"],
  checkKey: ["Check key", "校验 key"],
  editRecord: ["Edit record", "编辑记录"],
  deleteRecord: ["Delete record", "删除记录"],
  copying: ["Copying…", "正在复制…"],
  checking: ["Checking…", "正在校验…"],
  checkDone: ["Done: {0}", "检测完成：{0}"],
  deleting: ["Deleting…", "正在删除…"],
  copied: ["API key copied, clears in 30 s", "已复制 API key，30 秒后自动清除"],
  copy: ["Copy", "复制"],
  modelCopied: ["Model copied", "已复制模型名"],
  keyCheck: ["Key check: {0}{1}", "校验结果：{0}{1}"],
  httpCode: [" (HTTP {0})", " (HTTP {0})"],
  confirmDelete: ['Delete "{0}"?', "确定删除“{0}”？"],
  deleted: ['Deleted "{0}"', "已删除“{0}”"],
  added: ['Added "{0}"', "已添加“{0}”"],
  updated: ['Updated "{0}"', "已更新“{0}”"],
  // 编辑抽屉
  addTitle: ["Add record", "添加记录"],
  editTitle: ["Edit record", "编辑记录"],
  recordName: ["Record name", "记录名称"],
  protoCustomLabel: ["Custom protocol label", "自定义协议名"],
  baseUrl: ["Base URL", "Base URL"],
  apiKey: ["API key", "API key"],
  notesOpt: ["Notes", "备注"],
  modelsOpt: ["Models", "模型"],
  nameLabel: ["Name", "名称"],
  protoLabel: ["Type", "类型"],
  baseUrlLabel: ["Base URL", "Base URL"],
  apiKeyLabel: ["API key", "API key"],
  notesLabel: ["Notes (optional)", "备注（可选）"],
  pickSearch: ["Filter models…", "过滤模型…"],
  pickAll: ["Select all", "全选"],
  pickNone: ["Clear", "清空"],
  pickAdd: ["Add {0} selected", "添加所选 {0} 个"],
  modelsHint: ["e.g. DeepSeek-V4.1, press Enter or +", "如 DeepSeek-V4.1，回车或 + 添加"],
  addModel: ["+", "+"],
  removeModel: ["Remove model", "删除模型"],
  modelsEmpty: ["No models yet — add manually or fetch.", "暂无模型——手动添加，或点「获取」拉取。"],
  fetchModels: ["Fetch", "获取"],
  fetchingModels: ["Fetching…", "正在获取…"],
  fetchModelsEmpty: ["No models returned — add them manually.", "未获取到模型，请手动添加。"],
  fetchModelsNeedBoth: ["Fill in the base URL and API key first.", "请先填写 Base URL 和 API key。"],
  showPass: ["Show password", "显示密码"],
  hidePass: ["Hide password", "隐藏密码"],
  switchToEn: ["Switch to English", "切换到 English"],
  switchToZh: ["Switch to 中文", "切换到中文"],
  customRequired: ["Custom protocol label is required.", "请填写自定义协议名。"],
  // 设置页
  syncSection: ["Sync", "同步"],
  behaviorSection: ["Behavior", "行为"],
  appearanceSection: ["Appearance", "外观"],
  remotePlaceholder: ["Remote repository URL", "远端仓库地址"],
  repoLabel: ["Remote repository", "远端仓库"],
  repoUnbound: ["Not bound", "未绑定"],
  bindRepo: ["Bind repository", "绑定仓库"],
  authorizeGithub: ["Bind repository", "绑定仓库"],
  passJoinTitle: [
    "Found an existing vault on the remote. Enter the passphrase to join it.",
    "发现已有保险库，输入解锁密码以接入。",
  ],
  passSetTitle: ["Set a new passphrase.", "设置新的解锁密码。"],
  initWarnJoin: [
    "All devices must use the same passphrase.",
    "所有设备必须使用同一解锁密码。",
  ],
  passJoinBtn: ["Join the vault", "接入保险库"],
  unpushedPending: ["Unpushed changes — run sync", "有未推送的修改——请同步"],
  lastSyncChip: ["Last sync: {0}", "上次同步：{0}"],
  lastSyncNever: ["never", "从未"],
  relJustNow: ["just now", "刚刚"],
  relMinutesAgo: ["{0} min ago", "{0} 分钟前"],
  relHoursAgo: ["{0} h ago", "{0} 小时前"],
  relDaysAgo: ["{0} d ago", "{0} 天前"],
  reauthorizing: ["Finishing binding in the browser…", "正在浏览器中完成绑定…"],
  reAuthorized: ["Binding restored", "绑定已恢复"],
  joinedVault: ["Joined the vault · {0} records", "已接入保险库 · {0} 条记录"],
  daysUnit: ["days", "天"],
  autoLock: ["Auto-lock after idle", "空闲自动锁定"],
  lockNever: ["Never", "永不"],
  lockMinutesOpt: ["{0} min", "{0} 分钟"],
  lockHours: ["{0} h", "{0} 小时"],
  autoPush: ["Auto-push after changes", "修改后自动推送"],
  themeLabel: ["Theme", "主题"],
  themeLight: ["Light", "明亮"],
  themeDark: ["Dark", "暗黑"],
  themeSystem: ["Follow system", "跟随系统"],
  langLabel: ["Language", "语言"],
  savedLock: ["Auto-lock set to {0}", "自动锁定已改为 {0}"],
  savedAutoPushOn: ["Auto-push enabled", "修改后自动推送已开启"],
  savedAutoPushOff: ["Auto-push disabled", "修改后自动推送已关闭"],
  savedRemote: ["Remote address set to {0}", "远端地址已改为 {0}"],
  savedRemoteCleared: ["Remote address cleared, syncing stopped", "远端地址已清空，本机停止同步"],
  savedTheme: ["Theme set to {0}", "主题已改为 {0}"],
  savedLang: ["Language set to {0}", "界面语言已改为 {0}"],
  // 设置页 · 更新（D16 应用内更新）
  updateSection: ["Update", "更新"],
  currentVersion: ["Version {0}", "当前版本 {0}"],
  checkUpdate: ["Check for updates", "检查更新"],
  unbindGithub: ["Unbind GitHub", "解绑 GitHub"],
  unbindGithubConfirm: [
    "Unbind the remote repo? The remote repo and vault data are untouched.",
    "解绑远端仓库？远端仓库与保险库数据不受影响。",
  ],
  unboundGithub: ["GitHub unbound.", "已解绑 GitHub。"],
  // 设置页 · 安全（改解锁密码，2026-11-03 补齐入口）
  securitySection: ["Security", "安全"],
  changePassphrase: ["Change passphrase", "修改解锁密码"],
  changePassTitle: ["Confirm the current passphrase, then set a new one.", "验证当前解锁密码，设置新的解锁密码。"],
  oldPass: ["Current passphrase", "当前解锁密码"],
  newPass: ["New passphrase", "新解锁密码"],
  confirmNewPass: ["Confirm new passphrase", "确认新解锁密码"],
  oldPassRequired: ["Current passphrase must not be empty.", "请输入当前解锁密码。"],
  newPassRequired: ["New passphrase must not be empty.", "请输入新解锁密码。"],
  passChanged: ["Passphrase changed.", "解锁密码已修改。"],
  passChangeWarn: [
    "The remote vault will be re-encrypted with the new passphrase. Other devices must enter it on their next sync.",
    "远端保险库将以新解锁密码重新加密，其他设备下次同步需输入新密码。",
  ],
  checkingUpdate: ["Checking…", "正在检查…"],
  upToDate: ["Up to date.", "已是最新版本。"],
  notChecked: ["not checked", "未校验"],
  // 校验状态（后端 CheckStatus.label 为英文，前端按键映射成双语）
  checkValid: ["valid", "有效"],
  checkInvalid: ["invalid", "无效"],
  checkNoPerm: ["no permission", "无权限"],
  checkRateLimited: ["rate limited", "限流"],
  checkUnverifiable: ["unverifiable", "无法验证"],
  invalidUrl: [
    "Must start with http:// or https:// followed by a host.",
    "需为 http:// 或 https:// 开头并带主机名。",
  ],
  nameRequired: ["Record name must not be empty.", "记录名称不能为空。"],
  baseUrlRequired: ["Base URL must not be empty.", "Base URL 不能为空。"],
  apiKeyRequired: ["API key must not be empty.", "API key 不能为空。"],
  invalidRemote: [
    "Expected owner/name or a github.com URL.",
    "需为 owner/name 或 github.com 网址。",
  ],
  protoCustomOpt: ["custom…", "自定义…"],
  newVersionAvail: ["Version {0} is available.", "新版本 {0} 可用。"],
  breakingWarn: [
    "This is a breaking update and will not be installed automatically. Back up your data first, then upgrade manually from the releases page.",
    "这是破坏性更新，不会自动安装。请先备份数据，再到发布页面手动升级。",
  ],
  upgradeRestart: ["Upgrade and restart", "升级并重启"],
  upgrading: ["Upgrading…", "正在升级…"],
};

/**
 * 快捷语言切换按钮（初始化引导、解锁页与 GitHub 授权页用——这些页面
 * 到不了设置页，语言不符时用户无处可调，2026-10-25 需求）。
 * 样式沿用业界通行模式（Google / Cloudflare 等）：地球图标 + 当前语言
 * 短码——图标表明按钮职能，短码显示"现在是什么语言"（而非目标语言），
 * 点击即切换到另一种，悬停提示写明动作。
 */
function langToggle(rerender: () => void): HTMLButtonElement {
  const btn = el("button", "btn btn-small btn-ghost lang-toggle");
  const sync = () => {
    btn.replaceChildren(
      icon("globe"),
      document.createTextNode(prefs.lang === "zh" ? "中" : "EN"),
    );
    const tip = t(prefs.lang === "zh" ? "switchToEn" : "switchToZh");
    btn.dataset.tip = tip;
    btn.setAttribute("aria-label", tip);
  };
  sync();
  btn.addEventListener("click", () => {
    prefs.lang = prefs.lang === "zh" ? "en" : "zh";
    savePrefs();
    applyLang();
    rerender();
  });
  return btn;
}

/**
 * 协议存储标签 → 展示名。openai / anthropic 实为公司名，展示用公司规范
 * 大小写（OpenAI / Anthropic）；存储值保持小写不动——协议只是用户填的
 * 标签，改存储会破坏既有 vault 数据。自定义标签原样显示。
 */
function protoLabel(p: string): string {
  if (p === "openai") return "OpenAI (Responses)";
  if (p === "openai-chat") return "OpenAI Chat";
  if (p === "anthropic") return "Anthropic (Messages)";
  if (p === "anthropic-complete") return "Anthropic Complete";
  return p;
}

/**
 * 协议族短名（2026-10-31 卡片徽章）：只显示 OpenAI / Anthropic /
 * 自定义名，具体 API 形态靠 hover title（protoLabel）呈现——徽章上
 * 长文案是视觉噪音，且 openai-chat 等旧形态本就没有专属配色。
 */
function protoShort(p: string): string {
  if (p.startsWith("openai")) return "OpenAI";
  if (p.startsWith("anthropic")) return "Anthropic";
  if (p.startsWith("custom:")) return p.slice("custom:".length);
  return p;
}

/** 协议族 → 徽章配色类（同族同色：openai 绿 / anthropic 黄 / custom 中性）。 */
function protoBadgeClass(p: string): string {
  if (p.startsWith("openai")) return "badge-openai";
  if (p.startsWith("anthropic")) return "badge-anthropic";
  return "badge-custom";
}

/**
 * 模型名 → 胶囊配色下标（0-5）：名字哈希取模，同一模型永远同色，
 * 不同模型自然分散到色板（2026-10-31 模型胶囊彩色化）。
 */
function modelColorIndex(name: string): number {
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) >>> 0;
  return h % 6;
}

/** 取当前语言文案；`{0}`/`{1}` 占位符按参数顺序替换。 */
function t(key: string, ...args: (string | number)[]): string {
  const pair = STRINGS[key];
  const idx = prefs.lang === "zh" ? 1 : 0;
  let s = pair ? pair[idx] : key;
  args.forEach((a, i) => {
    s = s.replaceAll(`{${i}}`, String(a));
  });
  return s;
}

/** toast 容器（惰性创建，常驻 body；多条提示在其中纵向堆叠）。 */
let toastWrap: HTMLDivElement | null = null;

/** （重新）安排一条 toast 的自动消失。 */
function armToast(t: HTMLElement): void {
  const prev = t.dataset.timer;
  if (prev) clearTimeout(Number(prev));
  t.dataset.timer = String(window.setTimeout(() => t.remove(), 2600));
}

/**
 * 短暂的行内提示（toast）。
 *
 * - 多条提示在底部居中**纵向堆叠**（上限 3 条，超出时最旧的立即消失），
 *   避免连点时提示在同一位置互相叠住；
 * - 与最新一条文本相同则不重复弹出，只重置其消失计时——
 *   快速重复触发同一结果（如连点 Copy）不会刷出一串相同提示。
 */
function toast(msg: string, ok = true): void {
  if (!toastWrap) {
    toastWrap = el("div", "toast-wrap");
    document.body.appendChild(toastWrap);
  }
  const last = toastWrap.lastElementChild as HTMLElement | null;
  if (last?.dataset.msg === msg) {
    armToast(last);
    return;
  }
  const t = el("div", `toast ${ok ? "toast-ok" : "toast-err"}`, msg);
  t.dataset.msg = msg;
  toastWrap.appendChild(t);
  while (toastWrap.children.length > 3) toastWrap.firstElementChild?.remove();
  armToast(t);
}

/**
 * GitHub 授权失效状态（2026-11-03）：同步/写路径撞认证错误且自动续期
 * 未救场时置位——主界面常驻引导条 + 顶栏 chip 变「授权已失效」，用户
 * 不必从一条转瞬即逝的错误 toast 里猜原因。重新授权成功（showDeviceCode
 * 的 done 分支）时清除。
 */
let authInvalid = false;
/** GitHub 授权/安装诊断结果(后端 diagnose_github,2026-11-04 分步引导):
 * 非空且检测到未完成步骤时,引导条按诊断渲染对应步骤与按钮。 */
let authDiag: GhDiag | null = null;
/** 设备授权轮询的代号:每次进入授权页(或重试)递增——旧码 poll 的迟到
 * 结果凭代号丢弃,避免污染新码页面(2026-11-04 卡死修复配套)。 */
let devicePollSeq = 0;

/** 与 core::sync::is_auth_error 同规则的 TS 镜像（前端置位判断用）。 */
function isAuthError(msg: string): boolean {
  return (
    msg.includes("GitHub rejected the access") ||
    msg.includes("HTTP 401") ||
    msg.includes("HTTP 403") ||
    msg.includes("Bad credentials")
  );
}

/** 同步/写失败的用户可感知错误(2026-11-06 用户需求:令牌/刷新是
 * App 内部的事,用户不应感知):授权类 -> 一句人话指路重新授权;
 * 仓库不可见 -> 指路安装引导;其余(网络等)原样透出。完整技术链
 * 一律落文件日志,不进界面。 */
function syncUserError(msg: string): string {
  if (isAuthError(msg)) return t('syncStandaloneHint');
  if (msg.includes('HTTP 404') || msg.includes('not installed'))
    return t('syncInstallFailed');
  return msg;
}

/** 绑定失效判定(与主界面引导条同一规则):authInvalid 标记,或诊断显示
 * 绑定存在但访问不可用(令牌缺失/令牌失效——用户视角同为一个语义:
 * 绑定失效,2026-11-10 原子化)。诊断未取到时退回标记。 */
function bindingBroken(): boolean {
  if (authInvalid) return true;
  const d = authDiag;
  return d !== null && d.remoteBound && (!d.authorized || !d.tokenValid);
}

/** 授权失效置位：立即填充主界面的引导条与顶栏 chip（主界面可见时），
 * 并触发一次诊断刷新(网络探测,异步回来后引导条按最新状态重渲染)。
 * 错误详情不进界面——完整原因已由后端落文件日志(2026-11-05 用户
 * 反馈:悬停/弹窗输出详情是坏习惯,排查走日志)。 */
function markAuthInvalid(suggestInstall = false): void {
  if (suggestInstall) {
    // 404(仓库不可见)≠ 授权失效:令牌失效时 GitHub 回 401,走不到这里;
    // 对 GitHub App 这意味着「未安装/未含此仓」。合成安装语义诊断并立即
    // 渲染安装引导,**不置 authInvalid**——置了会把引导条顶进「授权已
    // 失效」分支,把用户推进重授权死循环(2026-11-04 状态错显实测)。
    // 异步诊断回来后按真实状态修正(含后端 install_url)。
    authDiag = {
      authorized: true,
      tokenValid: true,
      installed: false,
      // URL 不在此硬编码(2026-11-05 审查可选 C):slug 单一来源是 core
      // 的 APP_SLUG 配置,点击按钮时经 github_install_url 命令懒取。
      // 曾误写 myapikey:软件把用户带到无关 App 的安装页(2026-11-04 实测)
      installUrl: null,
      remoteBound: true,
      repoAccessible: null,
    };
    const slot = document.querySelector<HTMLElement>(".auth-banner-slot");
    if (slot) renderAuthBanner(slot);
    void refreshAuthDiag();
    return;
  }
  // 绑定失效折叠为未绑定(2026-11-10 用户拍板):不渲染失效引导条、
  // 不显示失效字样——置位内部判定供设置页折叠渲染,chip 显示单机,
  // 诊断刷新后各界面自然按未绑定呈现
  authInvalid = true;
  const chip = document.querySelector(".chip-remote");
  if (chip) {
    chip.className = "chip";
    chip.textContent = t("standaloneMode");
  }
  void refreshAuthDiag();
}

/** 拉取授权/安装诊断并重渲染引导条(boot 与失效置位时调用)。 */
async function refreshAuthDiag(): Promise<void> {
  // 诊断失败保留旧诊断:网络抖动不该把已确立的引导条清空
  // (2026-11-04 用户实测「引导条与 toast 一同消失」的场景之一)
  const d = await invoke<GhDiag>("diagnose_github").catch(() => null);
  if (d !== null) authDiag = d;
  const slot = document.querySelector<HTMLElement>(".auth-banner-slot");
  if (slot) renderAuthBanner(slot);
}

/** 按当前状态渲染引导条(空 = 正常,无引导条)。绑定失效已折叠为未绑定
 * (2026-11-10 用户拍板),不再有失效引导条;剩余两态均为 GitHub App
 * 安装语义——needInstall(令牌有效,未安装 **或安装范围不含同步仓库**)
 * → 安装/调整范围按钮。GitHub App 按仓库授权,「已安装」≠「装全」:
 * 范围缺失时 Contents API 依旧 404(2026-11-04 用户实测)。 */
function renderAuthBanner(slot: HTMLElement): void {
  slot.replaceChildren();
  const diag = authDiag;
  // 绑定失效折叠为未绑定(2026-11-10 用户拍板):失效引导条已删——
  // 用户只见绑定与未绑定两态。剩余引导条均为 GitHub App 安装语义
  // (未安装/范围缺失),与绑定状态无关
  const needInstall =
    diag !== null &&
    diag.authorized &&
    diag.tokenValid &&
    diag.remoteBound &&
    (!diag.installed || diag.repoAccessible === false);
  if (!needInstall) return;
  const banner = el("div", "auth-banner");
  const textWrap = el("div", "auth-banner-text-wrap");
  // 错误详情不进界面(title 悬停已废除,2026-11-05):完整错误链由后端
  // 统一落文件日志,设置页可开日志文件夹
  // scopeGap = 已安装但仓库范围不含同步仓库:按钮改指安装配置页
  // (installations 列表页,Configure 里调整 Repository access)
  const scopeGap = diag.installed === true;
  textWrap.append(
    el("span", "", scopeGap ? t("installScopeBanner") : t("installNeededBanner")),
  );
  banner.append(textWrap);
  const actions = el("div", "auth-banner-actions");
  const installBtn = el(
    "button",
    "btn btn-small btn-primary",
    scopeGap ? t("installScopeBtn") : t("installApp"),
  );
  installBtn.type = "button";
  installBtn.addEventListener("click", () => {
    // URL 懒取(2026-11-05 审查可选 C):诊断里没有时向 core 要——
    // slug 单一来源,前端永不硬编码
    void (async () => {
      let url = authDiag?.installUrl ?? "";
      if (!url) {
        url = await invoke<string>("github_install_url").catch(() => "");
      }
      if (!url) return;
      // 安装配置页 = 去掉安装页 URL 尾部的 /new
      if (scopeGap) url = url.replace(/\/new$/, "");
      void invoke("open_external", { url }).catch(() => {
        window.open(url, "_blank");
      });
      // 打开安装/配置页后自动检测:用户装好或调完范围 → 引导条自动
      // 消失,全程无需回应用里手动刷新
      startInstallWatch();
    })();
  });
  actions.append(installBtn);
  banner.append(actions);
  slot.append(banner);
}
/** 安装/配置完成检测轮询(installWatchTimer):用户打开安装或配置页后
 * 每 5 秒复查一次诊断,恢复健康(已安装**且**范围含同步仓库)即引导条
 * 消失。已安装但范围缺失时继续轮询——用户调完 Repository access 后
 * 引导条自动撤下,无需手动刷新。诊断失败(网络抖动)静默跳过本轮,
 * 下轮继续;计时器未启动时不重复启动。
 * 有总时长上限(INSTALL_WATCH_MAX_MS):这是「打开安装页后的伴随检测」,
 * 不是常驻服务——上限到了自动停,用户再次点按钮即重启(2026-11-05
 * 审查可选 A)。 */
let installWatchTimer: number | null = null;
let installWatchStartedAt = 0;
const INSTALL_WATCH_MAX_MS = 10 * 60 * 1000;

function startInstallWatch(): void {
  if (installWatchTimer !== null) return;
  installWatchStartedAt = Date.now();
  installWatchTimer = window.setInterval(async () => {
    if (Date.now() - installWatchStartedAt > INSTALL_WATCH_MAX_MS) {
      if (installWatchTimer !== null) {
        clearInterval(installWatchTimer);
        installWatchTimer = null;
      }
      return;
    }
    try {
      const d = await invoke<GhDiag>("diagnose_github");
      authDiag = d;
      const slot = document.querySelector<HTMLElement>(".auth-banner-slot");
      if (slot) renderAuthBanner(slot);
      const unhealthy =
        authDiag !== null &&
        authDiag.authorized &&
        authDiag.tokenValid &&
        authDiag.remoteBound &&
        (!authDiag.installed || authDiag.repoAccessible === false);
      if (!unhealthy && installWatchTimer !== null) {
        clearInterval(installWatchTimer);
        installWatchTimer = null;
      }
    } catch {
      // 网络抖动静默跳过本轮
    }
  }, 5000);
}

/**
 * 按钮级等待反馈：耗时操作期间禁用按钮、文案切换为 `busyText`
 * 并显示旋转指示（.btn-busy），结束后恢复原状。
 *
 * 同时是幂等闸门：按钮已处于禁用态（操作进行中）时直接忽略本次点击，
 * 从根上防止双击重复提交。`fn` 内部自行处理错误（toast / errBox），
 * 不要向外抛——finally 无论如何都会恢复按钮。
 *
 * 图标按钮（.btn-icon）忙碌时只显示旋转指示（文字会撑爆固定宽度），
 * 普通按钮显示 旋转指示 + 文案。成功后按钮所在子树通常已被整体替换
 * （页面切换），恢复动作无害。
 */
async function withBusy(btn: HTMLButtonElement, busyText: string, fn: () => Promise<void>): Promise<void> {
  if (btn.disabled) return; // 幂等：进行中的操作，忽略重复触发
  const orig = btn.innerHTML;
  btn.disabled = true;
  btn.classList.add("btn-busy");
  btn.textContent = btn.classList.contains("btn-icon") ? "" : busyText;
  try {
    await fn();
  } finally {
    btn.classList.remove("btn-busy");
    btn.disabled = false;
    btn.innerHTML = orig;
  }
}

function fmtSyncResult(v: SyncView): string {
  if (v.push_error) return t("syncPushFailed", v.push_error);
  if (v.pulled && v.pushed) return t("syncBoth", v.alive);
  if (v.pulled) return t("syncPulled", v.alive);
  if (v.pushed) return t("syncPushed", v.alive);
  return t("syncUpToDate", v.alive);
}

// ---------------------------------------------------------------------------
// 启动路由
// ---------------------------------------------------------------------------

/** 钥匙串授权引导是否显示（D57）：新构建首次 `gui_state` 为 true，后端
 * 已随检测写回指纹——本次会话内解锁页提示一次即可，之后不再打扰。 */
let keychainHintVisible = false;

async function boot(): Promise<void> {
  await onLocked(() => {
    clearDrawerDraft();
    renderUnlock(true);
    toast(t("autoLocked"));
  });
  // 启动期主动续期失败(后端线程发出):置失效态——进入主界面后
  // 引导条 + chip 立即可见,不必等用户点同步撞 401(2026-11-03 需求 2)
  void onAuthInvalid(() => markAuthInvalid());
  const st = await invoke<GuiState>("gui_state");
  keychainHintVisible = await invoke<boolean>("keychain_hint");
  if (!st.initialized) renderInit();
  else if (st.unlocked) renderMain();
  else {
    // 先渲染锁定页——remembered_status / unlock_remembered 的钥匙串
    // 读取可能触发系统授权弹窗并阻塞命令返回，绝不能挡住首帧
    // （2026-11-13 修复：曾因 gui_state 内同步读钥匙串导致弹窗期间
    // 窗口完全空白）。
    renderUnlock(false);
    const remembered = await invoke<boolean>("remembered_status");
    // 上次以解锁状态退出（锁定会清除记住的密码，走不到这里）——
    // 记住的密码仍在有效期内：静默恢复解锁，直接进主界面。
    // 2026-10-25 决议：启动状态跟随用户上次的状态。失败留在已渲染
    // 的解锁页（此时 keychainHintVisible 已就绪，引导面板可见）。
    if (remembered && (await invoke<boolean>("unlock_remembered"))) renderMain();
  }
  // 启动诊断(2026-11-04 分步引导):令牌失效/App 未安装时引导条立即
  // 呈现,不必等用户点同步撞错。单机(未绑定远端)时诊断返回不引导
  void refreshAuthDiag();
}

/**
 * 关闭所有编辑抽屉。抽屉的 overlay 挂在 body 上，页面级替换（$app）
 * 带不走它——进入解锁页（手动/自动锁定）前必须显式关闭，
 * 否则已 Reveal 明文 key 的抽屉会残留在解锁页之上。
 */
/**
 * 应用内确认弹窗（Promise 风格，替代 window.confirm）。
 * WebView2 给原生 JS 对话框硬塞 "tauri" 标题且样式与应用主题割裂
 * （2026-10-25 反馈），自绘与抽屉同一套 overlay 视觉。
 * 安全默认：取消按钮持焦——回车不会误确认；Escape / 点遮罩 = 取消。
 */
// 当前挂起的确认框 resolver：closeOverlays 移除 DOM 前先把它 settle 掉，
// 否则 await 它的 async 闭包永远悬挂在内存里（2026-10-26 审查建议）
let pendingConfirm: ((val: boolean) => void) | null = null;

function confirmBox(message: string, confirmText: string): Promise<boolean> {
  return new Promise((resolve) => {
    const overlay = el("div", "overlay confirm-overlay");
    const dialog = el("div", "dialog");
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");
    dialog.append(el("p", "dialog-msg", message));
    const actions = el("div", "dialog-actions");
    const cancel = el("button", "btn", t("cancel"));
    const ok = el("button", "btn btn-danger", confirmText);
    const close = (val: boolean) => {
      pendingConfirm = null;
      overlay.remove();
      resolve(val);
    };
    pendingConfirm = close;
    cancel.addEventListener("click", () => close(false));
    ok.addEventListener("click", () => close(true));
    // 点遮罩 = 取消（与抽屉同样带 mousedown 守卫，过滤浏览器弹窗合成的 click）
    overlay.addEventListener("mousedown", (ev) => {
      if (ev.target === overlay) close(false);
    });
    overlay.addEventListener("keydown", (ev) => {
      if (ev.key === "Escape") {
        ev.preventDefault();
        close(false);
      } else if (ev.key === "Enter" && ev.target instanceof HTMLButtonElement) {
        ev.preventDefault();
        close(ev.target === ok);
      } else if (ev.key === "Tab") {
        // 焦点陷阱：模态内两个按钮间循环，Tab 不落到被遮住的页面上
        ev.preventDefault();
        (document.activeElement === ok ? cancel : ok).focus();
      }
    });
    actions.append(cancel, ok);
    dialog.append(actions);
    overlay.append(dialog);
    document.body.appendChild(overlay);
    cancel.focus();
  });
}



function closeOverlays(): void {
  // 挂起的确认框先按"取消"结算，再摘 DOM——悬挂的 Promise 不进内存坟场
  pendingConfirm?.(false);
  document.querySelectorAll(".overlay").forEach((n) => n.remove());
}

/**
 * 捕获当前页所有带 data-field 输入框的值（按字段名索引）。
 * 语言切换等场景会整页重渲染，未提交的输入会跟着丢——重渲染前先快照，
 * 重建后用 {@link restoreFields} 回填（2026-10-25 修复"切换语言清空输入"）。
 */
function snapshotFields(): Record<string, { v: string; t?: string }> {
  const out: Record<string, { v: string; t?: string }> = {};
  document
    .querySelectorAll<HTMLInputElement | HTMLSelectElement>("[data-field]")
    .forEach((inp) => {
      // 密码框连带记录可见状态（password 或 text 都存——显示态恰是
      // text，排除 text 会把要保留的状态丢掉）：语言切换重建后明文/
      // 掩码不打回隐藏（2026-10-26 采纳审查建议）
      out[inp.dataset.field ?? ""] = {
        v: inp.value,
        t: inp instanceof HTMLInputElement ? inp.type : undefined,
      };
    });
  return out;
}

/**
 * ISO 时间戳 → 简洁相对时间（中英文随语言设置）：刚刚 / N 分钟前 /
 * N 小时前 / N 天前，超过 30 天回退本地日期。异常输入原样返回。
 * 完整精度由调用方以 title 悬停补足（2026-10-31 替代裸 ISO 时间戳）。
 */
function relativeTime(iso: string): string {
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return iso;
  const diff = Date.now() - then;
  const min = 60_000;
  const hour = 3_600_000;
  const day = 86_400_000;
  if (diff < min) return t("relJustNow");
  if (diff < hour) return t("relMinutesAgo", Math.floor(diff / min));
  if (diff < day) return t("relHoursAgo", Math.floor(diff / hour));
  if (diff < 30 * day) return t("relDaysAgo", Math.floor(diff / day));
  return fmtLocalFull(new Date(then));
}

/** Date → 本地时区的 `YYYY-MM-DD HH:mm`（悬停兜底与超期回退共用）。 */
function fmtLocalFull(d: Date): string {
  const p = (n: number): string => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 把 {@link snapshotFields} 捕获的值回填到同名 data-field 输入框（只回填存在的字段）。 */
function restoreFields(vals: Record<string, { v: string; t?: string }>): void {
  if (!Object.keys(vals).length) return;
  document
    .querySelectorAll<HTMLInputElement | HTMLSelectElement>("[data-field]")
    .forEach((inp) => {
      const v = vals[inp.dataset.field ?? ""];
      if (v === undefined) return;
      inp.value = v.v;
      if (v.t && inp instanceof HTMLInputElement) inp.type = v.t;
    });
  // type 被恢复后重放密码行按钮（图标/提示与明文状态对齐）
  document.querySelectorAll(".pass-row").forEach((r) => syncPassToggle(r as HTMLElement));
}

/**
 * 密码输入行：输入框 + 行内眼睛按钮切换可见性。
 * WebView2 会给 type=password 渲染原生"眼睛"（::-ms-reveal），但它只在
 * 悬停时出现、点击一次即消失且行为不受页面控制，已在 CSS 隐藏；初始化页
 * 与解锁页的解锁密码框统一用本按钮（2026-10-25 修复"点击一次后消失"）。
 * 眼睛为纯图标按钮，语义由 title / aria-label 悬停提示给出。
 */
/**
 * 解锁密码强度分（0 弱 / 1 中 / 2 强 / 3 极强）。软提示用，不做硬性拦截
 * （2026-11-03 决议）。**前置条件：长度合法（8–128，D32 硬性限制）且由
 * 调用方保证**——长度不足 8 根本不被允许，不参与强度评价。
 *
 * 评分引擎为 **zxcvbn**（Dropbox 算法，业界事实标准；2026-11-04 用户
 * 要求改用公认库，推翻同日手写启发式决议——手写规则漏评质数拼接
 * 23571113 一类模式）。zxcvbn 输出 0–4，映射到四档：0/1 → 弱、
 * 2 → 中、3 → 强、4 → 极强。
 */
async function passStrength(pw: string): Promise<number> {
  const checker = await ensureZxcvbnChecker();
  const { score } = checker.check(pw);
  return score <= 1 ? 0 : score - 1;
}

/**
 * 密码强度条（软提示）：4 段色块 + 文字标签，随输入实时刷新。
 * 空输入整块隐藏——未输入时不占位也不预判；不拦截提交。返回 sync
 * 供语言切换整页重建后回填密码时手动刷新（restoreFields 直赋 value
 * 不触发 input 事件）。
 */
function strengthMeter(input: HTMLInputElement): { el: HTMLDivElement; sync: () => void } {
  const wrap = el("div", "strength-meter hidden");
  const bars = el("div", "strength-bars");
  const segs: HTMLElement[] = [];
  for (let i = 0; i < 4; i++) {
    const s = el("span", "strength-seg");
    segs.push(s);
    bars.append(s);
  }
  const label = el("span", "strength-label");
  wrap.append(bars, label);
  // 评分代守卫:zxcvbn 首次加载与计算是异步的,快速输入时旧评分的
  // 迟到结果不得覆盖新输入的评分(与搜索竞态守卫同型)
  let strengthSeq = 0;
  const sync = (): void => {
    const pw = input.value;
    // 只有长度合法(8–128,与 validate_passphrase 同规则)才评强度:
    // 长度不足时「至少 8 个字符」的错误提示与强度条并存会互相打架
    // (2026-11-04 用户反馈)
    const n = [...pw].length;
    if (!pw || n < 8 || n > 128) {
      wrap.classList.add("hidden");
      return;
    }
    wrap.classList.remove("hidden");
    const gen = ++strengthSeq;
    void passStrength(pw).then((sc) => {
      if (gen !== strengthSeq) return;
      segs.forEach((s, i) => {
        s.className = i <= sc ? `strength-seg strength-s${sc}` : "strength-seg";
      });
      label.textContent = t(`strength${sc}`);
      label.className = `strength-label strength-s${sc}`;
    });
  };
  input.addEventListener("input", sync);
  return { el: wrap, sync };
}

function passField(input: HTMLInputElement): HTMLDivElement {
  const row = el("div", "field-row pass-row");
  const toggle = el("button", "btn btn-icon");
  toggle.type = "button";
  toggle.dataset.for = input.dataset.field ?? "";
  const shown = () => input.type === "text";
  const sync = () => syncPassToggle(row);
  toggle.addEventListener("click", () => {
    input.type = shown() ? "password" : "text";
    sync();
  });
  row.append(input, toggle);
  // 首次同步必须在元素入行之后：syncPassToggle 走 DOM 查询找 input，
  // append 之前查不到会静默跳过，按钮就一直没有图标（2026-10-26 反馈）
  sync();
  return row;
}

/** 按输入框当前 type 刷新一行密码切换按钮的图标与提示。
 * 独立成全局函数：restoreFields 恢复 type 后也要重放（构建时的 sync
 * 跑在恢复之前，不重放会出现"明文+睁眼图标"的错位，2026-10-26）。
 */
function syncPassToggle(row: HTMLElement): void {
  const input = row.querySelector("input");
  const toggle = row.querySelector("button");
  if (!(input instanceof HTMLInputElement) || !(toggle instanceof HTMLButtonElement)) return;
  const shown = input.type === "text";
  toggle.replaceChildren(icon(shown ? "eyeOff" : "eye"));
  const label = t(shown ? "hidePass" : "showPass");
  toggle.dataset.tip = label;
  toggle.setAttribute("aria-label", label);
}

/** 自绘下拉的句柄：root 挂载用，value 读写对齐原生 select 的语义。 */
interface DdHandle {
  root: HTMLElement;
  get value(): string;
  set value(v: string);
}

/**
 * 自绘下拉（替代原生 select，2026-10-31）：原生控件的下拉面板无法
 * 样式化，本组件用"按钮 + 卡片浮层"呈现，明暗主题自适应。
 * 外部点击关闭；Enter/Escape 在面板内不冒泡（抽屉里 Enter=保存）。
 * 选项变更走 onChange；整页重建的回填直接用新值重建即可。
 */
function makeSelect(
  opts: Array<{ value: string; label: string }>,
  initial: string,
  onChange?: (v: string) => void,
): DdHandle {
  let val = initial;
  const root = el("div", "dd");
  const btn = el("button", "dd-btn");
  btn.type = "button";
  const labelSpan = el("span", "dd-label", opts.find((o) => o.value === val)?.label ?? val);
  btn.append(labelSpan, icon("chevron"));
  const list = el("div", "dd-list");

  const onDoc = (ev: MouseEvent): void => {
    if (!root.contains(ev.target as Node)) {
      root.classList.remove("open");
      document.removeEventListener("mousedown", onDoc, true);
    }
  };
  const close = (): void => {
    root.classList.remove("open");
    document.removeEventListener("mousedown", onDoc, true);
  };
  btn.addEventListener("click", () => {
    if (root.classList.contains("open")) {
      close();
      return;
    }
    root.classList.add("open");
    // 捕获阶段挂 document：面板内 mousedown 不触发外部关闭
    document.addEventListener("mousedown", onDoc, true);
  });

  const renderItems = (): void => {
    list.replaceChildren();
    for (const o of opts) {
      const item = el("button", o.value === val ? "dd-item active" : "dd-item");
      item.type = "button";
      item.append(el("span", "dd-check", "✓"), el("span", "", o.label));
      // 面板内 Enter（选中）/Escape（收起）都不冒泡到抽屉的 Enter=保存
      item.addEventListener("keydown", (ev) => {
        if (ev.key === "Enter" || ev.key === "Escape") ev.stopPropagation();
      });
      item.addEventListener("click", () => {
        val = o.value;
        labelSpan.textContent = o.label;
        renderItems();
        close();
        onChange?.(val);
      });
      list.append(item);
    }
  };
  renderItems();
  root.append(btn, list);
  return {
    root,
    get value(): string {
      return val;
    },
    set value(v: string) {
      val = v;
      labelSpan.textContent = opts.find((o) => o.value === v)?.label ?? v;
      renderItems();
    },
  };
}

// ---------------------------------------------------------------------------
// 全局自绘 tooltip（2026-10-31 反馈：原生 title 黑底白字丑）
// ---------------------------------------------------------------------------

let tipTimer: number | null = null;
let tipTarget: Element | null = null;
const tipEl = el("div", "tooltip hidden");

/** 显示 tooltip：目标下方居中；越出视口底部翻转到上方、水平夹紧。 */
function showTip(target: Element): void {
  const text = target.getAttribute("data-tip") ?? "";
  // 定时器触发时目标可能已被移除（检测全部逐条重渲染是高频场景）：
  // detached 元素 rect 全 0，会把 tooltip 夹紧到视口左上角驻留
  if (!text || !target.isConnected) {
    hideTip();
    return;
  }
  tipEl.textContent = text;
  tipEl.classList.remove("hidden");
  if (!tipEl.isConnected) document.body.append(tipEl);
  const r = target.getBoundingClientRect();
  const tr = tipEl.getBoundingClientRect();
  let x = r.left + r.width / 2 - tr.width / 2;
  x = Math.max(8, Math.min(x, window.innerWidth - tr.width - 8));
  let y = r.bottom + 6;
  if (y + tr.height > window.innerHeight - 8) {
    y = Math.max(8, r.top - tr.height - 6);
  }
  tipEl.style.left = `${x}px`;
  tipEl.style.top = `${y}px`;
}

// 事件委托：悬停带 [data-tip] 的元素 350ms 后显示，移开即隐藏。
// 子元素间移动若仍在同一目标上不重置延迟。全部 hover 提示统一走
// data-tip（不再用原生 title——系统绘制无法跟随应用主题）。
document.documentElement.addEventListener("mouseleave", hideTip);
document.addEventListener("mouseover", (ev) => {
  const t = (ev.target as Element).closest?.("[data-tip]") ?? null;
  if (t === tipTarget) return;
  tipTarget = t;
  if (tipTimer !== null) {
    clearTimeout(tipTimer);
    tipTimer = null;
  }
  hideTip();
  if (t) {
    tipTimer = window.setTimeout(() => {
      tipTimer = null;
      showTip(t);
    }, 350);
  }
});

function hideTip(): void {
  tipEl.classList.add("hidden");
}

// ---------------------------------------------------------------------------
// 首次初始化
// ---------------------------------------------------------------------------

/** git 缺失引导页：说明 + 下载入口 + 重试。装好前主流程不进入。 */


/** 初始化页同步方式的跨重建记忆（自绘下拉不参与 data-field 快照）。 */
let initModePref = "github";

function renderInit(): void {
  // 语言切换会经 langToggle 重新走本函数：先把已填内容快照下来，重建后回填
  const keep = snapshotFields();
  $app.replaceChildren();
  const wrap = el("div", "center-wrap");
  const card = el("div", "card init-card");

  const brand = el("h1", "brand-row");
  brand.append(icon("key"), el("span", "brand", "Makara"), langToggle(renderInit));
  card.append(brand);
  card.append(el("p", "muted", t("initSubtitle")));

  // 密码不在本页：先选同步方式 → 探测远端 → 按远端状态显示对应密码表单。
  // 语言切换整页重建：mode 初值从模块级偏好恢复（自绘下拉没有
  // data-field，snapshotFields 抓不到——审查修复 M1：原注释声称的
  // "从快照恢复"是死路，每次重建都重置回 github）
  const mode = makeSelect(
    [
      { value: "github", label: t("modeGithub") },
      { value: "remote", label: t("modeRemote") },
      { value: "solo", label: t("modeSolo") },
    ],
    initModePref,
    (v) => {
      initModePref = v;
      syncRemote(v);
    },
  );
  const remoteUrl = el("input", "input");
  remoteUrl.type = "url";
  remoteUrl.placeholder = "https://github.com/<you>/<repo>.git";
  remoteUrl.dataset.field = "remoteUrl";
  // 回填可能改变 mode 的选中项，可见性同步必须可重复调用（不能只在 change 事件里做）
  function syncRemote(v: string): void {
    remoteUrl.style.display = v === "remote" ? "" : "none";
  }
  syncRemote(mode.value);

  const warn = el("p", "warn", t("initWarn"));
  const btn = el("button", "btn btn-primary", t("createVault"));

  async function doInit(): Promise<void> {
    if (mode.value === "remote") {
      if (!remoteUrl.value.trim()) {
        toast(t("remoteRequired"), false);
        return;
      }
      if (!isValidRemoteUrl(remoteUrl.value)) {
        fieldError(remoteUrl, t("invalidRemote"));
        return;
      }
    }
    try {
      const stage = await invoke<InitStage>("init_start", {
        remote: mode.value === "remote" ? remoteUrl.value.trim() : null,
        githubRepo: mode.value === "github" ? "myapikey-vault" : null,
      });
      if (stage.stage === "device_code") {
        showDeviceCode(stage.user_code, stage.verification_uri, () =>
          void renderInit(),
        );
        return;
      }
      if (stage.stage === "probe_done") {
        renderPassphrase(stage.has_data);
        return;
      }
      toast(stage.joined ? t("joinedVault", stage.records) : t("vaultCreated"));
      renderMain();
    } catch (e) {
      toast(syncUserError(String(e)), false);
    }
  }
  // withBusy 负责禁用与等待文案（建仓 + 探测需要一两秒）
  btn.addEventListener("click", () => void withBusy(btn, t("creating"), doInit));
  remoteUrl.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") btn.click();
  });

  card.append(mode.root, remoteUrl, warn, btn);
  wrap.append(card);
  $app.append(wrap);
  restoreFields(keep);
  // 回填后 mode 可能已是 remote：补一次同步，让远端地址框保持可见
  syncRemote(mode.value);
}

/** 初始化第二页：解锁密码表单。语义由远端状态决定——
 * `hasData = true` 显示"输入解锁密码接入已有数据"（单框）；
 * `false` 显示"设置新的解锁密码"（双框确认）。提交走 init_finish。 */
function renderPassphrase(hasData: boolean): void {
  // 语言切换整页重建：已输入的解锁密码照 renderUnlock 模式快照回填
  // （2026-10-31 审查修复 M2：两阶段密码页漏了这对机制）
  const keep = snapshotFields();
  $app.replaceChildren();
  const wrap = el("div", "center-wrap");
  const card = el("div", "card init-card");

  const brand = el("h1", "brand-row");
  brand.append(icon("key"), el("span", "brand", "Makara"), langToggle(() => renderPassphrase(hasData)));
  card.append(brand);
  card.append(el("p", "muted", hasData ? t("passJoinTitle") : t("passSetTitle")));

  const pass = el("input", "input grow");
  pass.type = "password";
  pass.placeholder = t("unlockPlaceholder");
  pass.dataset.field = "pass";
  const rows: HTMLElement[] = [passField(pass)];
  // 强度条仅在设置新密码时显示（接入已有保险库输的是已设过的密码，
  // 事后提示无意义）；紧贴新密码输入框，先于确认框
  let meter: { el: HTMLDivElement; sync: () => void } | null = null;

  if (!hasData) {
    meter = strengthMeter(pass);
    rows.push(meter.el);
    const pass2 = el("input", "input grow");
    pass2.type = "password";
    pass2.placeholder = t("confirmPass");
    pass2.dataset.field = "pass2";
    rows.push(passField(pass2));
  }

  const warn = el("p", "warn", hasData ? t("initWarnJoin") : t("initWarn"));
  const btn = el(
    "button",
    "btn btn-primary",
    hasData ? t("passJoinBtn") : t("createVault"),
  );

  async function doFinish(): Promise<void> {
    if (!hasData) {
      // 新设密码:长度硬性限制(8–128 字符,与后端 validate_passphrase 同规则)
      if ([...pass.value].length < 8) {
        toast(t("passTooShort"), false);
        return;
      }
      if ([...pass.value].length > 128) {
        toast(t("passTooLong"), false);
        return;
      }
      if (pass.value !== pass2value()) {
        toast(t("passMismatch"), false);
        return;
      }
    }
    try {
      await invoke<InitStage>("init_finish", { passphrase: pass.value });
      toast(hasData ? t("joinedVaultPlain") : t("vaultCreated"));
      renderMain();
    } catch (e) {
      toast(syncUserError(String(e)), false);
    }
  }
  function pass2value(): string {
    const inp = card.querySelector<HTMLInputElement>('input[data-field="pass2"]');
    return inp ? inp.value : "";
  }
  btn.addEventListener("click", () => void withBusy(btn, t("creating"), doFinish));
  card.append(...rows, warn, btn);
  wrap.append(card);
  $app.append(wrap);
  restoreFields(keep);
  // 回填的密码不触发 input 事件，强度条要手动对齐
  meter?.sync();
}

/**
 * 设备授权轮询的单例句柄：页面挂载即开始后台轮询，整页重建（语言切换）
 * 复用同一 in-flight 请求——后端 poll 会一次性消费 device code，并发
 * 第二个轮询只会空转到码过期。settle（成功或失败）后置空，允许重试。
 */
/**
 * 设备授权轮询（按 device code 键控，2026-11-04 重构）：同一码复用同一
 * in-flight 请求（后端 poll 一次性消费 device code，同码并发必有一个
 * 空转到码过期）；不同码各自独立——「重新授权」发新码后，旧码的 poll
 * 留在原地自然结束（expired_token），不再阻塞新码的轮询（原单例实现
 * 让新码永远无人轮询，重新授权+返回场景必然卡死，用户真机命中）。
 */
const devicePolls = new Map<string, Promise<InitStage>>();

function ensureDevicePoll(code: string): Promise<InitStage> {
  let p = devicePolls.get(code);
  if (!p) {
    p = invoke<InitStage>("init_device_authorized").finally(() => {
      devicePolls.delete(code);
    });
    devicePolls.set(code, p);
  }
  return p;
}

function showDeviceCode(code: string, uri: string, onBack?: () => void): void {
  $app.replaceChildren();
  const wrap = el("div", "center-wrap");
  const card = el("div", "card init-card");
  const headRow = el("div", "device-head");
  headRow.append(el("h2", "", t("githubAuth")), langToggle(() => showDeviceCode(code, uri, onBack)));
  card.append(headRow);
  // 返回入口(2026-11-04 用户要求「所有界面必须能正常返回」):授权页
  // 此前是死胡同,想放弃授权只能重启。返回只切 UI,后台轮询继续——
  // 若期间授权完成,apply 的自动流转照常接管(完成是用户主动发起的
  // 流程,完成即切走属合理)。按钮钉在页面最左上角(用户指定的位置),
  // 不随卡片布局走;不传 onBack(理论上不存在)则不渲染
  if (onBack) {
    const backBtn = el("button", "btn btn-small btn-ghost device-back", t("back"));
    backBtn.type = "button";
    backBtn.prepend(icon("back"));
    backBtn.addEventListener("click", onBack);
    $app.append(backBtn);
  }
  // 步骤用自定义编号结构（D32 配套微调）：链接行要插在步骤 1 与步骤 2
  // 之间（「下方链接」紧随步骤 1 文本），ol 的自动序号做不到中间插行
  const steps = el("div", "steps");
  steps.append(
    el("div", "step-line", t("stepOpen")),
  );
  const link = el("a", "link", uri);
  link.href = uri;
  link.target = "_blank";
  // WebView 内点链接会离开应用——改走系统浏览器
  link.addEventListener("click", (ev) => {
    ev.preventDefault();
    void invoke("open_external", { url: uri }).catch(() => {
      window.open(uri, "_blank");
    });
  });
  // 链接独立成行放在步骤下方（2026-11-04 修复：原为行内拼接在步骤 1
  // 尾部，文案「打开下方链接」与实际位置不符）；步进 2 的「下方代码」
  // 同理由紧随其后的授权码大字框承接
  const linkRow = el("div", "device-link-row");
  linkRow.append(link);
  steps.append(linkRow);
  steps.append(el("div", "step-line", t("stepEnter")));
  // 授权码行：大字框 + 行内复制按钮（2026-10-25 需求；码 15 分钟有效，
  // 非长期敏感值，复制后不做延时清除）
  const codeEl = el("div", "device-code", code);
  const copyCode = el("button", "btn btn-icon btn-inline");
  copyCode.dataset.tip = t("copyCode");
  copyCode.setAttribute("aria-label", t("copyCode"));
  copyCode.append(icon("copy"));
  copyCode.addEventListener("click", () =>
    void withBusy(copyCode, t("copying"), async () => {
      try {
        await invoke("copy_text", { text: code });
        toast(t("codeCopied"));
      } catch (e) {
        toast(syncUserError(String(e)), false);
      }
    }),
  );
  const codeRow = el("div", "code-row");
  codeRow.append(codeEl, copyCode);
  const note = el("p", "muted", t("deviceNote"));
  const btn = el("button", "btn btn-primary", t("iAuthorized"));
  const status = el("p", "muted", "");
  card.append(steps, codeRow, note, btn, status);
  wrap.append(card);
  $app.append(wrap);

  // 授权码进页即自动复制（2026-10-31 需求）：默认行为静默执行、不 toast，
  // 复制按钮保留（重复制/自动复制失败时的手动兜底）
  void invoke("copy_text", { text: code }).catch(() => {});

  // 授权完成自动识别（2026-10-31 需求）：页面挂载即开始后台轮询，拿到
  // 令牌自动走后续渲染；「我已完成授权」按钮保留为失败后的手动重试入口。
  // 轮询挂在模块级单例上——语言切换等整页重建复用同一 in-flight 请求，
  // 避免重复起轮询（后端 poll 一次性消费 device code，并发轮询必有一个
  // 空转到码过期）
  // 代守卫:重新授权发新码后,旧码 poll 的迟到结果不再污染新码页面
  const gen = ++devicePollSeq;
  const apply = (stage: InitStage): void => {
    if (stage.stage === "done") {
      // 新凭据已入库：授权失效引导条与 chip 就此恢复（无论失效因何而起，
      // 重新授权成功即视为恢复）
      authInvalid = false;
      // refresh = 已初始化机器的授权刷新（token 失效自愈），vault 未动
      toast(stage.refresh ? t("reAuthorized") : stage.joined ? t("joinedVault", stage.records) : t("vaultCreated"));
      if (stage.refresh) {
        // 重新授权只恢复绑定、不对账（解绑时同步记账已清，chips 会显示
        // never）——授权成功后自动补一次同步把状态带回"已同步"；
        // 失败不掩盖授权成功，警告后用户可手动点同步（2026-10-31）
        void invoke("sync_now")
          .catch((e) => toast(String(e), false))
          .finally(() => void renderMain());
      } else {
        void renderMain();
      }
    }
    if (stage.stage === "probe_done") {
      // 新初始化：token 已入库且远端已探测——按 has_data 进入对应
      // 语义的密码表单
      renderPassphrase(stage.has_data);
    }
  };
  const wait = (): void => {
    btn.disabled = true;
    btn.textContent = t("waitingGithub");
    ensureDevicePoll(code)
      .then((stage) => {
        // 旧码 poll 的迟到结果丢弃(页面已是新码)
        if (gen !== devicePollSeq) return;
        apply(stage);
      })
      .catch((e) => {
        if (gen !== devicePollSeq) return;
        toast(syncUserError(String(e)), false);
        // 轮询拿到令牌后令牌即已入库；报错也可能发生在**后续步骤**（建仓/
        // 远端探测）。此时「请在浏览器完成授权后重试」是死胡同：授权并没
        // 有失败，重试只会撞同一个错（2026-11-04 用户建仓 403 实测：App
        // 未安装 → 授权成功但建仓被拒，页面却引导用户无限重试授权）。
        // 用诊断区分两种情况：
        // - 令牌有效 → 视作授权已成功，回到原界面，由引导条按诊断接管
        //   （未安装则渲染安装引导）；
        // - 令牌无效 → 才是真授权失败，留在本页等待重试。
        void invoke<GhDiag>("diagnose_github")
          .then((d) => {
            if (gen !== devicePollSeq) return;
            if (d.authorized && d.tokenValid) {
              authInvalid = false;
              authDiag = d;
              if (onBack) onBack();
              else void renderMain();
            } else {
              status.textContent = t("authFailedHint");
              btn.disabled = false;
              btn.textContent = t("retryAuth");
            }
          })
          .catch(() => {
            // 诊断失败（网络抖动）：退回原「授权失败重试」文案，不静默
            if (gen !== devicePollSeq) return;
            status.textContent = t("authFailedHint");
            btn.disabled = false;
            btn.textContent = t("retryAuth");
          });
      });
  };
  btn.addEventListener("click", () => {
    // 同码轮询进行中点击无新动作；失败后点击 = 重试
    if (devicePolls.has(code)) return;
    wait();
  });
  wait();
}

// ---------------------------------------------------------------------------
// 解锁页
// ---------------------------------------------------------------------------

/**
 * 解锁页：一律手动输入解锁密码（2026-10-24 决议，主人确认）。
 *
 * GUI 不提供任何形式的一键解锁——应用启动与锁定后的解锁页完全一致：
 * 密码输入框 + Unlock 按钮（Enter 也可提交）。"记住的密码"（TTL）只服务
 * CLI 的免输体验；GUI 里成功输入后仍会按 TTL 写入记忆（供 CLI 使用）。
 *
 * @param afterLock 是否因锁定（而非启动）进入解锁页，影响提示文案
 */
/**
 * 钥匙串授权引导面板（D57，2026-11-13 升级为图文形态）：模拟 macOS
 * 授权弹窗的内联 SVG 示意图 + 说明文字——告诉用户系统会弹什么、点哪个
 * 按钮（「始终允许」高亮 + 虚线圈强调）。按钮文字随界面语言渲染。
 */
function keychainGuide(): HTMLElement {
  const wrap = el("div", "kc-guide");
  const title = el("p", "kc-guide-title", t("keychainGuideTitle"));
  const body = el("p", "kc-guide-body", t("keychainGuideBody"));
  const svgWrap = el("div", "kc-guide-svg");
  svgWrap.innerHTML = keychainGuideSvg();
  wrap.append(title, svgWrap, body);
  return wrap;
}

/** 按钮行（对齐 macOS 真实 ACL 弹窗的排布，2026-11-13 实拍修正）：
 * 「始终允许」在左侧独立位（? 帮助钮之后），「拒绝/允许」右对齐成组。
 * 宽度按实际渲染估算：CJK 全角 ~11px、ASCII ~6.5px，内边距 20——保证
 * 文字不撑满按钮（原公式按 7px/字估算，中文按钮被撑爆）。 */
function keychainGuideBtnRow(): Array<{ x: number; w: number; label: string; hot?: boolean }> {
  const w = (s: string) => {
    let text = 0;
    for (const ch of s) text += ch.charCodeAt(0) > 0xff ? 11 : 6.5;
    return Math.ceil(text) + 20;
  };
  const deny = t("keychainGuideDeny");
  const allow = t("keychainGuideAllow");
  const always = t("keychainGuideAlways");
  const wA = w(deny);
  const wB = w(allow);
  const wC = w(always);
  return [
    { x: 86, w: wC, label: always, hot: true },
    { x: 280 - wA - wB - 10, w: wA, label: deny },
    { x: 280 - wB, w: wB, label: allow },
  ];
}

/** 授权弹窗示意图 SVG（对齐 macOS 真实 ACL 弹窗布局，2026-11-13 按
 * Sequoia/Tahoe 形态修正）：应用图标左侧垂直居中（32px 圆角方块）、
 * 标题+正文在图标右侧、全宽分隔线下按钮行右对齐贴边。「始终允许」
 * 蓝底高亮 + 外围虚线圈为教学标注。 */
function keychainGuideSvg(): string {
  const buttons = keychainGuideBtnRow()
    .map(
      (b) => `
    <rect x="${b.x}" y="80" width="${b.w}" height="24" rx="6" fill="${b.hot ? "#2f5fe0" : "#ffffff"}"${b.hot ? "" : ' stroke="#c8c8ce"'}/>
    <text x="${b.x + b.w / 2}" y="96.5" text-anchor="middle" font-size="10.5" font-family="-apple-system,sans-serif" font-weight="${b.hot ? 600 : 400}" fill="${b.hot ? "#ffffff" : "#3c3c40"}">${b.label}</text>
    ${
      b.hot
        ? `<rect x="${b.x - 5}" y="75" width="${b.w + 10}" height="34" rx="9" fill="none" stroke="#2f5fe0" stroke-width="1.5" stroke-dasharray="4 3" opacity="0.75"/>`
        : ""
    }`,
    )
    .join("");
  return `
<svg viewBox="0 0 300 118" xmlns="http://www.w3.org/2000/svg" role="img">
  <rect x="8" y="4" width="284" height="110" rx="12" fill="#ffffff" stroke="#d9d9de"/>
  <rect x="22" y="20" width="34" height="34" rx="8" fill="#2f5fe0"/>
  <path d="M47.5 31.5l-2 2m-6.8 5.5a4.8 4.8 0 1 1-6.78-6.78 4.8 4.8 0 0 1 6.78 6.78zm0 0L44 32.5" stroke="#fff" stroke-width="1.8" fill="none" stroke-linecap="round"/>
  <rect x="68" y="24" width="158" height="8" rx="4" fill="#48484e"/>
  <rect x="68" y="38" width="198" height="6" rx="3" fill="#c9c9cf"/>
  <rect x="68" y="49" width="176" height="6" rx="3" fill="#c9c9cf"/>
  <line x1="8" y1="70" x2="292" y2="70" stroke="#ececf0"/>
  <circle cx="34" cy="92" r="9" fill="#ececf0"/>
  <text x="34" y="96.5" text-anchor="middle" font-size="11" font-family="-apple-system,sans-serif" font-weight="600" fill="#6c6c72">?</text>
  ${buttons}
</svg>`;
}

function renderUnlock(afterLock: boolean): void {
  // 进入解锁页必然意味着会话已销毁：先关掉可能残留的编辑抽屉
  closeOverlays();
  // 语言切换会经 langToggle 重新走本函数：保留已输入的密码（2026-10-25 修复）
  const keep = snapshotFields();
  $app.replaceChildren();
  const wrap = el("div", "center-wrap");
  const card = el("div", "card init-card");
  const brandH = el("h1", "brand-row");
  brandH.append(icon("key"), el("span", "brand", "Makara"), langToggle(() => renderUnlock(afterLock)));
  card.append(brandH);
  // 钥匙串授权引导（D57）：系统弹窗文案不可自定义，应用内预告补位——
  // 新构建首次打开时以「示意图 + 说明」面板呈现一次，让用户带着预期
  // 去点「始终允许」（2026-11-13 升级：一行提示条 → 图文面板）
  if (keychainHintVisible) {
    card.append(keychainGuide());
  }
  if (afterLock) {
    card.append(el("p", "muted", t("lockedHint")));
  }

  {
    const pass = el("input", "input grow");
    pass.type = "password";
    pass.placeholder = t("unlockPlaceholder");
    pass.dataset.field = "pass";
    const btn = el("button", "btn btn-primary", t("unlock"));
    btn.addEventListener("click", () =>
      void withBusy(btn, t("unlocking"), async () => {
        try {
          await invoke("unlock", { passphrase: pass.value });
          renderMain();
        } catch (e) {
          // 密码错误:清空输入框(2026-11-08 用户需求)——错误的整串
          // 密码对下次输入毫无价值,保留只会逼用户手动清除
          pass.value = "";
          pass.focus();
          toast(syncUserError(String(e)), false);
        }
      }),
    );
    pass.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") btn.click();
    });
    card.append(passField(pass), btn);
    wrap.append(card);
    $app.append(wrap);
    restoreFields(keep);
    pass.focus();
  }
}

// ---------------------------------------------------------------------------
// 主界面
// ---------------------------------------------------------------------------

let search = "";

async function renderMain(): Promise<void> {
  $app.replaceChildren();
  let records: RecordView[] = [];
  // 当前展示集：空搜索 = 全量，否则为后端评分排序结果。
  // 搜索在 core::search（2026-10-26 下沉核心层），前端只发查询收列表
  let visible: RecordView[] = [];
  // 逐键异步查询的竞态守卫：只认最新一次的结果，过期响应直接丢弃
  let searchSeq = 0;
  try {
    records = await invoke<RecordView[]>("list_records");
  } catch (e) {
    toast(syncUserError(String(e)), false);
    renderUnlock(false);
    return;
  }

  const shell = el("div", "shell");

  // 顶栏
  const top = el("div", "topbar");
  const brand = el("div", "topbar-brand");
  brand.append(icon("key"), el("span", "brand", "Makara"));
  const chips = el("div", "chips");
  const syncBtn = el("button", "btn btn-small", t("syncNow"));
  syncBtn.prepend(icon("sync"));
  syncBtn.dataset.tip = t("syncNow");
  // 一键检测全部（2026-10-31）：核心层并发探测，结果按 id 合并进
  // 当前列表后整表刷新；缓存写盘与单条检测同源（list_records 会带回）
  // 窗口宽度跨过列数断点时重排瀑布流（防抖 200ms；applyFilter 重建列表）
  let resizeTimer: number | null = null;
  window.addEventListener("resize", () => {
    if (resizeTimer !== null) clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(() => void applyFilter(), 200);
  });

  const checkAllBtn = el("button", "btn btn-small", t("checkAll"));
  checkAllBtn.prepend(icon("shield"));
  checkAllBtn.dataset.tip = t("checkAll");
  checkAllBtn.addEventListener("click", () =>
    void (async () => {
      if (records.length === 0) return;
      // 过程反馈（2026-10-31 反馈"看不到正在检测"）：①全部 chip 先进入
      // 检测中脉冲态；②后端 Channel 逐条推送，卡片逐个点亮结果；
      // ③按钮显示进度计数；④完成弹分类统计
      checkAllBtn.disabled = true;
      checkAllBtn.textContent = t("checkingAll");
      const total = records.length;
      // 状态写入必须同时覆盖 records 与 visible：搜索激活时 visible 是
      // search_records 返回的后端新对象，只改 records 会让卡片反馈整体
      // 失效（2026-10-31 审查修复 M3）
      const putCheck = (id: string, check: CheckView): void => {
        for (const r of records) if (r.id === id) r.check = check;
        for (const r of visible) if (r.id === id) r.check = check;
      };
      for (const r of records) {
        putCheck(r.id, { status: "checking", label: "checking", http: null });
      }
      await applyFilter();
      let done = 0;
      const progress = new ProgressChannel<{ id: string; status: string; label: string; http: number | null }>();
      progress.onmessage = (c) => {
        done += 1;
        checkAllBtn.textContent = `${t("checkingAll")} ${done}/${total}`;
        putCheck(c.id, { status: c.status, label: c.label, http: c.http });
        void applyFilter();
      };
      try {
        const results = await invoke<
          Array<{ id: string; status: string; label: string; http: number | null }>
        >("check_all_keys", { onProgress: progress.handle });
        const byId = new Map(results.map((c) => [c.id, c]));
        for (const r of records) {
          const c = byId.get(r.id);
          if (c) putCheck(r.id, { status: c.status, label: c.label, http: c.http });
        }
        await refresh();
        // 分类统计（非零项拼接，" · "分隔两语言皆可）
        const cnt: Record<string, number> = {};
        for (const c of results) cnt[c.label] = (cnt[c.label] ?? 0) + 1;
        const parts = Object.entries(cnt)
          .sort((a, b) => b[1] - a[1])
          .map(([label, n]) => `${n} ${checkLabel(label)}`);
        toast(t("checkDone", parts.join(" · ")));
      } catch (e) {
        toast(syncUserError(String(e)), false);
      } finally {
        checkAllBtn.disabled = false;
        checkAllBtn.replaceChildren(icon("shield"), document.createTextNode(t("checkAll")));
      }
    })(),
  );
  const lockBtn = el("button", "btn btn-small btn-ghost", t("lock"));
  lockBtn.prepend(icon("lock"));
  lockBtn.dataset.tip = t("lock");
  const gearBtn = el("button", "btn btn-small btn-ghost", t("settings"));
  gearBtn.prepend(icon("gear"));
  gearBtn.dataset.tip = t("settings");
  top.append(brand, chips, checkAllBtn, syncBtn, lockBtn, gearBtn);

  // 搜索 + 新增（搜索框内嵌放大镜图标）
  const toolbar = el("div", "toolbar");
  const searchWrap = el("div", "search search-wrap");
  const searchIcon = icon("search");
  const searchBox = el("input", "input");
  searchBox.type = "search";
  searchBox.placeholder = t("searchPlaceholder");
  searchBox.value = search;
  searchBox.addEventListener("input", () => {
    search = searchBox.value;
    void applyFilter();
  });

  // 发查询 → 更新 visible → 重渲染。百条级数据后端过滤毫秒返回，
  // 不做防抖；seq 守卫挡住乱序返回的旧结果
  async function applyFilter(): Promise<void> {
    const seq = ++searchSeq;
    const q = search.trim();
    if (!q) {
      visible = records;
      renderList();
      return;
    }
    try {
      const hits = await invoke<RecordView[]>("search_records", { query: q });
      if (seq !== searchSeq) return;
      visible = hits;
    } catch {
      if (seq !== searchSeq) return;
      visible = [];
    }
    renderList();
  }
  searchWrap.append(searchIcon, searchBox);
  const addBtn = el("button", "btn btn-primary", t("addRecord"));
  addBtn.prepend(icon("plus"));
  addBtn.addEventListener("click", () => openDrawer(null, refresh));
  toolbar.append(searchWrap, addBtn);

  const list = el("div", "record-list");
  // 授权失效引导条插槽（2026-11-03）：常驻占位，authInvalid 时填充——
  // 同步/写路径撞认证错误后用户看到的不是转瞬即逝的 toast，而是
  // 持久的「授权已失效 → 重新授权」引导
  const authSlot = el("div", "auth-banner-slot");
  renderAuthBanner(authSlot);
  shell.append(top, toolbar, authSlot, list);
  $app.append(shell);

  function renderList(): void {
    list.replaceChildren();
    const rows = visible;
    if (rows.length === 0) {
      const empty = el("div", "empty");
      const title = el(
        "div",
        "empty-title",
        records.length === 0 ? t("emptyNoRecords") : t("emptyNoMatch"),
      );
      const hint = el(
        "div",
        "small",
        records.length === 0 ? t("emptyAddFirst") : t("emptyTryOther"),
      );
      empty.append(icon("inbox"), title, hint);
      list.classList.remove("cols-host");
      list.append(empty);
      return;
    }
    // 贪心瀑布流（2026-10-31 反馈：grid 行对齐时，同行最高卡片决定
    // 行高，矮卡片下方出现大片空隙）：按序放入当前最矮的列，列内
    // 纵向间隔固定。列数与原 grid 断点一致（卡片最小 420px）。
    // 切出 grid 布局（2026-10-31 修复：record-list 仍是 grid 时，
    // 列容器会被压进第一条 420px 轨道，卡片窄到文字竖排）
    list.classList.add("cols-host");
    const cols = Math.max(
      1,
      Math.min(3, Math.floor((list.clientWidth + 10) / 430)),
    );
    const colEls: HTMLElement[] = [];
    for (let i = 0; i < cols; i++) colEls.push(el("div", "record-col"));
    const colWrap = el("div", "record-cols");
    colWrap.append(...colEls);
    list.append(colWrap);
    for (const r of rows) {
      const card = recordCard(r);
      // 找当前最矮列（未挂载时 offsetHeight 为 0——colWrap 已在 list 内，
      // list 本身在 DOM 中，逐卡插入后实时测量）
      let target = colEls[0];
      let min = target.offsetHeight;
      for (const col of colEls) {
        const h = col.offsetHeight;
        if (h < min) {
          min = h;
          target = col;
        }
      }
      target.append(card);
    }
  }

  function recordCard(r: RecordView): HTMLElement {
    const card = el("div", "record");
    const head = el("div", "record-head");
    // 徽章只显示协议族短名（OpenAI / Anthropic / 自定义名），具体
    // API 形态 hover title 呈现（2026-10-31 反馈：长文案是视觉噪音）
    const badge = el("span", `badge ${protoBadgeClass(r.protocol)}`, protoShort(r.protocol));
    badge.dataset.tip = protoLabel(r.protocol);
    head.append(el("span", "record-name", r.name), badge);

    // 字段级复制（2026-10-25 需求）：base_url 与 api_key 成对使用，
    // 各自的行内提供复制按钮（1Password 的字段复制模式）。
    // key 复制后 30 秒自动清除（敏感）；URL 不清除（非敏感）。
    const copyUrlBtn = el("button", "btn btn-icon btn-inline");
    copyUrlBtn.dataset.tip = t("copyUrl");
    copyUrlBtn.setAttribute("aria-label", t("copyUrl"));
    copyUrlBtn.append(icon("copy"));
    copyUrlBtn.addEventListener("click", () =>
      void withBusy(copyUrlBtn, t("copying"), async () => {
        try {
          await invoke("copy_base_url", { id: r.id });
          toast(t("urlCopied"));
        } catch (e) {
          toast(syncUserError(String(e)), false);
        }
      }),
    );
    const urlRow = el("div", "record-url-row");
    urlRow.append(el("span", "record-url", r.base_url), copyUrlBtn);

    const copyKeyBtn = el("button", "btn btn-icon btn-inline");
    copyKeyBtn.dataset.tip = t("copyKey");
    copyKeyBtn.setAttribute("aria-label", t("copyKey"));
    copyKeyBtn.append(icon("copy"));
    copyKeyBtn.addEventListener("click", () =>
      void withBusy(copyKeyBtn, t("copying"), async () => {
        try {
          await invoke("copy_key", { id: r.id });
          toast(t("copied"));
        } catch (e) {
          toast(syncUserError(String(e)), false);
        }
      }),
    );

    const keyRow = el("div", "record-key");
    keyRow.append(el("code", "mono", r.masked_key), copyKeyBtn);
    // 备注独立成行（2026-10-25 优化）：引用块样式（左竖线）与模型胶囊
    // 在视觉上明确区分（2026-10-31 反馈：两者此前同为 muted 文本行，
    // 区分度不足）
    const notesRow = r.notes ? el("div", "record-notes", r.notes) : null;
    // 模型清单：彩色胶囊（按模型名哈希取色，同一模型永远同色）；
    // 空列表不占位
    const modelsRow =
      r.models && r.models.length > 0 ? el("div", "record-models") : null;
    if (modelsRow) {
      for (const m of r.models) {
        const chip = el("span", `model-chip mc-${modelColorIndex(m)}`);
        chip.append(el("code", "mono", m));
        // 单击胶囊 → 复制模型名（2026-11-11 用户定稿：右键菜单只有复制
        // 一项，不值得绕一层）：剪贴板走后端 copy_text（与授权码同路，
        // 非敏感内容不做 30 秒清除）；右键同效并压掉 WebView 默认菜单
        const copyModel = (): void => {
          void (async () => {
            try {
              await invoke("copy_text", { text: m });
              toast(t("modelCopied"));
            } catch (e) {
              toast(syncUserError(String(e)), false);
            }
          })();
        };
        chip.addEventListener("click", copyModel);
        chip.addEventListener("contextmenu", (ev) => {
          ev.preventDefault();
          copyModel();
        });
        modelsRow.append(chip);
      }
    }
    const foot = el("div", "record-foot");
    const checkChip = el("span", `check check-${r.check ? r.check.status : "none"}`, r.check ? checkLabel(r.check.label) : t("notChecked"));
    // 更新时间同样相对化（2026-10-31 采纳：与 last_sync chip 一致），
    // 悬停看完整本地时间
    const updated = el("span", "muted small", relativeTime(r.updated_at));
    updated.title = fmtLocalFull(new Date(r.updated_at));
    const actions = el("div", "record-actions");
    const checkBtn = el("button", "btn btn-icon");
    checkBtn.dataset.tip = t("checkKey");
    checkBtn.setAttribute("aria-label", t("checkKey"));
    checkBtn.append(icon("shield"));
    checkBtn.addEventListener("click", () =>
      // 网络探测最长 10 秒：busy 态防误判卡死；withBusy 的 finally
      // 同时修复了旧实现里"失败一次后按钮永久禁用"的问题
      void withBusy(checkBtn, t("checking"), async () => {
        try {
          const c = await invoke<CheckView>("check_key", { id: r.id });
          r.check = c;
          refresh();
          toast(t("keyCheck", checkLabel(c.label), c.http ? t("httpCode", c.http) : ""));
        } catch (e) {
          toast(syncUserError(String(e)), false);
        }
      }),
    );
    const editBtn = el("button", "btn btn-icon");
    editBtn.dataset.tip = t("editRecord");
    editBtn.setAttribute("aria-label", t("editRecord"));
    editBtn.append(icon("edit"));
    editBtn.addEventListener("click", () => openDrawer(r, refresh));
    const delBtn = el("button", "btn btn-icon danger");
    delBtn.dataset.tip = t("deleteRecord");
    delBtn.setAttribute("aria-label", t("deleteRecord"));
    delBtn.append(icon("trash"));
    delBtn.addEventListener("click", () => {
      void (async () => {
        if (!(await confirmBox(t("confirmDelete", r.name), t("deleteRecord")))) return;
        await withBusy(delBtn, t("deleting"), async () => {
          try {
            await invoke("delete_record", { id: r.id });
            toast(t("deleted", r.name));
            await refresh();
          } catch (e) {
            if (isAuthError(String(e))) {
          markAuthInvalid();
        } else if (/\bHTTP 404\b|not installed/.test(String(e))) {
          // 404(仓库不可见):合成「未安装」诊断,引导条立即渲染安装
          // 引导,不等异步诊断(2026-11-04 卸载后同步实测)。**只有 404
          // 进此分支**——旧写法 else 吞一切,绑定失效串曾落此处又被
          // 真实诊断顶掉,引导条消失(2026-11-10 用户实测)
          markAuthInvalid(true);
        }
            toast(syncUserError(String(e)), false);
          }
        });
      })();
    });
    actions.append(checkBtn, editBtn, delBtn);
    foot.append(checkChip, updated, actions);
    card.append(head, urlRow, keyRow);
    if (modelsRow) card.append(modelsRow);
    if (notesRow) card.append(notesRow);
    card.append(foot);
    return card;
  }

  async function refresh(): Promise<void> {
    try {
      records = await invoke<RecordView[]>("list_records");
    } catch {
      renderUnlock(false);
      return;
    }
    // records 变了，当前搜索条件要重新应用到新数据
    await applyFilter();
    await refreshChips();
  }

  async function refreshChips(): Promise<void> {
    chips.replaceChildren();
    try {
      const st = await invoke<StatusView>("status");
      // 同步按钮可用性从数据派生（D27）：单机没有远端可对账，
      // 禁用而非点了再报"已是最新"的假成功
      syncBtn.disabled = !st.remote;
      syncBtn.dataset.tip = st.remote ? "" : t("syncStandaloneHint");
      // 绑定失效折叠为未绑定(2026-11-10 用户拍板):rust 侧 status 的
      // remote 已按凭据存在折叠,chip 自然显示「已绑定远端/单机模式」
      const remoteChip = el("span", "chip chip-remote", st.remote ? t("remoteBound") : t("standaloneMode"));
      if (st.pending) chips.append(el("span", "chip chip-warn", t("unpushedPending")));
      if (st.last_sync) {
        // 简洁相对时间（2026-10-31 反馈：完整 ISO 时间戳不可读）；
        // 悬停 title 显示完整本地时间兜底精度
        const chip = el("span", "chip chip-last-sync", t("lastSyncChip", relativeTime(st.last_sync)));
        chip.dataset.tip = fmtLocalFull(new Date(st.last_sync));
        chips.append(chip);
      } else {
        chips.append(el("span", "chip chip-last-sync", t("lastSyncChip", t("lastSyncNever"))));
      }
      chips.append(remoteChip, el("span", "chip chip-count", t("nRecords", st.alive)));
    } catch {
      /* 状态获取失败不影响主界面 */
    }
  }

  // git fetch/merge/push 走网络，可能数秒：busy 文案 + 幂等闸门
  syncBtn.addEventListener("click", () =>
    void withBusy(syncBtn, t("syncing"), async () => {
      try {
        const v = await invoke<SyncView>("sync_now");
        toast(fmtSyncResult(v), !v.push_error);
        await refresh();
      } catch (e) {
        // 认证失败且续期未救场 → 置授权失效态（引导条 + chip）
        if (isAuthError(String(e))) {
          markAuthInvalid();
        } else if (/\bHTTP 404\b|not installed/.test(String(e))) {
          // 404(仓库不可见):合成「未安装」诊断,引导条立即渲染安装
          // 引导,不等异步诊断(2026-11-04 卸载后同步实测)。**只有 404
          // 进此分支**——旧写法 else 吞一切,绑定失效串曾落此处又被
          // 真实诊断顶掉,引导条消失(2026-11-10 用户实测)
          markAuthInvalid(true);
        }
        toast(syncUserError(String(e)), false);
      }
    }),
  );

  lockBtn.addEventListener("click", async () => {
    await invoke("lock");
    // 手动锁定同样清除抽屉草稿（2026-10-26 反馈：锁定即清除是唯一边界，
    // 此前只挂了自动锁回调，手动锁定漏了）
    clearDrawerDraft();
    renderUnlock(true);
  });

  // 设置页的返回/保存回调必须整体重建主界面（renderMain 自带数据拉取）；
  // 不能传 refresh——它只更新本闭包里的节点，而设置页已把主界面 shell 摘出 DOM，
  // 传入 refresh 会导致用户永远停留在设置页。
  gearBtn.addEventListener("click", () => void renderSettings(renderMain));

  await refreshChips();
  // 首次渲染必须经 applyFilter 把 records 同步进 visible（含既有搜索词
  // 重放）——曾在此直接 renderList 渲染初始空 visible，启动/解锁后
  // 列表显示"没有匹配"而数据其实已加载（2026-10-26 用户反馈修复）
  await applyFilter();
}

// ---------------------------------------------------------------------------
// 编辑抽屉
// ---------------------------------------------------------------------------

/** 编辑抽屉的草稿：误触关闭（遮罩/Escape/取消）不丢已填内容，
 * 保存成功或应用锁定时清除（2026-10-26 主人决议"至少保存到锁定"）。
 * 按编辑目标隔离：新增草稿与各记录的编辑草稿互不串扰。
 */
let drawerDraft: {
  editingId: string | null;
  name: string;
  protocol: string;
  custom: string;
  baseUrl: string;
  apiKey: string;
  keyType: string;
  models: string[];
  notes: string;
} | null = null;

function clearDrawerDraft(): void {
  drawerDraft = null;
}


/**
 * 修改解锁密码抽屉（设置页「安全」区块入口，2026-11-03 补齐）：验当前
 * 密码 → 输新密码（带强度条，软提示不拦截）→ 确认后落盘。
 *
 * 绑定远端时显示"其他设备需用新密码"的提示（远端将以新密文覆盖）。
 * 键盘与关闭语义对齐记录抽屉：Escape / 点遮罩关闭（busy 态禁关），
 * 输入框内 Enter 直接保存。
 */
function openPassDrawer(remoteBound: boolean): void {
  if (document.querySelector(".overlay")) return; // 幂等：不叠抽屉
  const overlay = el("div", "overlay");
  const drawer = el("div", "drawer");
  drawer.append(el("h2", "", t("changePassphrase")));
  drawer.append(el("p", "muted", t("changePassTitle")));

  const field = (labelText: string, control: HTMLElement): HTMLElement => {
    const wrap = el("div", "field");
    wrap.append(el("div", "field-label", labelText), control);
    return wrap;
  };

  const oldPass = el("input", "input grow");
  oldPass.type = "password";
  oldPass.placeholder = t("oldPass");
  const newPass = el("input", "input grow");
  newPass.type = "password";
  newPass.placeholder = t("newPass");
  // 新密码强度条（软提示）：紧贴新密码框，空输入隐藏
  const meter = strengthMeter(newPass);
  const newPass2 = el("input", "input grow");
  newPass2.type = "password";
  newPass2.placeholder = t("confirmNewPass");

  const errBox = el("p", "warn hidden");
  const actions = el("div", "drawer-actions");
  const cancel = el("button", "btn", t("cancel"));
  cancel.type = "button";
  const save = el("button", "btn btn-primary", t("save"));
  save.type = "button";
  const closeDrawer = () => overlay.remove();
  cancel.addEventListener("click", closeDrawer);

  save.addEventListener("click", () => {
    // 全量同步校验（一次标出所有错误，焦点落第一个出错框）
    const bads: HTMLInputElement[] = [];
    const bad = (i: HTMLInputElement, msg: string) => {
      fieldError(i, msg);
      bads.push(i);
    };
    if (!oldPass.value) bad(oldPass, t("oldPassRequired"));
    if (!newPass.value) bad(newPass, t("newPassRequired"));
    // 长度硬性限制(8–128 字符,与后端 validate_passphrase 同规则)
    else if ([...newPass.value].length < 8) bad(newPass, t("passTooShort"));
    else if ([...newPass.value].length > 128) bad(newPass, t("passTooLong"));
    else if (newPass.value !== newPass2.value) bad(newPass2, t("passMismatch"));
    if (bads.length > 0) {
      bads[0].focus();
      return;
    }
    errBox.classList.add("hidden");
    // 保存 = 重加密 + 可推送远端（网络），需要一两秒
    void withBusy(save, t("saving"), async () => {
      cancel.disabled = true; // 保存进行中禁止中途关闭，防止状态不清
      try {
        await invoke("change_passphrase", {
          oldPassphrase: oldPass.value,
          newPassphrase: newPass.value,
        });
        toast(t("passChanged"));
        overlay.remove();
      } catch (e) {
        if (isAuthError(String(e))) {
          markAuthInvalid();
        } else if (/\bHTTP 404\b|not installed/.test(String(e))) {
          // 404(仓库不可见):合成「未安装」诊断,引导条立即渲染安装
          // 引导,不等异步诊断(2026-11-04 卸载后同步实测)。**只有 404
          // 进此分支**——旧写法 else 吞一切,绑定失效串曾落此处又被
          // 真实诊断顶掉,引导条消失(2026-11-10 用户实测)
          markAuthInvalid(true);
        }
        errBox.textContent = syncUserError(String(e));
        errBox.classList.remove("hidden");
      } finally {
        cancel.disabled = false;
      }
    });
  });

  overlay.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      if (!save.disabled) closeDrawer();
    } else if (ev.key === "Enter" && (ev.target as HTMLElement).tagName === "INPUT") {
      ev.preventDefault();
      save.click();
    }
  });
  actions.append(cancel, save);

  drawer.append(
    field(t("oldPass"), passField(oldPass)),
    field(t("newPass"), passField(newPass)),
    meter.el,
    field(t("confirmNewPass"), passField(newPass2)),
  );
  if (remoteBound) drawer.append(el("p", "warn", t("passChangeWarn")));
  drawer.append(errBox, actions);
  overlay.append(drawer);
  // 遮罩空白关闭的 mousedown 守卫：与记录抽屉同因（WebView2 密码框
  // 自动填充会产生合成 click，必须确认"先在遮罩按下"）
  let pressedOnOverlay = false;
  overlay.addEventListener("mousedown", (ev) => {
    pressedOnOverlay = ev.target === overlay;
  });
  overlay.addEventListener("click", (ev) => {
    if (ev.target === overlay && pressedOnOverlay && !save.disabled) closeDrawer();
  });
  document.body.appendChild(overlay);
  oldPass.focus();
}

function openDrawer(existing: RecordView | null, onSaved: () => Promise<void>): void {
  // 幂等闸门：已有抽屉打开时忽略（连点 Add/Edit 不会叠出多层抽屉）
  if (document.querySelector(".overlay")) return;
  const overlay = el("div", "overlay");
  const drawer = el("div", "drawer");
  drawer.append(el("h2", "", existing ? t("editTitle") : t("addTitle")));

  // 导入面板已随 D20（2026-10-27）移除——抽屉回归纯表单
  const name = el("input", "input");
  name.placeholder = t("recordName");
  name.value = existing?.name ?? "";

  const protoWrap = el("div", "field-row");
  // 协议选项文本用公司规范名展示，value 保持小写存储标签（自绘下拉）
  const proto = makeSelect(
    [
      { value: "openai", label: "OpenAI Responses (recommended)" },
      { value: "openai-chat", label: "OpenAI Chat (widely compatible)" },
      { value: "anthropic", label: "Anthropic Messages (standard)" },
      { value: "anthropic-complete", label: "Anthropic Complete (deprecated)" },
      { value: "custom", label: t("protoCustomOpt") },
    ],
    existing && !existing.protocol.startsWith("custom") ? existing.protocol : "openai",
    syncCustom,
  );
  const protoCustom = el("input", "input");
  protoCustom.placeholder = t("protoCustomLabel");
  if (existing?.protocol.startsWith("custom:")) {
    protoCustom.value = existing.protocol.slice("custom:".length);
  }
  const isCustom = () => proto.value === "custom";
  function syncCustom(): void {
    protoCustom.style.display = isCustom() ? "" : "none";
  }
  syncCustom();
  protoWrap.append(proto.root, protoCustom);

  const baseUrl = el("input", "input");
  baseUrl.type = "url";
  baseUrl.placeholder = t("baseUrl");
  baseUrl.value = existing?.base_url ?? "";

  // key 输入：默认掩码；新增时直接可输入，编辑时点眼睛取回明文。
  // 纯图标按钮 + hover 提示（2026-10-31 反馈：眼睛符号已含义明确，
  // "查看/隐藏"文字冗余）
  const keyWrap = el("div", "field-row");
  const key = el("input", "input grow");
  key.type = "password";
  key.placeholder = t("apiKey");
  key.autocomplete = "off";
  const reveal = el("button", "btn btn-icon");
  reveal.type = "button";
  let revealed = false;
  const syncRevealLabel = () => {
    reveal.replaceChildren(icon(revealed ? "eyeOff" : "eye"));
    const label = t(revealed ? "hidePass" : "showPass");
    reveal.dataset.tip = label;
    reveal.setAttribute("aria-label", label);
  };
  reveal.addEventListener("click", async () => {
    if (existing && !revealed) {
      try {
        key.value = await invoke<string>("reveal_key", { id: existing.id });
      } catch (e) {
        toast(syncUserError(String(e)), false);
        return;
      }
    }
    revealed = !revealed;
    key.type = revealed ? "text" : "password";
    syncRevealLabel();
  });
  syncRevealLabel();
  keyWrap.append(key, reveal);
  if (existing) {
    // 掩码占位：保存时若原样未动，后端保留原 key；点 Reveal 可查看，输入新值则替换
    key.value = existing.masked_key;
    key.dataset.placeholderMode = "masked";
  }

  const notes = el("input", "input");
  notes.placeholder = t("notesOpt");
  notes.value = existing?.notes ?? "";

  // 模型列表编辑器（D21 models 字段）：chip 逐项展示 + 输入框添加 +
  // ×删除 + 「获取」自动填充。apply 时 Qwen 整列表成条目、Codex/Claude
  // 取第一个。chosen 是唯一事实源，chip 与保存值都从它渲染。
  let chosen: string[] = [...(existing?.models ?? [])];
  const modelsWrap = el("div", "models-editor");
  const modelsLabel = el("div", "field-label", t("modelsOpt"));
  const chipBox = el("div", "chip-box");
  const modelsInput = el("input", "input grow");
  modelsInput.placeholder = t("modelsHint");
  const addBtn = el("button", "btn btn-small", t("addModel"));
  addBtn.type = "button";
  const fetchBtn = el("button", "btn btn-small", t("fetchModels"));
  fetchBtn.type = "button";
  const modelsInputRow = el("div", "field-row models-input-row");
  modelsInputRow.append(modelsInput, addBtn, fetchBtn);
  modelsWrap.append(modelsLabel, chipBox, modelsInputRow);

  // 候选挑选面板（2026-10-31 重设计）：「获取」拉到的清单不再整表塞进
  // 已选（中转站动辄几百个模型），而是展开复选面板让用户勾选——搜索
  // 过滤 + 全选/清空 + 按服务端顺序合并；已选过的候选自动预勾选，手工
  // 添加的、服务端清单里没有的项合并时原样保留
  let candidates: string[] = [];
  const picked = new Set<string>();
  const pickWrap = el("div", "model-pick hidden");
  const pickSearch = el("input", "input");
  pickSearch.placeholder = t("pickSearch");
  pickSearch.type = "search";
  const pickAllBtn = el("button", "btn btn-small", t("pickAll"));
  pickAllBtn.type = "button";
  const pickNoneBtn = el("button", "btn btn-small", t("pickNone"));
  pickNoneBtn.type = "button";
  const pickHead = el("div", "field-row");
  pickHead.append(pickSearch, pickAllBtn, pickNoneBtn);
  const pickList = el("div", "pick-list");
  const pickAdd = el("button", "btn btn-small btn-primary", t("pickAdd", 0));
  pickAdd.type = "button";
  pickWrap.append(pickHead, pickList, pickAdd);
  modelsWrap.append(pickWrap);
  const syncPickAdd = (): void => {
    pickAdd.textContent = t("pickAdd", picked.size);
    pickAdd.disabled = picked.size === 0;
  };
  const renderPickList = (): void => {
    const kw = pickSearch.value.trim().toLowerCase();
    pickList.replaceChildren();
    for (const m of candidates) {
      if (kw && !m.toLowerCase().includes(kw)) continue;
      const row = el("label", picked.has(m) ? "pick-item picked" : "pick-item");
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = picked.has(m);
      cb.addEventListener("change", () => {
        if (cb.checked) picked.add(m);
        else picked.delete(m);
        row.classList.toggle("picked", cb.checked);
        syncPickAdd();
      });
      row.append(cb, el("code", "mono", m));
      pickList.append(row);
    }
    syncPickAdd();
  };
  pickSearch.addEventListener("input", renderPickList);
  // Enter/Escape 不冒泡（2026-10-31 审查修复：抽屉的"INPUT 内 Enter=保存"
  // 会提前落盘并丢掉未合并的勾选）；Enter 语义对齐 modelsInput = 添加所选
  pickSearch.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      ev.stopPropagation();
      if (picked.size > 0) pickAdd.click();
    } else if (ev.key === "Escape") {
      ev.stopPropagation();
    }
  });
  // 全选 = 勾选当前过滤可见的候选（跨过滤累积，不碰被过滤掉的勾选态）
  pickAllBtn.addEventListener("click", () => {
    const kw = pickSearch.value.trim().toLowerCase();
    for (const m of candidates) {
      if (!kw || m.toLowerCase().includes(kw)) picked.add(m);
    }
    renderPickList();
  });
  pickNoneBtn.addEventListener("click", () => {
    picked.clear();
    renderPickList();
  });
  pickAdd.addEventListener("click", () => {
    chosen = chosen
      .filter((m) => !candidates.includes(m))
      .concat(candidates.filter((m) => picked.has(m)));
    renderChips();
    pickWrap.classList.add("hidden");
  });

  /** 重渲染 chip 区（chosen → DOM）；空列表显示占位提示。 */
  const renderChips = () => {
    chipBox.replaceChildren();
    if (chosen.length === 0) {
      chipBox.append(el("span", "models-empty", t("modelsEmpty")));
      return;
    }
    chosen.forEach((m, i) => {
      const chip = el("span", "model-chip");
      chip.append(el("code", "mono", m));
      const del = el("button", "chip-del");
      del.type = "button";
      del.textContent = "×";
      del.setAttribute("aria-label", `${t("removeModel")}: ${m}`);
      del.addEventListener("click", () => {
        chosen.splice(i, 1);
        renderChips();
      });
      chip.append(del);
      chipBox.append(chip);
    });
  };
  /** 输入框内容解析入列（中英逗号分隔均可），清空输入框。 */
  const addFromInput = () => {
    for (const m of modelsInput.value.split(/[,，]/)) {
      const v = m.trim();
      if (v && !chosen.includes(v)) chosen.push(v);
    }
    modelsInput.value = "";
    renderChips();
  };
  addBtn.addEventListener("click", addFromInput);
  modelsInput.addEventListener("keydown", (ev) => {
    // Enter/逗号 都能添加；不冒泡到抽屉的 Enter=保存（列表语义）
    if (ev.key === "Enter" || ev.key === "," || ev.key === "，") {
      ev.preventDefault();
      ev.stopPropagation();
      addFromInput();
    }
  });

  const fetchBtnHandler = () =>
    void withBusy(fetchBtn, t("fetchingModels"), async () => {
      // key 取值：编辑态掩码占位（未 Reveal）→ 用后端已存的真实 key
      //（reveal_key 与 fetch_models 同为后端命令，明文不落前端持久层）；
      // 其余情况用输入框现值
      let keyVal = key.value.trim();
      if (existing && key.dataset.placeholderMode === "masked") {
        try {
          keyVal = await invoke<string>("reveal_key", { id: existing.id });
        } catch (e) {
          toast(syncUserError(String(e)), false);
          return;
        }
      }
      if (!baseUrl.value.trim() && !keyVal) {
        toast(t("fetchModelsNeedBoth"), false);
        return;
      }
      if (!baseUrl.value.trim()) {
        fieldError(baseUrl, t("baseUrlRequired"));
        return;
      }
      if (!keyVal) {
        fieldError(key, t("apiKeyRequired"));
        return;
      }
      try {
        const list = await invoke<string[]>("fetch_models", {
          protocol: isCustom() ? protoCustom.value : proto.value,
          baseUrl: baseUrl.value.trim(),
          apiKey: keyVal,
        });
        if (list.length === 0) {
          toast(t("fetchModelsEmpty"), false);
          return;
        }
        // 展开候选面板供挑选（不再整表塞进已选）；已选中的候选预勾选
        candidates = list;
        picked.clear();
        for (const m of chosen) {
          if (candidates.includes(m)) picked.add(m);
        }
        pickSearch.value = "";
        pickWrap.classList.remove("hidden");
        renderPickList();
      } catch (e) {
        toast(syncUserError(String(e)), false);
      }
    });
  fetchBtn.addEventListener("click", fetchBtnHandler);
  renderChips();

  const errBox = el("p", "warn hidden");

  // 恢复草稿（同目标：新增或同一条记录的编辑），key 的可见状态一并还原
  if (drawerDraft && drawerDraft.editingId === (existing?.id ?? null)) {
    name.value = drawerDraft.name;
    proto.value = drawerDraft.protocol;
    protoCustom.value = drawerDraft.custom;
    syncCustom();
    baseUrl.value = drawerDraft.baseUrl;
    key.value = drawerDraft.apiKey;
    key.type = drawerDraft.keyType as "text" | "password";
    if (existing && key.value !== existing.masked_key) delete key.dataset.placeholderMode;
    chosen = [...drawerDraft.models];
    renderChips();
    notes.value = drawerDraft.notes;
  }

  const actions = el("div", "drawer-actions");
  const cancel = el("button", "btn", t("cancel"));
  const save = el("button", "btn btn-primary", t("save"));
  // 关闭前把已填内容存草稿（误触关闭可恢复；保存成功/锁定时清除）
  const captureDraft = () => {
    drawerDraft = {
      editingId: existing?.id ?? null,
      name: name.value,
      protocol: proto.value,
      custom: protoCustom.value,
      baseUrl: baseUrl.value,
      apiKey: key.value,
      keyType: key.type,
      models: [...chosen],
      notes: notes.value,
    };
  };
  const closeDrawer = () => {
    captureDraft();
    overlay.remove();
  };
  cancel.addEventListener("click", closeDrawer);

  async function doSave(): Promise<void> {
    // 编辑时 key 字段仍是掩码占位且未改动 → 传空 key，后端保留原 key
    // （2026-10-25 优化：只改备注/名称等不再强制点 Reveal）
    const keyUntouched =
      existing !== null &&
      key.dataset.placeholderMode === "masked" &&
      key.value === existing.masked_key;
    const input: RecordInput = {
      name: name.value,
      protocol: isCustom() ? protoCustom.value : proto.value,
      base_url: baseUrl.value,
      api_key: keyUntouched ? "" : key.value,
      models: chosen,
      notes: notes.value || null,
    };
    try {
      await invoke("save_record", { input, editingId: existing?.id ?? null });
      // 已落盘，草稿使命完成
      if (drawerDraft?.editingId === (existing?.id ?? null)) clearDrawerDraft();
      toast(existing ? t("updated", input.name) : t("added", input.name));
      overlay.remove();
      await onSaved();
    } catch (e) {
      if (isAuthError(String(e))) {
          markAuthInvalid();
        } else if (/\bHTTP 404\b|not installed/.test(String(e))) {
          // 404(仓库不可见):合成「未安装」诊断,引导条立即渲染安装
          // 引导,不等异步诊断(2026-11-04 卸载后同步实测)。**只有 404
          // 进此分支**——旧写法 else 吞一切,绑定失效串曾落此处又被
          // 真实诊断顶掉,引导条消失(2026-11-10 用户实测)
          markAuthInvalid(true);
        }
      errBox.textContent = syncUserError(String(e));
      errBox.classList.remove("hidden");
    }
  }
  save.addEventListener("click", () => {
    // 同步校验先行（不进入 busy 态，用户可立即改正）：
    // 选了 custom 协议必须给标签，否则后端会产生空的 custom: 徽标。
    // key 未动不必校验——掩码占位原样提交，后端识别后保留原 key。
    // 全量校验（2026-10-26 主人要求）：一次标出所有字段的错误，
    // 不逐个报——焦点落在第一个出错的框。key 仅新增模式必填（编辑时
    // 留空 = 保留原 key 的合法语义）
    const bads: Array<HTMLInputElement | HTMLSelectElement> = [];
    const bad = (i: HTMLInputElement | HTMLSelectElement, msg: string) => {
      fieldError(i, msg);
      bads.push(i);
    };
    if (!name.value.trim()) bad(name, t("nameRequired"));
    if (isCustom() && !protoCustom.value.trim()) bad(protoCustom, t("customRequired"));
    if (!baseUrl.value.trim()) bad(baseUrl, t("baseUrlRequired"));
    else if (!isValidHttpUrl(baseUrl.value)) bad(baseUrl, t("invalidUrl"));
    if (existing === null && !key.value.trim()) bad(key, t("apiKeyRequired"));
    if (bads.length > 0) {
      bads[0].focus();
      return;
    }
    errBox.classList.add("hidden");
    // 保存 = 重加密 + git 提交（可能自动推送），需要一两秒
    void withBusy(save, t("saving"), async () => {
      cancel.disabled = true; // 保存进行中禁止中途关闭，防止状态不清
      try {
        await doSave();
      } finally {
        cancel.disabled = false;
      }
    });
  });

  // 键盘语义：Escape 关闭抽屉；在任意输入框内 Enter 直接保存。
  // 保存进行中（save 处于 busy 禁用态）禁止 Escape 关闭——错误会写进
  // 已脱离 DOM 的提示框，用户永远看不到失败原因（2026-10-26 审查修复）。
  overlay.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      if (!save.disabled) closeDrawer();
    } else if (ev.key === "Enter" && (ev.target as HTMLElement).tagName === "INPUT") {
      ev.preventDefault();
      save.click();
    }
  });
  actions.append(cancel, save);

  // 每字段一行 muted 小标签（2026-10-31 反馈：仅 placeholder 在填值后
  // 失去提示）；(optional) 为伴随信息标注，非补充说明
  const field = (labelText: string, control: HTMLElement): HTMLElement => {
    const wrap = el("div", "field");
    wrap.append(el("div", "field-label", labelText), control);
    return wrap;
  };
  drawer.append(
    field(t("nameLabel"), name),
    field(t("protoLabel"), protoWrap),
    field(t("baseUrlLabel"), baseUrl),
    field(t("apiKeyLabel"), keyWrap),
    modelsWrap,
    field(t("notesLabel"), notes),
    errBox,
    actions,
  );
  overlay.append(drawer);
  // 点击遮罩空白处关闭。必须用 mousedown 做守卫：WebView2 对 type=password
  // 会弹浏览器级自动填充建议，选择建议时页面会收到一个合成的 click，落点
  // 可能在遮罩上——没有"先在遮罩按下"的过程，据此过滤，避免抽屉被误关
  // （2026-10-25 反馈"浏览器有弹窗时添加记录页自动退出"）。
  let pressedOnOverlay = false;
  overlay.addEventListener("mousedown", (ev) => {
    pressedOnOverlay = ev.target === overlay;
  });
  overlay.addEventListener("click", (ev) => {
    if (ev.target === overlay && pressedOnOverlay && !save.disabled) {
      captureDraft();
      overlay.remove();
    }
  });
  document.body.appendChild(overlay);
  name.focus();
}

// ---------------------------------------------------------------------------
// 设置页
// ---------------------------------------------------------------------------

async function renderSettings(
  onBack: () => Promise<void>,
  keepEdits = true,
): Promise<void> {
  let s: SettingsView;
  try {
    s = await invoke<SettingsView>("get_settings");
  } catch (e) {
    toast(syncUserError(String(e)), false);
    return;
  }
  // 语言/主题切换会整页重建设置页：先快照未保存的编辑（2026-10-26
  // 审查修复：此前用户改了一半的 remote/数字会被静默回滚）。
  // 解绑等"数据刚被清掉"的重开必须传 keepEdits=false——回填会把
  // 刚清空的地址又填回输入框，看起来像解绑没生效（2026-10-31 修复）
  const keep = keepEdits ? snapshotFields() : {};
  $app.replaceChildren();
  const shell = el("div", "shell settings-shell");
  const top = el("div", "topbar");
  const back = el("button", "btn btn-small", t("back"));
  back.prepend(icon("back"));
  back.addEventListener("click", () => void onBack());
  const settingsBrand = el("div", "topbar-brand");
  settingsBrand.append(icon("gear"), el("span", "brand", t("settings")));
  top.append(settingsBrand, back);

  const body = el("div", "settings");
  const h2Sync = el("h2", "", t("syncSection"));
  // ---- 远端绑定(2026-11-10 重设计):状态与操作分离,不再有
  // 「空 = 单机」的魔法输入框——空值有歧义(没填?故意清空?),且解绑
  // 这种敏感操作不该用清空文本承载。已绑定:只读展示仓库 + 「解绑」
  // (清授权+解绑,回单机);未绑定:「绑定仓库」按钮 → 展开输入表单。
  // 两步(解绑→绑定)与旧一步改绑等价:下次同步走保险合并,数据无损。
  // 重新授权的入口仍只有主界面失效引导条(2026-11-09 定调)。
  const repoRow = el("div", "setting-row");
  // 绑定失效折叠为未绑定(2026-11-10 用户拍板:用户只感知绑定与未绑定
  // 两态,不拿气泡批注失效):bindingBroken 时整页按未绑定渲染——地址
  // 显示「未绑定」、按钮显示「绑定仓库」;重绑走同一授权流程
  const remoteUi = bindingBroken() ? null : s.remote;
  // 显示完整 GitHub 路径(与 CLI `mak remote show` 一致)——用户应清晰
  // 感知 Makara 依赖 GitHub;存储形态保持归一化 owner/name(D27)不变。
  // URL 可单击:经系统浏览器打开仓库页(2026-11-10 用户需求)
  const repoValue = remoteUi
    ? (() => {
        const url = `https://github.com/${s.remote}`;
        const link = el("a", "repo-link", url);
        link.href = url;
        link.addEventListener("click", (ev) => {
          ev.preventDefault();
          // 外链一律系统浏览器(WebView 内导航会离开应用)
          void invoke("open_external", { url }).catch(() => {});
        });
        return link;
      })()
    : el("span", "", t("repoUnbound"));
  if (remoteUi) repoValue.classList.add("repo-value");
  repoRow.append(el("label", "", t("repoLabel")), repoValue);


  const doReauthorize = (btn: HTMLButtonElement): void => {
    void withBusy(btn, t("reauthorizing"), async () => {
      try {
        const stage = await invoke<InitStage>("github_reauthorize");
        if (stage.stage === "device_code") {
          // 来源是设置页:返回设置页;完成后接管分支自动建仓+绑定。
          // 先刷新诊断再重开——失效标注/按钮按新状态渲染,否则旧诊断
          // (authorized:false)会让页面继续呈现失效态
          showDeviceCode(stage.user_code, stage.verification_uri, () => {
            void refreshAuthDiag().then(() => void renderSettings(onBack));
          });
        }
      } catch (e) {
        toast(syncUserError(String(e)), false);
      }
    });
  };
  const syncBtn2 = el(
    "button",
    remoteUi ? "btn btn-small btn-danger" : "btn btn-small",
    remoteUi ? t("unbindGithub") : t("bindRepo"),
  );
  syncBtn2.addEventListener("click", () => {
    if (!remoteUi) {
      // 未绑定：走授权(device flow)——完成后接管分支自动建仓/复用
      // 默认仓库(用户名/myapikey-vault)并写绑定,全程零输入
      // (2026-11-10 用户纠正:绑定不该让用户手动填地址,MakaraApp
      // 透明化 GitHub 层正是产品的承诺)
      doReauthorize(syncBtn2);
      return;
    }
    // 已绑定：解绑（清令牌 + 解绑，一次写清）——确认后执行；
    // 数据层一次写清后重开设置页，从 get_settings 重新派生
    // （状态行变回「未绑定」），主界面 chips 由返回时的 renderMain 重读
    void (async () => {
      if (!(await confirmBox(t("unbindGithubConfirm"), t("unbindGithub")))) return;
      await withBusy(syncBtn2, t("creating"), async () => {
        try {
          await invoke("forget_github_token");
          toast(t("unboundGithub"));
          // 解绑即离开失效态:清标记与旧诊断,否则残留的失效判定会让
          // 未绑定状态继续显示失效标注与重新绑定按钮(2026-11-10)
          authInvalid = false;
          authDiag = null;
          // keepEdits=false：数据层刚清掉绑定，不能把旧地址回填回输入框
          await renderSettings(onBack, false);
        } catch (e) {
          toast(syncUserError(String(e)), false);
        }
      });
    })();
  });

  const h2Backup = el("h2", "", t("backupSection"));
  // ---- 备份与迁移（D47）：与 CLI export/import 同语义同格式。导出 =
  // 两道确认（明文泄露警示，对齐 CLI 双确认）→ Downloads 时间戳文件名；
  // 导入 = WebView 原生文件选择器（零依赖，不用 dialog 插件）→ 确认 →
  // 后端按 id LWW 合并（core import_records），零变化直接提示
  const exportRow = el("div", "setting-row");
  const exportBtn = el("button", "btn btn-small", t("exportBtn"));
  exportBtn.type = "button";
  exportBtn.addEventListener("click", () => {
    void (async () => {
      if (!(await confirmBox(t("exportConfirm"), t("exportBtn")))) return;
      await withBusy(exportBtn, t("saving"), async () => {
        // 另存为对话框（D49）：取消 = 放弃本次导出；默认名 makara-export-<时间戳>
        const d = new Date();
        const pad = (n: number): string => String(n).padStart(2, "0");
        const ts = `${d.getFullYear()}${pad(d.getMonth() + 1)}${pad(d.getDate())}-${pad(d.getHours())}${pad(d.getMinutes())}${pad(d.getSeconds())}`;
        const target = await saveDialog({
          title: t("exportBtn"),
          defaultPath: `makara-export-${ts}.json`,
          filters: [{ name: "JSON", extensions: ["json"] }],
        });
        if (!target) return;
        try {
          const v = await invoke<{ path: string; count: number }>("export_records", {
            path: target,
          });
          toast(t("exportedPath", v.path));
        } catch (e) {
          toast(syncUserError(String(e)), false);
        }
      });
    })();
  });
  exportRow.append(el("label", "", t("exportLabel")), exportBtn);

  const importRow = el("div", "setting-row");
  const importBtn = el("button", "btn btn-small", t("importBtn"));
  importBtn.type = "button";
  importBtn.addEventListener("click", () => {
    // WebView 原生文件选择器：选中后读文本直接交给后端合并（导入是
    // 无损操作——按 id 只增改不删除，无需确认，与 CLI import 一致）
    const inp = document.createElement("input");
    inp.type = "file";
    inp.accept = ".json,application/json";
    inp.addEventListener("change", () => {
      void (async () => {
        const f = inp.files?.[0];
        if (!f) return;
        const json = await f.text();
        await withBusy(importBtn, t("saving"), async () => {
          try {
            const v = await invoke<{ added: number; updated: number; backup_path: string | null }>(
              "import_records",
              { json },
            );
            if (v.added + v.updated === 0) {
              toast(t("importedNone"));
            } else {
              toast(t("importedSummary", String(v.added), String(v.updated)));
              if (v.backup_path) {
                toast(t("importedBackup", v.backup_path));
              }
            }
          } catch (e) {
            toast(syncUserError(String(e)), false);
          }
        });
      })();
    });
    inp.click();
  });
  importRow.append(el("label", "", t("importLabel")), importBtn);

  const h2Behavior = el("h2", "", t("behaviorSection"));
  // ---- 安全（2026-11-03 补齐 GUI 改解锁密码入口）：行式小按钮，点击开
  // 抽屉表单（验旧密码 + 新密码强度条）；区块紧跟同步——改密码会以新
  // 密文覆盖远端，语义上与同步强相关
  const h2Security = el("h2", "", t("securitySection"));
  const passRow = el("div", "setting-row");
  const passBtn = el("button", "btn btn-small", t("changePassphrase"));
  passBtn.type = "button";
  passBtn.addEventListener("click", () => openPassDrawer(!!remoteUi));
  passRow.append(passBtn);

  const autoPushRow = el("div", "setting-row");
  const autoPush = el("input");
  autoPush.type = "checkbox";
  autoPush.checked = s.auto_push;
  autoPushRow.append(el("label", "", t("autoPush")), autoPush);
  autoPush.addEventListener("change", () => {
    const msg = autoPush.checked ? t("savedAutoPushOn") : t("savedAutoPushOff");
    void persistSettings(msg);
  });

  // 外观区（D14）：主题与语言均为本机偏好（localStorage），切换即时生效，
  // 不走"Save settings"——它们与 config.toml 无关，立即重渲染本页以新外观呈现
  const h2Appearance = el("h2", "", t("appearanceSection"));
  const mkSelect = (
    label: string,
    opts: [string, string][],
    value: string,
    onChange: (v: string) => void,
  ): HTMLElement => {
    const row = el("div", "setting-row");
    // 自绘下拉（2026-10-31 三处 select 统一）：固定宽度对齐原 .num 形态
    const dd = makeSelect(
      opts.map(([v, text]) => ({ value: v, label: text })),
      value,
      onChange,
    );
    dd.root.style.width = "160px";
    row.append(el("label", "", label), dd.root);
    return row;
  };
  // 空闲自动锁定:D32 统一后用预设档位下拉(2026-11-04,连续数字输入
  // +「填 0」提示已废)。永不 = 记住副本不过期;存量手改 config 的值
  // 显示时就近归档,保存后即精确落档。
  const LOCK_PRESETS = [5, 10, 15, 30, 60, 240, 480, 0];
  const lockLabel = (m: number): string =>
    m === 0 ? t("lockNever") : m >= 60 ? t("lockHours", m / 60) : t("lockMinutesOpt", m);
  const nearestPreset = (m: number): number => {
    let best = LOCK_PRESETS[0]!;
    for (const p of LOCK_PRESETS) {
      if (Math.abs(p - m) < Math.abs(best - m)) best = p;
    }
    return best;
  };
  let lockMinutes = nearestPreset(s.auto_lock_minutes);
  const lockRow = mkSelect(
    t("autoLock"),
    LOCK_PRESETS.map((p) => [String(p), lockLabel(p)] as [string, string]),
    String(lockMinutes),
    (v) => {
      lockMinutes = Number(v);
      void persistSettings(t("savedLock", lockLabel(lockMinutes)));
    },
  );

  const themeRow = mkSelect(
    t("themeLabel"),
    [
      ["light", t("themeLight")],
      ["dark", t("themeDark")],
      ["system", t("themeSystem")],
    ],
    prefs.theme,
    (v) => {
      prefs.theme = v as ThemePref;
      savePrefs();
      applyTheme();
      // 提示带新值:值文案按选项表回查(明亮/暗黑/跟随系统)
      const themeText = { light: t("themeLight"), dark: t("themeDark"), system: t("themeSystem") }[
        v as ThemePref
      ];
      toast(t("savedTheme", themeText));
      void renderSettings(onBack);
    },
  );
  const langRow = mkSelect(
    t("langLabel"),
    [
      ["en", "English"],
      ["zh", "中文"],
    ],
    prefs.lang,
    (v) => {
      prefs.lang = v as Lang;
      savePrefs();
      applyLang();
      toast(t("savedLang", v === "en" ? "English" : "中文"));
      void renderSettings(onBack); // 立即以新语言重渲染本页(提示随重建后 toast 显示)
    },
  );

  // ---- 日志（2026-11-05 用户需求）：错误详情落文件不进界面,级别可控;
  // 「打开日志文件夹」直达排查现场
  let logLevel = s.log_level || "info"; // 旧 mock 种子/异常数据兜底
  const LOG_PRESETS: [string, string][] = [
    ["off", t("logOff")],
    ["error", t("logError")],
    ["info", t("logInfo")],
    ["debug", t("logDebug")],
  ];
  const logLevelRow = mkSelect(
    t("logLevel"),
    LOG_PRESETS,
    logLevel,
    (v) => {
      logLevel = v;
      const label = LOG_PRESETS.find(([k]) => k === v)?.[1] ?? v;
      void persistSettings(t("savedLogLevel", label));
    },
  );

  // ---- 日志区（2026-11-05 用户需求）：级别可控 + 文件夹直达;
  // 错误详情在日志文件里,界面只保留简短现象
  const h2Logs = el("h2", "", t("logSection"));


  // ---- 即时保存（D33）：设置页无「保存设置」按钮，每项配置改完即生效 ----
  // 远端绑定改由独立的绑定表单保存(见上),不在此函数
  async function persistSettings(savedToast: string): Promise<void> {
    try {
      await invoke("set_settings", {
        settings: {
          remote: s.remote,
          tombstone_days: s.tombstone_days,
          auto_push: autoPush.checked,
          auto_lock_minutes: lockMinutes,
          log_level: logLevel,
        },
      });
      toast(savedToast);
    } catch (e) {
      toast(syncUserError(String(e)), false);
    }
  }

  // ---- 更新（D16）：当前版本 + 检查 + 破坏性守卫 ----
  const h2Update = el("h2", "", t("updateSection"));
  const updateRow = el("div", "setting-row");
  const versionLabel = el("span", "small muted", "");
  const updateResult = el("p", "small hidden");
  updateResult.style.margin = "0";
  updateResult.style.gridColumn = "1 / -1";
  const checkBtn = el("button", "btn btn-small", t("checkUpdate"));
  checkBtn.addEventListener("click", () =>
    void withBusy(checkBtn, t("checkingUpdate"), async () => {
      try {
        const v = await invoke<UpdateView>("check_update");
        versionLabel.textContent = t("currentVersion", v.current_version);
        // 先清掉上一轮结果（升级按钮 / 破坏性警告），避免重复检查时
        // 按钮重复 append、旧警告残留（2026-10-26 审查修复）
        updateRow.querySelectorAll("button:not(:first-of-type)").forEach((n) => n.remove());
        const prevWarn = updateResult.nextElementSibling;
        if (prevWarn?.classList.contains("update-warn")) prevWarn.remove();
        updateResult.classList.remove("hidden");
        updateResult.className = "small";
        if (!v.available) {
          updateResult.textContent = t("upToDate");
          return;
        }
        updateResult.textContent = t("newVersionAvail", v.version ?? "");
        if (v.breaking) {
          // 破坏性更新：不给一键升级，警告紧跟检查结果展示（同区）
          const warn = el("p", "warn update-warn", t("breakingWarn"));
          warn.style.margin = "6px 0 0";
          warn.style.gridColumn = "1 / -1";
          updateResult.after(warn);
          return;
        }
        const upgradeBtn = el("button", "btn btn-small btn-primary", t("upgradeRestart"));
        upgradeBtn.addEventListener("click", () =>
          void withBusy(upgradeBtn, t("upgrading"), async () => {
            try {
              // 成功路径在安装完成后由应用重启接管，不会走到这里
              await invoke("apply_update");
            } catch (e) {
              toast(syncUserError(String(e)), false);
            }
          }),
        );
        updateRow.append(upgradeBtn);
      } catch (e) {
        toast(syncUserError(String(e)), false);
      }
    }),
  );
  // 初始版本号（离线也显示）
  void invoke<string>("app_version")
    .then((v) => (versionLabel.textContent = t("currentVersion", v)))
    .catch(() => {});
  updateRow.append(versionLabel, checkBtn);
  const syncActions = el("div", "field-row");
  syncActions.append(syncBtn2);

  body.append(
    h2Sync,
    repoRow,
    syncActions,
    h2Security,
    passRow,
    h2Backup,
    exportRow,
    importRow,
    h2Behavior,
    lockRow,
    autoPushRow,
    h2Logs,
    logLevelRow,
    h2Appearance,
    themeRow,
    langRow,
    h2Update,
    updateRow,
    updateResult,
  );
  shell.append(top, body);
  $app.append(shell);
  // 回填必须在挂载到 document 之后：restoreFields 走 document 查询，
  // 挂载前调用查不到任何字段（曾在 init/unlock 页踩过同款，这里保持一致）
  restoreFields(keep);
}

boot().catch((e) => {
  $app.replaceChildren(el("div", "center-wrap", el("div", "card", el("p", "warn", String(e)))));
});
