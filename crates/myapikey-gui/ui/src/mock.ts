/**
 * 浏览器模式的后端 mock：仅用于无 Tauri 环境下的 UI 开发与自动化测试。
 * 数据存 sessionStorage（会话级，明文 key 不落浏览器持久层），
 * 不涉及任何真实加密/同步；解锁密码为初始化时
 * 设置的那个（与真实 vault 行为一致，旧状态退回历史固定值 "mock"）。
 */

interface MockRecord {
  id: string;
  name: string;
  protocol: string;
  base_url: string;
  api_key: string;
  models: string[];
  notes: string | null;
  updated_at: string;
  /** 可选的校验结果（手工注入 localStorage 可预览校验 chip 的双语展示）。 */
  check?: { status: string; label: string; http: number | null } | null;
}

// mock 数据放 sessionStorage（会话级）：开发预览时可能录入真实 key，
// 明文不落浏览器持久层（2026-10-31 审查修复）；mak-mock-* 调试开关
// 仍走 localStorage（跨会话保留）
const STORE_KEY = "myapikey-mock";

function mockStoreGet(key: string): string | null {
  return key === STORE_KEY ? sessionStorage.getItem(key) : localStorage.getItem(key);
}

function mockStoreSet(key: string, value: string): void {
  if (key === STORE_KEY) sessionStorage.setItem(key, value);
  else localStorage.setItem(key, value);
}

interface MockStore {
  initialized: boolean;
  unlocked: boolean;
  /** 初始化时设置的主密码（早期 mock 状态无此字段，解锁时退回 "mock"）。 */
  pass?: string;
  records: MockRecord[];
  settings: {
    remote: string | null;
    tombstone_days: number;
    auto_push: boolean;
    auto_lock_minutes: number;
    /** 文件日志级别——mock 不写文件,仅字段保真供设置页交互。 */
    log_level: string;
    /** 最近一次对账时刻（对齐真实 config.last_sync_at，sync 成功时刷新）。 */
    last_sync: string | null;
  };
}

function load(): MockStore {
  const raw = mockStoreGet(STORE_KEY);
  if (raw) return JSON.parse(raw) as MockStore;
  return {
    initialized: false,
    unlocked: false,
    records: [],
    settings: {
      remote: null,
      tombstone_days: 30,
      auto_push: true,
      auto_lock_minutes: 10,
      log_level: "info",
      last_sync: null,
    },
  };
}

function save(s: MockStore): void {
  mockStoreSet(STORE_KEY, JSON.stringify(s));
}

function mask(key: string): string {
  return key.length <= 8 ? `${key.slice(0, 2)}...` : `${key.slice(0, 4)}...${key.slice(-4)}`;
}

function delay<T>(v: T, ms = 120): Promise<T> {
  return new Promise((r) => setTimeout(() => r(v), ms));
}

let lockedCb: (() => void) | null = null;

/** 供测试脚本手动触发自动锁事件。 */
export async function mockListen(cb: () => void): Promise<void> {
  lockedCb = cb;
  (window as unknown as Record<string, unknown>).__makMockLock = () => lockedCb?.();
}

/**
 * 顶层参数键 camelCase → snake_case 规范化。
 * 真实 Tauri IPC 会自动把 JS 侧的 camelCase 键映射到 Rust 命令的 snake_case 形参，
 * mock 必须做同样转换，否则 editingId 这类多词参数在浏览器模式下会被静默忽略。
 */
function snakeKeys(args?: Record<string, unknown>): Record<string, unknown> | undefined {
  if (!args) return args;
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(args)) {
    out[k.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase()] = v;
  }
  return out;
}

export function mockInvoke<T>(cmd: string, rawArgs?: Record<string, unknown>): Promise<T> {
  const args = snakeKeys(rawArgs);
  const s = load();
  const ok = <T,>(v: T) => delay(v);
  const err = (m: string): Promise<T> =>
    new Promise((_, rej) => setTimeout(() => rej(m), 200));

  switch (cmd) {
    case "gui_state":
      return ok({
        initialized: s.initialized,
        unlocked: s.unlocked,
        has_remembered: s.initialized, // mock：初始化过就允许一键解锁
        git_ok: !localStorage.getItem("mak-mock-no-git"), // 调试开关：置 mak-mock-no-git 可预览 git 缺失引导页
      } as T);
    case "init_start": {
      if (s.initialized) return err("already initialized on this machine");
      // 两阶段（对齐真实后端）：init_start 不再收主密码。
      // solo 模式直接返回探测结果（远端无数据 → 前端显示设置密码表单）；
      // GitHub 模式返回设备码阶段，便于在浏览器里目检授权页。
      if (args?.github_repo) {
        localStorage.setItem("mak-mock-pending-remote", "1");
        return ok({ stage: "device_code", user_code: "AB12-CD34", verification_uri: "https://github.com/login/device" } as T);
      }
      localStorage.removeItem("mak-mock-pending-remote");
      return ok({ stage: "probe_done", has_data: false, repo: "" } as T);
    }
    case "init_device_authorized": {
      // mock：授权完成 → 探测。localStorage 置 mak-mock-remote-has-data
      // 可模拟“远端已有保险库”，预览接入表单（密码须与 mak-mock-remote-pass
      // 一致，默认 "remote"）；置 mak-mock-auth-fail 可模拟授权失败，
      // 预览“重试”按钮路径。已初始化机器 = 授权刷新（对齐真实后端：
      // 返回 Done{refresh:true}，绑定恢复、vault 不动）。
      if (localStorage.getItem("mak-mock-auth-fail")) {
        return err("authorization_pending: waiting for the user to authorize");
      }
      if (s.initialized) {
        // 对齐真实后端 refresh 分支：授权刷新会把 remote 重新指向同步仓库
        s.settings.remote = "you/myapikey-vault";
        save(s);
        return ok({ stage: "done", joined: false, records: s.records.length, refresh: true } as T);
      }
      return ok({
        stage: "probe_done",
        has_data: !!localStorage.getItem("mak-mock-remote-has-data"),
        repo: "you/myapikey-vault",
      } as T);
    }
    case "init_finish": {
      // 与真实 init_finish 对齐：接入模式以密码解密远端（不匹配即拒绝），
      // 新设模式直接以该密码加密空 vault；长度硬性限制同真实后端。
      const pass = String(args?.passphrase ?? "");
      if (localStorage.getItem("mak-mock-remote-has-data")) {
        const expected = localStorage.getItem("mak-mock-remote-pass") ?? "remote";
        if (pass !== expected) {
          return err("cannot decrypt the remote vault with the given passphrase. All devices must use the same master passphrase");
        }
      } else if ([...pass].length < 8) {
        return err("passphrase must be at least 8 characters");
      } else if ([...pass].length > 128) {
        return err("passphrase must be at most 128 characters");
      }
      s.initialized = true;
      s.unlocked = true;
      s.pass = pass; // 与真实 vault 一致：解锁校验初始化/接入时用的密码
      // GitHub 流程收尾：回写远端仓库（对齐真实后端写 config.remote_repo），
      // 使状态页显示 GitHub 模式而非单机模式；solo 流程保持 null。
      if (localStorage.getItem("mak-mock-pending-remote")) {
        s.settings.remote = "you/myapikey-vault";
        localStorage.removeItem("mak-mock-pending-remote");
      }
      save(s);
      return ok({ stage: "done", joined: !!localStorage.getItem("mak-mock-remote-has-data"), records: 0 } as T);
    }
    case "fetch_models":
      // mock：返回三个假模型（真实行为见 core::probe::fetch_models）
      return ok(["GLM-5.2", "GLM-5.1", "GLM-5-Air"] as T);
    case "github_reauthorize":
      return ok({ stage: "device_code", user_code: "AB12-CD34", verification_uri: "https://github.com/login/device" } as T);
    case "forget_github_token":
      // 对齐真实后端：清令牌 + 解绑（一次写清）；返回是否清到了令牌
      if (s.settings.remote) {
        s.settings.remote = null;
        save(s);
      }
      return ok(true as T);
    case "unlock": {
      const expected = s.pass ?? "mock"; // 旧 mock 状态未存密码时退回历史固定值
      if (String(args?.passphrase) !== expected) return err("wrong passphrase, or the vault data is corrupted");
      s.unlocked = true;
      save(s);
      return ok(undefined as T);
    }
    case "unlock_remembered": {
      if (!s.initialized) return ok(false as T);
      s.unlocked = true;
      save(s);
      return ok(true as T);
    }
    case "lock":
      s.unlocked = false;
      save(s);
      return ok(undefined as T);
    case "change_passphrase": {
      if (!s.unlocked) return err("locked — unlock first");
      const expected = s.pass ?? "mock"; // 与 unlock 同源：旧状态退回历史固定值
      if (String(args?.old_passphrase) !== expected) {
        return err("the current passphrase is incorrect");
      }
      const next = String(args?.new_passphrase ?? "");
      // 长度硬性限制同真实后端(8–128 字符)
      if ([...next].length < 8) return err("passphrase must be at least 8 characters");
      if ([...next].length > 128) return err("passphrase must be at most 128 characters");
      s.pass = next;
      save(s);
      return ok(undefined as T);
    }
    case "list_records": {
      if (!s.unlocked) return err("locked — unlock first");
      return ok(
        s.records.map((r) => ({
          id: r.id,
          name: r.name,
          protocol: r.protocol,
          base_url: r.base_url,
          masked_key: mask(r.api_key),
          models: r.models ?? [],
          notes: r.notes,
          updated_at: r.updated_at,
          check: r.check ?? null,
        })) as T,
      );
    }
    case "save_record": {
      if (!s.unlocked) return err("locked — unlock first");
      const input = args?.input as {
        name: string; protocol: string; base_url: string; api_key: string;
        models?: string[] | null; notes?: string | null;
      };
      if (!input.name.trim()) return err("name must not be empty");
      const editing = args?.editing_id as string | null;
      if (editing) {
        const r = s.records.find((x) => x.id === editing);
        if (!r) return err(`record not found: ${editing}`);
        // 空 key = 保持原 key（与真实后端一致：掩码占位未动时只改其他字段）
        const keep = { ...input, api_key: input.api_key || r.api_key };
        Object.assign(r, keep, {
          models: input.models ?? r.models ?? [],
          notes: input.notes || null,
          updated_at: new Date().toISOString(),
        });
      } else {
        s.records.push({
          id: crypto.randomUUID(),
          ...input,
          models: input.models ?? [],
          notes: input.notes || null,
          updated_at: new Date().toISOString(),
        });
      }
      save(s);
      return ok(undefined as T);
    }
    case "delete_record": {
      const id = String(args?.id);
      s.records = s.records.filter((r) => r.id !== id);
      save(s);
      return ok(undefined as T);
    }
    case "export_records": {
      if (!s.unlocked) return err("locked — unlock first");
      // mock：浏览器无法真实写盘，返回一个示意路径（UI 流程验证用）
      return ok({
        path: "C:\\Users\\mock\\Downloads\\makara-export-mock.json",
        count: s.records.length,
      } as T);
    }
    case "import_records": {
      if (!s.unlocked) return err("locked — unlock first");
      // mock：解析数组形状，全部按新增合并进 mock 状态（UI 流程验证用）
      let arr: Array<Record<string, unknown>>;
      const raw = String(args?.json ?? "");
      try {
        const parsed = JSON.parse(raw);
        arr = Array.isArray(parsed) ? parsed : (parsed.records ?? []);
      } catch {
        return err("import file is not a valid record array");
      }
      let added = 0;
      for (const item of arr) {
        const r = item as { id?: string; name?: string };
        if (!r.id || !r.name) continue;
        if (s.records.some((x) => x.id === r.id)) continue;
        s.records.push({
          id: r.id, name: r.name, protocol: "openai",
          base_url: "", api_key: "", models: [], notes: null,
          created_at: new Date().toISOString(), updated_at: new Date().toISOString(),
          deleted: false,
        } as unknown as typeof s.records[number]);
        added += 1;
      }
      save(s);
      return ok({ added, updated: 0, backup_path: null } as T);
    }
    case "reveal_key": {
      if (!s.unlocked) return err("locked — unlock first");
      const r = s.records.find((x) => x.id === args?.id);
      return r ? ok(r.api_key as T) : err(`record not found: ${args?.id}`);
    }
    case "copy_key":
    case "copy_base_url":
      return ok(undefined as T);
    case "check_key":
      return ok({ status: "unverifiable", label: "unverifiable", http: null } as T);
    case "check_all_keys": {
      // mock：全部记录返回"无法验证"（无真实网络）；带 id 供前端合并。
      // 加 900ms 延迟模拟真实探测耗时，让"检测中"脉冲态可感知
      return new Promise((resolve) =>
        setTimeout(
          () =>
            resolve(
              ok(
                s.records.map((r) => ({
                  id: r.id,
                  status: "unverifiable",
                  label: "unverifiable",
                  http: null,
                })) as T,
              ),
            ),
          900,
        ),
      );
    }
    case "sync_now":
      // 对齐真实后端：单机模式没有远端，明确报错而非静默“已是最新”；
      // 成功 = 对账完成，刷新记账时间（对齐 mark_synced 语义）
      if (!s.settings.remote) {
        return err("standalone mode: no remote is bound to sync with. Bind one on the settings page");
      }
      // 调试开关：置 mak-mock-sync-401 可预览授权失效路径（引导条 + chip）
      if (localStorage.getItem("mak-mock-sync-401")) {
        return err(
          'GitHub rejected the request (HTTP 401): { "message": "Bad credentials" } ; automatic token refresh failed too: no refresh token stored — re-authorize GitHub from the settings page',
        );
      }
      // 调试开关：置 mak-mock-sync-404 可预览「仓库不可见」路径——引导条
      // 应显示**安装引导**而非「授权已失效」（2026-11-04 状态错显回归）
      if (localStorage.getItem("mak-mock-sync-404")) {
        return err(
          'repository not found or not accessible by this token (HTTP 404): { "message": "Not Found" }. If you use a GitHub App, check that it is installed and includes this repo.',
        );
      }
      s.settings.last_sync = new Date().toISOString();
      save(s);
      return ok({ pulled: false, merged: false, pushed: false, push_error: null, alive: s.records.length } as T);
    case "status":
      return ok({
        remote: s.settings.remote,
        pending: localStorage.getItem("mak-mock-pending") === "1",
        last_sync: s.settings.last_sync,
        alive: s.records.length,
      } as T);
    case "github_install_url":
      // 与 core 的 APP_SLUG_DEFAULT 同源(浏览器 mock 无后端,这里镜像)
      return ok("https://github.com/apps/makaraapp/installations/new" as T);
    case "diagnose_github":
      // 调试开关:置 mak-mock-app-not-installed 可模拟「App 未安装」
      // (引导安装);置 mak-mock-app-scope-excluded 可模拟「已安装但
      // 范围不含同步仓库」(引导调整仓库范围);置 mak-mock-token-expired
      // 可模拟「令牌失效」(引导重新授权)——2026-11-04 引导条三态验证
      const scopeExcluded = localStorage.getItem("mak-mock-app-scope-excluded") === "1";
      const notInstalled =
        !scopeExcluded && localStorage.getItem("mak-mock-app-not-installed") === "1";
      const tokenExpired = localStorage.getItem("mak-mock-token-expired") === "1";
      return ok({
        authorized: s.initialized,
        tokenValid: !tokenExpired,
        installed: !notInstalled,
        installUrl: "https://github.com/apps/makaraapp/installations/new",
        remoteBound: !!s.settings.remote,
        repoAccessible: s.settings.remote ? !notInstalled && !scopeExcluded : null,
      } as T);
    case "get_settings":
      return ok(s.settings as T);
    case "set_settings":
      // 逐字段合并：前端 SettingsView 不含 mock 自有的 last_sync——
      // 整体替换会让保存设置后"上次同步"清零（2026-10-31 审查采纳）
      s.settings = { ...s.settings, ...(args?.settings as Partial<MockStore["settings"]>) };
      save(s);
      return ok(undefined as T);
    case "search_records": {
      // 浏览器 mock 用 TS 复刻 core::search 的评分（仅供开发预览；
      // 真实实现只有 crates/myapikey-core/src/search.rs 一份，改规则以
      // Rust 为准并同步这份 mock）
      const q = String(args?.query ?? "").trim().toLowerCase();
      const sorted = s.records
        .map((r) => ({ r, score: mockSearchScore(r, q) }))
        .filter((x) => x.score !== null)
        .sort((a, b) => (b.score as number) - (a.score as number))
        .map((x) => x.r);
      return ok((q ? sorted : [...s.records].sort((a, b) => b.updated_at.localeCompare(a.updated_at))) as T);
    }
    case "app_version":
      return ok("0.1.0-mock" as T);
    case "check_update":
      return ok({ current_version: "0.1.0-mock", available: false, version: null, notes: null, breaking: false } as T);
    case "apply_update":
      return ok(undefined as T);
    case "set_ui_lang":
      // 真实后端用于本地化托盘菜单；浏览器 mock 无托盘，直接成功
      return ok(undefined as T);
    case "copy_text":
      return ok(undefined as T);
    default:
      return err(`mock: unknown command ${cmd}`);
  }
}

// ---------------------------------------------------------------------------
// core::search 的 TS 复刻（仅供浏览器 mock，与 Rust 版规则保持一致：
// 子串优先、子序列 3 字符起、URL 剥 scheme 按 / 分词、协议参与匹配
// （2026-10-31 加入，权重 3 最低）、名称 30 / 域名段 10 / 备注 8 / 路径段 4）
// ---------------------------------------------------------------------------


/** 对齐 core Protocol::search_terms：协议的搜索匹配词（小写）。 */
function protoSearchTerms(protocol: string): string {
  if (protocol === "openai") return "openai responses";
  if (protocol === "openai-chat") return "openai openai-chat chat";
  if (protocol === "anthropic") return "anthropic messages";
  if (protocol === "anthropic-complete") return "anthropic anthropic-complete complete";
  const name = protocol.startsWith("custom:") ? protocol.slice("custom:".length) : protocol;
  return `custom ${name.toLowerCase()}`;
}

function mockFuzzyScore(target: string, q: string): number | null {
  const idx = target.indexOf(q);
  if (idx >= 0) return 1000 - Math.min(idx, 500) + (mockWordBoundary(target, idx) ? 100 : 0);
  if (q.length < 3) return null;
  let ti = 0, score = 0, run = 0;
  for (const ch of q) {
    const found = target.indexOf(ch, ti);
    if (found < 0) return null;
    run = found === ti ? run + 1 : 1;
    score += 10 + run * 4 + (mockWordBoundary(target, found) ? 8 : 0) - Math.min(found - ti, 5);
    ti = found + 1;
  }
  return score;
}

function mockWordBoundary(target: string, i: number): boolean {
  return i === 0 || /[-_./: ]/.test(target[i - 1] ?? "");
}

function mockSearchScore(
  r: { name: string; base_url: string; notes?: string | null; protocol: string },
  q: string,
): number | null {
  if (!q) return 0;
  const segs = r.base_url.toLowerCase().replace(/^https?:\/\//, "").split("/").filter(Boolean);
  const fields: Array<[string, number]> = [
    [r.name.toLowerCase(), 30],
    [r.notes?.toLowerCase() ?? "", 8],
    ...segs.map((sg, i): [string, number] => [sg, i === 0 ? 10 : 4]),
    // 协议字段（2026-10-31 加入）：标签 + 完整形态词，权重 3 最低
    [protoSearchTerms(r.protocol), 3],
  ];
  let best: number | null = null;
  for (const [text, weight] of fields) {
    if (!text) continue;
    const sc = mockFuzzyScore(text, q);
    if (sc !== null && (best === null || sc + weight > best)) best = sc + weight;
  }
  return best;
}
