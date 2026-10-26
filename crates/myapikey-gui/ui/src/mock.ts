/**
 * 浏览器模式的后端 mock：仅用于无 Tauri 环境下的 UI 开发与自动化测试。
 * 数据存 localStorage，不涉及任何真实加密/同步；解锁密码为初始化时
 * 设置的那个（与真实 vault 行为一致，旧状态退回历史固定值 "mock"）。
 */

interface MockRecord {
  id: string;
  name: string;
  protocol: string;
  base_url: string;
  api_key: string;
  notes: string | null;
  updated_at: string;
  /** 可选的校验结果（手工注入 localStorage 可预览校验 chip 的双语展示）。 */
  check?: { status: string; label: string; http: number | null } | null;
}

const STORE_KEY = "myapikey-mock";

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
    remember_days: number;
    auto_lock_minutes: number;
  };
}

function load(): MockStore {
  const raw = localStorage.getItem(STORE_KEY);
  if (raw) return JSON.parse(raw) as MockStore;
  return {
    initialized: false,
    unlocked: false,
    records: [],
    settings: {
      remote: null,
      tombstone_days: 30,
      auto_push: true,
      remember_days: 7,
      auto_lock_minutes: 10,
    },
  };
}

function save(s: MockStore): void {
  localStorage.setItem(STORE_KEY, JSON.stringify(s));
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
      const pass = String(args?.passphrase ?? "");
      if (pass.length < 4) return err("passphrase too short");
      // GitHub 模式返回设备码阶段，便于在浏览器里目检授权页
      if (args?.github_repo) {
        return ok({ stage: "device_code", user_code: "AB12-CD34", verification_uri: "https://github.com/login/device" } as T);
      }
      s.initialized = true;
      s.unlocked = true;
      s.pass = pass; // 与真实 vault 一致：解锁校验初始化时设置的密码
      save(s);
      return ok({ stage: "done", joined: false, records: 0 } as T);
    }
    case "init_device_authorized":
      // mock：模拟"已初始化机器的授权刷新"完成
      return ok({ stage: "done", joined: false, records: s.records.length, refresh: true } as T);
    case "github_reauthorize":
      return ok({ stage: "device_code", user_code: "AB12-CD34", verification_uri: "https://github.com/login/device" } as T);
    case "unlock": {
      const expected = s.pass ?? "mock"; // 旧 mock 状态未存密码时退回历史固定值
      if (String(args?.passphrase) !== expected) return err("wrong master passphrase, or the vault data is corrupted");
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
    case "list_records": {
      if (!s.unlocked) return err("locked — unlock first");
      return ok(
        s.records.map((r) => ({
          id: r.id,
          name: r.name,
          protocol: r.protocol,
          base_url: r.base_url,
          masked_key: mask(r.api_key),
          notes: r.notes,
          updated_at: r.updated_at,
          check: r.check ?? null,
        })) as T,
      );
    }
    case "save_record": {
      if (!s.unlocked) return err("locked — unlock first");
      const input = args?.input as {
        name: string; protocol: string; base_url: string; api_key: string; notes?: string | null;
      };
      if (!input.name.trim()) return err("name must not be empty");
      const editing = args?.editing_id as string | null;
      if (editing) {
        const r = s.records.find((x) => x.id === editing);
        if (!r) return err(`record not found: ${editing}`);
        // 空 key = 保持原 key（与真实后端一致：掩码占位未动时只改其他字段）
        const keep = { ...input, api_key: input.api_key || r.api_key };
        Object.assign(r, keep, { notes: input.notes || null, updated_at: new Date().toISOString() });
      } else {
        s.records.push({
          id: crypto.randomUUID(),
          ...input,
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
    case "sync_now":
      return ok({ pulled: false, merged: false, pushed: false, push_error: null, alive: s.records.length } as T);
    case "status":
      return ok({
        remote: s.settings.remote,
        ahead: 0,
        behind: 0,
        last_sync: null,
        alive: s.records.length,
      } as T);
    case "get_settings":
      return ok(s.settings as T);
    case "set_settings":
      s.settings = { ...(args?.settings as MockStore["settings"]) };
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
// 子串优先、子序列 3 字符起、URL 剥 scheme 按 / 分词、协议不参与、
// 名称 30 / 域名段 10 / 备注 8 / 路径段 4）
// ---------------------------------------------------------------------------

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

function mockSearchScore(r: { name: string; base_url: string; notes?: string | null }, q: string): number | null {
  if (!q) return 0;
  const segs = r.base_url.toLowerCase().replace(/^https?:\/\//, "").split("/").filter(Boolean);
  const fields: Array<[string, number]> = [
    [r.name.toLowerCase(), 30],
    [r.notes?.toLowerCase() ?? "", 8],
    ...segs.map((sg, i): [string, number] => [sg, i === 0 ? 10 : 4]),
  ];
  let best: number | null = null;
  for (const [text, weight] of fields) {
    if (!text) continue;
    const sc = mockFuzzyScore(text, q);
    if (sc !== null && (best === null || sc + weight > best)) best = sc + weight;
  }
  return best;
}
