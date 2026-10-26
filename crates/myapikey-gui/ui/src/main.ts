/**
 * GUI 主体：视图路由（初始化 / 解锁 / 主界面 / 设置）与全部交互。
 *
 * 语言规范（AGENTS.md）：面向用户的文案一律英文。
 * 界面层不包含任何加密/同步逻辑——全部经 bridge 调后端 command。
 */

import { invoke, onLocked } from "./bridge";

// ---------------------------------------------------------------------------
// 与 Rust 侧对应的数据形状
// ---------------------------------------------------------------------------

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
  notes: string | null;
  updated_at: string;
  check: CheckView | null;
}

interface GuiState {
  initialized: boolean;
  unlocked: boolean;
  has_remembered: boolean;
  git_ok: boolean;
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
  ahead: number;
  behind: number;
  last_sync: string | null;
  alive: number;
}

interface SettingsView {
  remote: string | null;
  tombstone_days: number;
  auto_push: boolean;
  remember_days: number;
  auto_lock_minutes: number;
}

type InitStage =
  | { stage: "done"; joined: boolean; records: number; refresh?: boolean }
  | { stage: "device_code"; user_code: string; verification_uri: string };

interface RecordInput {
  name: string;
  protocol: string;
  base_url: string;
  api_key: string;
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
function isValidRemoteUrl(t: string): boolean {
  const v = t.trim();
  if (!v || /\s/.test(v)) return false;
  if (v.includes("://")) return true;
  return v.startsWith("/") || v.startsWith("\\\\") || /^[a-zA-Z]:/.test(v);
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
  unlockPlaceholder: ["Master passphrase", "主密码"],
  lockedHint: ["Locked. Enter the master passphrase to continue.", "已锁定，输入主密码继续。"],
  cancel: ["Cancel", "取消"],
  save: ["Save", "保存"],
  saving: ["Saving…", "正在保存…"],
  settings: ["Settings", "设置"],
  back: ["Back", "返回"],
  // 初始化
  initSubtitle: ["Set up this device. Choose how to create your sync repository.", "初始化本设备，选择同步仓库的创建方式。"],
  confirmPass: ["Confirm passphrase", "确认主密码"],
  modeGithub: ["GitHub, auto-create a private repo", "GitHub，自动创建私有仓库"],
  modeRemote: ["Existing repository URL", "已有仓库地址"],
  modeSolo: ["Standalone", "单机使用"],
  initWarn: [
    "If you forget the master passphrase, all data is unrecoverable. All devices must use the same passphrase.",
    "忘记主密码将无法恢复数据。所有设备必须使用同一主密码。",
  ],
  createVault: ["Create vault", "创建保险库"],
  creating: ["Creating…", "正在创建…"],
  passMismatch: ["Passphrases do not match", "两次输入的主密码不一致"],
  remoteRequired: ["Remote URL is required", "请填写仓库地址"],
  vaultCreated: ["Vault created", "保险库已创建"],
  joinedVault: ["Joined existing vault, {0} records", "已接入现有保险库，{0} 条记录"],
  // 设备授权
  githubAuth: ["GitHub authorization", "GitHub 授权"],
  stepOpen: ["Open the page below in your browser", "在浏览器打开下方链接"],
  stepEnter: ["Enter the code below", "在页面中输入下方代码"],
  deviceNote: ["The code expires in 15 minutes. This window continues automatically after you authorize.", "代码 15 分钟内有效，授权完成后本窗口会自动继续。"],
  iAuthorized: ["I have authorized", "我已完成授权"],
  copyCode: ["Copy code", "复制授权码"],
  codeCopied: ["Code copied", "已复制授权码"],
  waitingGithub: ["Waiting for GitHub…", "等待 GitHub 确认…"],
  authFailedHint: ["Authorization failed, try again after authorizing in the browser.", "授权失败，请在浏览器完成授权后重试。"],
  // 主界面
  syncNow: ["Sync now", "立即同步"],
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
  // 顶栏状态标签（曾长期硬编码英文，中文界面里格外突兀，2026-10-25 修复）
  remoteBound: ["Remote bound", "已绑定远端"],
  standaloneMode: ["Standalone", "单机模式"],
  nUnpushed: ["{0} unpushed", "{0} 条未推送"],
  nToPull: ["{0} to pull", "{0} 条待拉取"],
  nRecords: ["{0} records", "{0} 条记录"],
  emptyNoRecords: ["No records yet", "还没有记录"],
  emptyAddFirst: ["Add the first one, it stays encrypted on this device.", "添加第一条，数据在本机加密存储。"],
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
  deleting: ["Deleting…", "正在删除…"],
  copied: ["API key copied, clears in 30 s", "已复制 API key，30 秒后自动清除"],
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
  reveal: ["Reveal", "查看"],
  hide: ["Hide", "隐藏"],
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
  remoteHint: ["Leave empty for standalone mode", "留空为单机模式"],
  reauthorizeGithub: ["Re-authorize GitHub", "重新授权 GitHub"],
  gitMissingTitle: ["Git is required", "需要安装 Git"],
  gitMissingBody: [
    "MyApiKey stores history and syncs through git. Install it, then come back and retry.",
    "MyApiKey 的历史与同步基于 git。请先安装 git，装好后回到这里重试。",
  ],
  gitOpenDownload: ["Open the download page", "打开下载页面"],
  gitCopyWinget: ["Copy the winget command", "复制 winget 命令"],
  gitWingetCopied: ["Copied: winget install Git.Git", "已复制：winget install Git.Git"],
  gitRetry: ["Installed, retry", "我已安装，重试"],
  reauthorizing: ["Opening GitHub authorization…", "正在打开 GitHub 授权…"],
  reAuthorized: ["GitHub authorization updated. Sync is ready.", "GitHub 授权已更新，可以同步了。"],
  daysUnit: ["days", "天"],
  rememberFor: ["Remember passphrase for", "记住密码时长"],
  daysNeverHint: ["0 = never remember", "填 0 为永不记住密码"],
  autoLock: ["Auto-lock after idle", "空闲自动锁定"],
  minutesUnit: ["minutes", "分钟"],
  minutesOffHint: ["0 = disable auto-lock", "填 0 为关闭自动锁定"],
  autoPush: ["Auto-push after changes", "修改后自动推送"],
  themeLabel: ["Theme", "主题"],
  themeLight: ["Light", "明亮"],
  themeDark: ["Dark", "暗黑"],
  themeSystem: ["Follow system", "跟随系统"],
  langLabel: ["Language", "语言"],
  saveSettings: ["Save settings", "保存设置"],
  settingsSaved: ["Settings saved", "设置已保存"],
  // 设置页 · 更新（D16 应用内更新）
  updateSection: ["Update", "更新"],
  currentVersion: ["Version {0}", "当前版本 {0}"],
  checkUpdate: ["Check for updates", "检查更新"],
  checkingUpdate: ["Checking…", "正在检查…"],
  upToDate: ["You are on the latest version.", "已是最新版本。"],
  notChecked: ["not checked", "未校验"],
  // 校验状态（后端 CheckStatus.label 为英文，前端按键映射成双语）
  checkValid: ["valid", "有效"],
  checkInvalid: ["invalid", "无效"],
  checkNoPerm: ["no permission", "无权限"],
  checkRateLimited: ["rate limited", "限流"],
  checkUnverifiable: ["unverifiable", "无法验证"],
  invalidUrl: [
    "Must start with http:// or https:// followed by a host.",
    "需以 http:// 或 https:// 开头并带主机名。",
  ],
  nameRequired: ["Record name must not be empty.", "记录名称不能为空。"],
  baseUrlRequired: ["Base URL must not be empty.", "Base URL 不能为空。"],
  apiKeyRequired: ["API key must not be empty.", "API key 不能为空。"],
  invalidRemote: [
    "Expected https://… or an absolute local path.",
    "需为 https://… 网址或本地绝对路径。",
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
    btn.title = tip;
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
  if (p === "openai") return "OpenAI";
  if (p === "anthropic") return "Anthropic";
  return p;
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

async function boot(): Promise<void> {
  await onLocked(() => {
    clearDrawerDraft();
    renderUnlock(true);
    toast(t("autoLocked"));
  });
  const st = await invoke<GuiState>("gui_state");
  // git 预检：缺失直接进安装引导页（历史与同步都依赖它）
  if (!st.git_ok) {
    renderGitMissing();
    return;
  }
  if (!st.initialized) renderInit();
  else if (st.unlocked) renderMain();
  else if (st.has_remembered) {
    // 上次以解锁状态退出（锁定会清除记住的密码，走不到这里）——
    // 记住的密码仍在有效期内：静默恢复解锁，直接进主界面。
    // 2026-10-25 决议：启动状态跟随用户上次的状态。
    if (await invoke<boolean>("unlock_remembered")) renderMain();
    else renderUnlock(false);
  } else renderUnlock(false);
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
 * 与解锁页的主密码框统一用本按钮（2026-10-25 修复"点击一次后消失"）。
 * 眼睛为纯图标按钮，语义由 title / aria-label 悬停提示给出。
 */
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
  toggle.title = label;
  toggle.setAttribute("aria-label", label);
}

// ---------------------------------------------------------------------------
// 首次初始化
// ---------------------------------------------------------------------------

/** git 缺失引导页：说明 + 下载入口 + 重试。装好前主流程不进入。 */
function renderGitMissing(): void {
  closeOverlays();
  $app.replaceChildren();
  const wrap = el("div", "center-wrap");
  const card = el("div", "card init-card");
  const brand = el("h1", "brand-row");
  brand.append(icon("key"), el("span", "brand", "MyApiKey"));
  card.append(brand, el("p", "warn", t("gitMissingTitle")), el("p", "muted", t("gitMissingBody")));
  const openBtn = el("button", "btn btn-primary", t("gitOpenDownload"));
  openBtn.addEventListener("click", () => {
    const a = el("a", "link", "https://git-scm.com/download/win") as HTMLAnchorElement;
    a.href = "https://git-scm.com/download/win";
    a.target = "_blank";
    a.click();
  });
  const copyBtn = el("button", "btn", t("gitCopyWinget"));
  copyBtn.addEventListener("click", async () => {
    try {
      await invoke("copy_text", { text: "winget install Git.Git" });
      toast(t("gitWingetCopied"));
    } catch (e) {
      toast(String(e), false);
    }
  });
  const retryBtn = el("button", "btn", t("gitRetry"));
  retryBtn.addEventListener("click", () => {
    void boot();
  });
  const actions = el("div", "init-actions");
  actions.style.display = "grid";
  actions.style.gap = "8px";
  actions.append(openBtn, copyBtn, retryBtn);
  card.append(actions);
  wrap.append(card);
  $app.append(wrap);
}

function renderInit(): void {
  // 语言切换会经 langToggle 重新走本函数：先把已填内容快照下来，重建后回填
  const keep = snapshotFields();
  $app.replaceChildren();
  const wrap = el("div", "center-wrap");
  const card = el("div", "card init-card");

  const brand = el("h1", "brand-row");
  brand.append(icon("key"), el("span", "brand", "MyApiKey"), langToggle(renderInit));
  card.append(brand);
  card.append(el("p", "muted", t("initSubtitle")));

  const pass = el("input", "input grow");
  pass.type = "password";
  pass.placeholder = t("unlockPlaceholder");
  pass.dataset.field = "pass";
  const pass2 = el("input", "input grow");
  pass2.type = "password";
  pass2.placeholder = t("confirmPass");
  pass2.dataset.field = "pass2";

  const mode = el("select", "input");
  mode.dataset.field = "mode";
  mode.append(
    Object.assign(el("option"), { value: "github", textContent: t("modeGithub") }),
    Object.assign(el("option"), { value: "remote", textContent: t("modeRemote") }),
    Object.assign(el("option"), { value: "solo", textContent: t("modeSolo") }),
  );
  const remoteUrl = el("input", "input");
  remoteUrl.type = "url";
  remoteUrl.placeholder = "https://github.com/<you>/<repo>.git";
  remoteUrl.dataset.field = "remoteUrl";
  // 回填可能改变 mode 的选中项，可见性同步必须可重复调用（不能只在 change 事件里做）
  const syncRemote = () => {
    remoteUrl.style.display = mode.value === "remote" ? "" : "none";
  };
  mode.addEventListener("change", syncRemote);
  syncRemote();

  const warn = el("p", "warn", t("initWarn"));
  const btn = el("button", "btn btn-primary", t("createVault"));

  async function doInit(): Promise<void> {
    if (!pass.value || pass.value !== pass2.value) {
      toast(t("passMismatch"), false);
      return;
    }
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
        passphrase: pass.value,
        remote: mode.value === "remote" ? remoteUrl.value.trim() : null,
        githubRepo: mode.value === "github" ? "myapikey-vault" : null,
      });
      if (stage.stage === "device_code") {
        showDeviceCode(stage.user_code, stage.verification_uri);
      } else {
        toast(stage.joined ? t("joinedVault", stage.records) : t("vaultCreated"));
        renderMain();
      }
    } catch (e) {
      toast(String(e), false);
    }
  }
  // withBusy 负责禁用与等待文案（Argon2 派生 + git 初始化需要一两秒）
  btn.addEventListener("click", () => void withBusy(btn, t("creating"), doInit));
  pass2.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") btn.click();
  });

  card.append(passField(pass), passField(pass2), mode, remoteUrl, warn, btn);
  wrap.append(card);
  $app.append(wrap);
  restoreFields(keep);
  // 回填后 mode 可能已是 remote：补一次同步，让远端地址框保持可见
  syncRemote();
}

function showDeviceCode(code: string, uri: string): void {
  $app.replaceChildren();
  const wrap = el("div", "center-wrap");
  const card = el("div", "card init-card");
  const headRow = el("div", "device-head");
  headRow.append(el("h2", "", t("githubAuth")), langToggle(() => showDeviceCode(code, uri)));
  card.append(headRow);
  const steps = el("ol", "steps");
  steps.append(
    el("li", "", t("stepOpen")),
    el("li", "", t("stepEnter")),
  );
  const link = el("a", "link", uri);
  link.href = uri;
  link.target = "_blank";
  steps.children[0].appendChild(link);
  // 授权码行：大字框 + 行内复制按钮（2026-10-25 需求；码 15 分钟有效，
  // 非长期敏感值，复制后不做延时清除）
  const codeEl = el("div", "device-code", code);
  const copyCode = el("button", "btn btn-icon btn-inline");
  copyCode.title = t("copyCode");
  copyCode.setAttribute("aria-label", t("copyCode"));
  copyCode.append(icon("copy"));
  copyCode.addEventListener("click", () =>
    void withBusy(copyCode, t("copying"), async () => {
      try {
        await invoke("copy_text", { text: code });
        toast(t("codeCopied"));
      } catch (e) {
        toast(String(e), false);
      }
    }),
  );
  const codeRow = el("div", "code-row");
  codeRow.append(codeEl, copyCode);
  const note = el("p", "muted", t("deviceNote"));
  const btn = el("button", "btn btn-primary", t("iAuthorized"));
  const status = el("p", "muted", "");
  // 轮询最长到设备码过期（分钟级）：busy 态防重复点击，状态行说明在等什么
  btn.addEventListener("click", () =>
    void withBusy(btn, t("waitingGithub"), async () => {
      try {
        const stage = await invoke<InitStage>("init_device_authorized");
        if (stage.stage === "done") {
          // refresh = 已初始化机器的授权刷新（token 失效自愈），vault 未动
          toast(stage.refresh ? t("reAuthorized") : stage.joined ? t("joinedVault", stage.records) : t("vaultCreated"));
          void renderMain();
        }
      } catch (e) {
        toast(String(e), false);
        status.textContent = t("authFailedHint");
      }
    }),
  );
  card.append(steps, codeRow, note, btn, status);
  wrap.append(card);
  $app.append(wrap);
}

// ---------------------------------------------------------------------------
// 解锁页
// ---------------------------------------------------------------------------

/**
 * 解锁页：一律手动输入主密码（2026-10-24 决议，主人确认）。
 *
 * GUI 不提供任何形式的一键解锁——应用启动与锁定后的解锁页完全一致：
 * 密码输入框 + Unlock 按钮（Enter 也可提交）。"记住的密码"（TTL）只服务
 * CLI 的免输体验；GUI 里成功输入后仍会按 TTL 写入记忆（供 CLI 使用）。
 *
 * @param afterLock 是否因锁定（而非启动）进入解锁页，影响提示文案
 */
function renderUnlock(afterLock: boolean): void {
  // 进入解锁页必然意味着会话已销毁：先关掉可能残留的编辑抽屉
  closeOverlays();
  // 语言切换会经 langToggle 重新走本函数：保留已输入的密码（2026-10-25 修复）
  const keep = snapshotFields();
  $app.replaceChildren();
  const wrap = el("div", "center-wrap");
  const card = el("div", "card init-card");
  const brandH = el("h1", "brand-row");
  brandH.append(icon("key"), el("span", "brand", "MyApiKey"), langToggle(() => renderUnlock(afterLock)));
  card.append(brandH);
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
          toast(String(e), false);
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
    toast(String(e), false);
    renderUnlock(false);
    return;
  }

  const shell = el("div", "shell");

  // 顶栏
  const top = el("div", "topbar");
  const brand = el("div", "topbar-brand");
  brand.append(icon("key"), el("span", "brand", "MyApiKey"));
  const chips = el("div", "chips");
  const syncBtn = el("button", "btn btn-small", t("syncNow"));
  syncBtn.prepend(icon("sync"));
  syncBtn.title = t("syncNow");
  const lockBtn = el("button", "btn btn-small btn-ghost", t("lock"));
  lockBtn.prepend(icon("lock"));
  lockBtn.title = t("lock");
  const gearBtn = el("button", "btn btn-small btn-ghost", t("settings"));
  gearBtn.prepend(icon("gear"));
  gearBtn.title = t("settings");
  top.append(brand, chips, syncBtn, lockBtn, gearBtn);

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
  shell.append(top, toolbar, list);
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
      list.append(empty);
      return;
    }
    for (const r of rows) list.append(recordCard(r));
  }

  function recordCard(r: RecordView): HTMLElement {
    const card = el("div", "record");
    const head = el("div", "record-head");
    head.append(
      el("span", "record-name", r.name),
      el("span", `badge badge-${r.protocol.startsWith("custom") ? "custom" : r.protocol}`, protoLabel(r.protocol)),
    );

    // 字段级复制（2026-10-25 需求）：base_url 与 api_key 成对使用，
    // 各自的行内提供复制按钮（1Password 的字段复制模式）。
    // key 复制后 30 秒自动清除（敏感）；URL 不清除（非敏感）。
    const copyUrlBtn = el("button", "btn btn-icon btn-inline");
    copyUrlBtn.title = t("copyUrl");
    copyUrlBtn.setAttribute("aria-label", t("copyUrl"));
    copyUrlBtn.append(icon("copy"));
    copyUrlBtn.addEventListener("click", () =>
      void withBusy(copyUrlBtn, t("copying"), async () => {
        try {
          await invoke("copy_base_url", { id: r.id });
          toast(t("urlCopied"));
        } catch (e) {
          toast(String(e), false);
        }
      }),
    );
    const urlRow = el("div", "record-url-row");
    urlRow.append(el("span", "record-url", r.base_url), copyUrlBtn);

    const copyKeyBtn = el("button", "btn btn-icon btn-inline");
    copyKeyBtn.title = t("copyKey");
    copyKeyBtn.setAttribute("aria-label", t("copyKey"));
    copyKeyBtn.append(icon("copy"));
    copyKeyBtn.addEventListener("click", () =>
      void withBusy(copyKeyBtn, t("copying"), async () => {
        try {
          await invoke("copy_key", { id: r.id });
          toast(t("copied"));
        } catch (e) {
          toast(String(e), false);
        }
      }),
    );

    const keyRow = el("div", "record-key");
    keyRow.append(el("code", "mono", r.masked_key), copyKeyBtn);
    // 备注独立成行（2026-10-25 优化）：不再挤在 key 行里，
    // 作为卡片末尾的补充说明展示
    const notesRow = r.notes ? el("div", "notes", r.notes) : null;
    const foot = el("div", "record-foot");
    const checkChip = el("span", `check check-${r.check ? r.check.status : "none"}`, r.check ? checkLabel(r.check.label) : t("notChecked"));
    const updated = el("span", "muted small", r.updated_at);
    const actions = el("div", "record-actions");
    const checkBtn = el("button", "btn btn-icon");
    checkBtn.title = t("checkKey");
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
          toast(String(e), false);
        }
      }),
    );
    const editBtn = el("button", "btn btn-icon");
    editBtn.title = t("editRecord");
    editBtn.setAttribute("aria-label", t("editRecord"));
    editBtn.append(icon("edit"));
    editBtn.addEventListener("click", () => openDrawer(r, refresh));
    const delBtn = el("button", "btn btn-icon danger");
    delBtn.title = t("deleteRecord");
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
            toast(String(e), false);
          }
        });
      })();
    });
    actions.append(checkBtn, editBtn, delBtn);
    foot.append(checkChip, updated, actions);
    card.append(head, urlRow, keyRow);
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
      const remoteChip = el("span", "chip", st.remote ? t("remoteBound") : t("standaloneMode"));
      if (st.ahead > 0) chips.append(el("span", "chip chip-warn", t("nUnpushed", st.ahead)));
      if (st.behind > 0) chips.append(el("span", "chip chip-warn", t("nToPull", st.behind)));
      chips.append(remoteChip, el("span", "chip", t("nRecords", st.alive)));
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
        toast(String(e), false);
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
  notes: string;
} | null = null;

function clearDrawerDraft(): void {
  drawerDraft = null;
}

function openDrawer(existing: RecordView | null, onSaved: () => Promise<void>): void {
  // 幂等闸门：已有抽屉打开时忽略（连点 Add/Edit 不会叠出多层抽屉）
  if (document.querySelector(".overlay")) return;
  const overlay = el("div", "overlay");
  const drawer = el("div", "drawer");
  drawer.append(el("h2", "", existing ? t("editTitle") : t("addTitle")));

  const name = el("input", "input");
  name.placeholder = t("recordName");
  name.value = existing?.name ?? "";

  const protoWrap = el("div", "field-row");
  const proto = el("select", "input");
  const preset = existing && !existing.protocol.startsWith("custom") ? existing.protocol : "";
  proto.append(
    // 选项文本用公司规范名展示，value 保持小写存储标签
    Object.assign(el("option"), { value: "openai", textContent: "OpenAI" }),
    Object.assign(el("option"), { value: "anthropic", textContent: "Anthropic" }),
    Object.assign(el("option"), { value: "custom", textContent: t("protoCustomOpt") }),
  );
  proto.value = preset || "openai";
  const protoCustom = el("input", "input");
  protoCustom.placeholder = t("protoCustomLabel");
  if (existing?.protocol.startsWith("custom:")) {
    protoCustom.value = existing.protocol.slice("custom:".length);
  }
  const isCustom = () => proto.value === "custom";
  const syncCustom = () => (protoCustom.style.display = isCustom() ? "" : "none");
  proto.addEventListener("change", syncCustom);
  syncCustom();
  protoWrap.append(proto, protoCustom);

  const baseUrl = el("input", "input");
  baseUrl.type = "url";
  baseUrl.placeholder = t("baseUrl");
  baseUrl.value = existing?.base_url ?? "";

  // key 输入：默认掩码；新增时直接可输入，编辑时点 Reveal 取回明文
  const keyWrap = el("div", "field-row");
  const key = el("input", "input grow");
  key.type = "password";
  key.placeholder = t("apiKey");
  key.autocomplete = "off";
  const reveal = el("button", "btn btn-small", t("reveal"));
  reveal.prepend(icon("eye"));
  let revealed = false;
  const syncRevealLabel = () => {
    reveal.replaceChildren(icon(revealed ? "eyeOff" : "eye"), document.createTextNode(t(revealed ? "hide" : "reveal")));
  };
  reveal.addEventListener("click", async () => {
    if (existing && !revealed) {
      try {
        key.value = await invoke<string>("reveal_key", { id: existing.id });
      } catch (e) {
        toast(String(e), false);
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
      errBox.textContent = String(e);
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
  // 已脱离 DOM 的提示框，用户永远看不到失败原因（2026-10-26 审查修复）
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

  drawer.append(name, protoWrap, baseUrl, keyWrap, notes, errBox, actions);
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

async function renderSettings(onBack: () => Promise<void>): Promise<void> {
  let s: SettingsView;
  try {
    s = await invoke<SettingsView>("get_settings");
  } catch (e) {
    toast(String(e), false);
    return;
  }
  // 语言/主题切换会整页重建设置页：先快照未保存的编辑（2026-10-26
  // 审查修复：此前用户改了一半的 remote/数字会被静默回滚）
  const keep = snapshotFields();
  $app.replaceChildren();
  const shell = el("div", "shell");
  const top = el("div", "topbar");
  const back = el("button", "btn btn-small", t("back"));
  back.prepend(icon("back"));
  back.addEventListener("click", () => void onBack());
  const settingsBrand = el("div", "topbar-brand");
  settingsBrand.append(icon("gear"), el("span", "brand", t("settings")));
  top.append(settingsBrand, back);

  const body = el("div", "settings");
  const h2Sync = el("h2", "", t("syncSection"));
  const remote = el("input", "input");
  remote.type = "url";
  remote.placeholder = t("remotePlaceholder");
  remote.title = t("remoteHint"); // 细则走 hover 提示
  remote.value = s.remote ?? "";
  remote.dataset.field = "remote";
  // token 失效自愈入口（2026-10-26）：重新走设备授权，完成后可立即同步
  const reauthBtn = el("button", "btn btn-small", t("reauthorizeGithub"));
  reauthBtn.style.justifySelf = "start";
  reauthBtn.addEventListener("click", () =>
    void withBusy(reauthBtn, t("reauthorizing"), async () => {
      try {
        const stage = await invoke<InitStage>("github_reauthorize");
        if (stage.stage === "device_code") {
          showDeviceCode(stage.user_code, stage.verification_uri);
        }
      } catch (e) {
        toast(String(e), false);
      }
    }),
  );

  const h2Behavior = el("h2", "", t("behaviorSection"));
  const mkNum = (
    field: string,
    label: string,
    val: number,
    suffix: string,
    hint?: string,
  ): [HTMLInputElement, HTMLElement] => {
    const row = el("div", "setting-row");
    const num = el("input", "input num");
    num.type = "number";
    num.min = "0";
    num.dataset.field = field;
    num.value = String(val);
    if (hint) num.title = hint; // 细则走 hover 提示，文案本体保持简洁
    row.append(el("label", "", label), num, el("span", "muted", suffix));
    return [num, row];
  };
  // 墓碑保留期已从设置页移除（2026-10-25 主人决议）：它是同步引擎的
  // 内部正确性参数，默认 30 天对所有人足够，暴露只会制造困惑；
  // 原值原样回传，进阶用户仍可改 config.toml 的 tombstone_days。
  const [remember, rememberRow] = mkNum("rememberDays", t("rememberFor"), s.remember_days, t("daysUnit"), t("daysNeverHint"));
  const [lockMin, lockRow] = mkNum("lockMinutes", t("autoLock"), s.auto_lock_minutes, t("minutesUnit"), t("minutesOffHint"));

  const autoPushRow = el("div", "setting-row");
  const autoPush = el("input");
  autoPush.type = "checkbox";
  autoPush.checked = s.auto_push;
  autoPushRow.append(el("label", "", t("autoPush")), autoPush);

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
    const sel = el("select", "input num");
    for (const [v, text] of opts) {
      const o = el("option", undefined, text);
      o.value = v;
      sel.append(o);
    }
    sel.value = value;
    sel.addEventListener("change", () => onChange(sel.value));
    row.append(el("label", "", label), sel);
    return row;
  };
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
      void renderSettings(onBack); // 立即以新语言重渲染本页
    },
  );

  const errBox = el("p", "warn hidden");
  const save = el("button", "btn btn-primary", t("saveSettings"));
  // 数字输入清洗：空串/非数字 → NaN → 0；负数截为 0（后端 u32 不接受负值）
  const numOf = (v: string): number => Math.max(0, Math.floor(Number(v) || 0));
  save.addEventListener("click", () =>
    void withBusy(save, t("saving"), async () => {
      // remote 前置校验（后端 set_remote 仍兜底）：非法时留在本页提示
      const remoteVal = remote.value.trim();
      if (remoteVal && !isValidRemoteUrl(remoteVal)) {
        fieldError(remote, t("invalidRemote"));
        return;
      }
      try {
        await invoke("set_settings", {
          settings: {
            remote: remoteVal || null,
            tombstone_days: s.tombstone_days,
            auto_push: autoPush.checked,
            remember_days: numOf(remember.value),
            auto_lock_minutes: numOf(lockMin.value),
          },
        });
        toast(t("settingsSaved"));
        await onBack();
      } catch (e) {
        errBox.textContent = String(e);
        errBox.classList.remove("hidden");
      }
    }),
  );

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
              toast(String(e), false);
            }
          }),
        );
        updateRow.append(upgradeBtn);
      } catch (e) {
        toast(String(e), false);
      }
    }),
  );
  // 初始版本号（离线也显示）
  void invoke<string>("app_version")
    .then((v) => (versionLabel.textContent = t("currentVersion", v)))
    .catch(() => {});
  updateRow.append(versionLabel, checkBtn);
  body.append(h2Sync, remote, reauthBtn, h2Behavior, rememberRow, lockRow, autoPushRow, h2Appearance, themeRow, langRow, h2Update, updateRow, updateResult, errBox, save);
  shell.append(top, body);
  $app.append(shell);
  // 回填必须在挂载到 document 之后：restoreFields 走 document 查询，
  // 挂载前调用查不到任何字段（曾在 init/unlock 页踩过同款，这里保持一致）
  restoreFields(keep);
}

boot().catch((e) => {
  $app.replaceChildren(el("div", "center-wrap", el("div", "card", el("p", "warn", String(e)))));
});
