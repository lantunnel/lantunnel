/**
 * The one seam between the shared UI and whichever Client is hosting it.
 *
 * The desktop speaks Tauri's `invoke`/`listen`. A phone speaks a posted
 * message answered by id. Both are the same two verbs, so the UI above this
 * file never learns which one it is running on — and the three Clients cannot
 * drift apart by each re-deciding what a screen says.
 */

export type HostKind = 'desktop' | 'android' | 'ios'

type Pending = {
  resolve: (value: unknown) => void
  reject: (error: Error) => void
}

interface AndroidHost {
  postMessage(payload: string): void
}

interface WebKitHost {
  messageHandlers?: {
    lantunnel?: { postMessage(payload: unknown): void }
  }
}

declare global {
  interface Window {
    __lantunnelAndroid?: AndroidHost
    webkit?: WebKitHost
    /** The host answers a call by id. Never called by the UI itself. */
    __lantunnelResolve?: (id: number, ok: boolean, payload: string) => void
    /** The host pushes an event by name. */
    __lantunnelEmit?: (event: string, payload: string) => void
  }
}

export function hostKind(): HostKind {
  if (window.__lantunnelAndroid) return 'android'
  if (window.webkit?.messageHandlers?.lantunnel) return 'ios'
  return 'desktop'
}

const pending = new Map<number, Pending>()
const listeners = new Map<string, Set<(payload: unknown) => void>>()
let nextCallId = 1

/**
 * Installed once, for both phones.
 *
 * A host that answered on a per-call global would race two calls in flight;
 * the id is what keeps a slow `get_logs` from resolving a fast `get_status`.
 */
function installPostMessageHost() {
  if (window.__lantunnelResolve) return

  window.__lantunnelResolve = (id, ok, payload) => {
    const entry = pending.get(id)
    if (!entry) return
    pending.delete(id)
    let parsed: unknown = null
    try {
      parsed = payload ? JSON.parse(payload) : null
    } catch {
      // A host that cannot serialise its answer is a host error, not a null
      // result: resolving with null here would render an empty screen and say
      // nothing about why.
      entry.reject(new Error(`host sent an unreadable answer: ${payload}`))
      return
    }
    if (ok) entry.resolve(parsed)
    else entry.reject(new Error(typeof parsed === 'string' ? parsed : JSON.stringify(parsed)))
  }

  window.__lantunnelEmit = (event, payload) => {
    const subscribers = listeners.get(event)
    if (!subscribers?.size) return
    let parsed: unknown = payload
    try {
      parsed = payload ? JSON.parse(payload) : null
    } catch {
      // An event payload that will not parse is still worth delivering as the
      // raw line: this is how log lines arrive.
    }
    subscribers.forEach((fn) => fn(parsed))
  }
}

function postToHost(id: number, command: string, args: Record<string, unknown>) {
  const message = JSON.stringify({ id, command, args })
  const android = window.__lantunnelAndroid
  if (android) {
    android.postMessage(message)
    return
  }
  const webkit = window.webkit?.messageHandlers?.lantunnel
  if (webkit) {
    webkit.postMessage(message)
    return
  }
  throw new Error('no host bridge is attached')
}

let desktopApi: Promise<typeof import('@tauri-apps/api/core')> | null = null
let desktopEvent: Promise<typeof import('@tauri-apps/api/event')> | null = null

export async function invoke<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  if (hostKind() === 'desktop') {
    desktopApi ??= import('@tauri-apps/api/core')
    const core = await desktopApi
    return core.invoke<T>(command, args)
  }
  installPostMessageHost()
  const id = nextCallId++
  return new Promise<T>((resolve, reject) => {
    pending.set(id, { resolve: resolve as (value: unknown) => void, reject })
    try {
      postToHost(id, command, args)
    } catch (error) {
      pending.delete(id)
      reject(error instanceof Error ? error : new Error(String(error)))
    }
  })
}

export type Unlisten = () => void

export async function listen<T>(event: string, fn: (payload: T) => void): Promise<Unlisten> {
  if (hostKind() === 'desktop') {
    desktopEvent ??= import('@tauri-apps/api/event')
    const events = await desktopEvent
    return events.listen<T>(event, (e) => fn(e.payload))
  }
  installPostMessageHost()
  const subscribers = listeners.get(event) ?? new Set()
  const handler = fn as (payload: unknown) => void
  subscribers.add(handler)
  listeners.set(event, subscribers)
  return () => {
    subscribers.delete(handler)
  }
}

/**
 * Choosing a `.peer` file.
 *
 * The desktop owns a file dialog in the webview; a phone cannot, so it asks
 * its host to run the platform picker and hand back what it imported. Same
 * call either way, so the Connection tab has one button, not two branches.
 */
export async function pickPeerProfile<T>(): Promise<T | null> {
  if (hostKind() !== 'desktop') {
    return invoke<T | null>('pick_peer_profile')
  }
  const { open } = await import('@tauri-apps/plugin-dialog')
  const selected = await open({
    multiple: false,
    directory: false,
    filters: [{ name: 'Lantunnel Peer Profile', extensions: ['peer'] }],
  })
  if (typeof selected !== 'string') return null
  return invoke<T>('import_peer_profile', { path: selected })
}
