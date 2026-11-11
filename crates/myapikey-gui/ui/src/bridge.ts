/**
 * 前后端桥接层。
 *
 * - 在 Tauri 窗口内：走真实的 IPC（`window.__TAURI__.core.invoke`，
 *   由 tauri.conf.json 的 withGlobalTauri 提供，前端零 npm 运行时依赖）；
 * - 在纯浏览器（开发调试 / 自动化测试）：走 mock 后端（mock.ts），
 *   因此整套 UI 流程可以在浏览器里无 Tauri 环境验证。
 */

import { mockInvoke, mockListen } from "./mock";

interface TauriGlobal {
  __TAURI__?: {
    core: {
      invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
      Channel?: new () => unknown;
    };
    event: { listen: (name: string, cb: (e: { payload: unknown }) => void) => Promise<() => void> };
    dialog?: {
      save: (opts: {
        title?: string;
        defaultPath?: string;
        filters?: Array<{ name: string; extensions: string[] }>;
      }) => Promise<string | null>;
    };
  };
}

const tauri = (window as unknown as TauriGlobal).__TAURI__;

/** 调用一个后端命令。 */
export function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (tauri) {
    return tauri.core.invoke(cmd, args) as Promise<T>;
  }
  return mockInvoke<T>(cmd, args);
}

/**
 * 另存为对话框（dialog 插件，D49）：返回用户选定的路径；取消返回
 * `null`。浏览器 mock 模式无 Tauri 环境——返回 `defaultPath` 本身，
 * 让导出流程在 mock 验证中继续走通（路径即 mock 示意值）。
 */
export async function saveDialog(opts: {
  title?: string;
  defaultPath?: string;
  filters?: Array<{ name: string; extensions: string[] }>;
}): Promise<string | null> {
  const dlg = tauri?.dialog;
  if (!dlg) return opts.defaultPath ?? null;
  return dlg.save(opts) as Promise<string | null>;
}

/** 监听自动锁事件（浏览器 mock 模式下不触发）。 */
export async function onLocked(cb: () => void): Promise<void> {
  if (tauri) {
    await tauri.event.listen("locked", cb);
  } else {
    await mockListen(cb);
  }
}

/**
 * 监听启动期授权失效事件（2026-11-03）：后端启动线程主动续期失败
 * （刷新令牌同样失效等）时发出，前端据此显示「授权已失效」引导条。
 * 浏览器 mock 模式无真实令牌，不会触发。
 */
export async function onAuthInvalid(cb: (reason: string) => void): Promise<void> {
  if (tauri) {
    await tauri.event.listen("auth-invalid", (e) => cb(String(e.payload ?? "")));
  }
}

// 批量进度通道（2026-10-31 一键检测全部的过程反馈）：
// Tauri 窗口内包装 __TAURI__.core.Channel，后端每完成一条即推送；
// 浏览器 mock 下是空壳（handle 传普通对象，mock 不认识该参数——
// 由后端全量返回值兜底一次性更新）。

/** 批量进度通道（Tauri 2 ipc::Channel 的前端壳，零 npm 依赖）。 */
export class ProgressChannel<T> {
  private raw: unknown;

  /** 每条进度回调（属性式赋值；Tauri 下逐条触发，mock 下不触发）。 */
  onmessage: ((payload: T) => void) | null = null;

  constructor() {
    const Ctor = tauri?.core.Channel;
    this.raw = Ctor ? new Ctor() : {};
    if (this.raw && "onmessage" in (this.raw as Record<string, unknown>)) {
      (this.raw as { onmessage: unknown }).onmessage = (p: T) => this.onmessage?.(p);
    }
  }

  /** 传给 invoke 的参数值（Tauri Channel 实例或 mock 占位对象）。 */
  get handle(): unknown {
    return this.raw;
  }
}
