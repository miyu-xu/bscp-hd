export type NativeMessage = Record<string, unknown> & { command: string };

declare global {
  interface Window {
    ipc?: { postMessage(message: string): void };
    __hdReceive?: (message: HostMessage) => void;
  }
}

export interface HostMessage {
  type: string;
  payload?: unknown;
}

export function post(message: NativeMessage) {
  window.ipc?.postMessage(JSON.stringify(message));
}
