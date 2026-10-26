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
    core: { invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown> };
    event: { listen: (name: string, cb: (e: { payload: unknown }) => void) => Promise<() => void> };
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

/** 监听自动锁事件（浏览器 mock 模式下不触发）。 */
export async function onLocked(cb: () => void): Promise<void> {
  if (tauri) {
    await tauri.event.listen("locked", cb);
  } else {
    await mockListen(cb);
  }
}
