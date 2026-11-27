/**
 * @addiscorp/stdio/telemetry: the optional telemetry client.
 *
 * Nothing here runs unless the app imports this entry and creates a client;
 * the main entry (`@addiscorp/stdio`) never imports it. Wire format:
 * docs/telemetry-wire.md. Runs in browsers, Cloudflare Workers, Node, Bun and
 * React Native; the on-disk spool for Node and Bun is in
 * `@addiscorp/stdio/telemetry/node`.
 */

import { Telemetry, type KeyValueStore, type Spool, type SpoolItem, type TelemetryConfig } from './client.js'

export {
  Telemetry,
  backoffMs,
  fetchTransport,
  retryAfterMs,
  type ConsentGrant,
  type ConsentHandle,
  type ErrorInput,
  type KeyValueStore,
  type MetricOptions,
  type Spool,
  type SpoolItem,
  type TelemetryConfig,
  type TelemetryStats,
  type Transport,
  type TransportRequest,
  type TransportResponse,
} from './client.js'
export { CUSTOMER_ATTR_KEYS, DROP_ATTR, DROP_KEY, URL_KEY, scrubRecord } from './scrub.js'
export * from './wire.js'

/** A client. Throws a TypeError for a config that can never send (an unregistered-looking client id, a malformed key). */
export function createTelemetry(config: TelemetryConfig): Telemetry {
  return new Telemetry(config)
}

/** A store in memory (the default when none is given): the install id lasts one process. */
export function memoryStore(): KeyValueStore {
  const m = new Map<string, string>()
  return { get: (k) => m.get(k) ?? null, set: (k, v) => void m.set(k, v) }
}

/** A browser store on `localStorage`; falls back to memory where storage is blocked. */
export function localStorageStore(): KeyValueStore {
  const fallback = memoryStore()
  const ls = (): Storage | null => {
    try {
      return (globalThis as { localStorage?: Storage }).localStorage ?? null
    } catch {
      return null
    }
  }
  return {
    get(k) {
      try {
        return ls()?.getItem(k) ?? fallback.get(k)
      } catch {
        return fallback.get(k)
      }
    },
    set(k, v) {
      fallback.set(k, v)
      try {
        ls()?.setItem(k, v)
      } catch {
        // Blocked or full: the memory copy stands.
      }
    },
  }
}

/** An AsyncStorage-shaped store (React Native): `getItem` / `setItem`. */
export type AsyncStorageLike = {
  getItem(key: string): Promise<string | null>
  setItem(key: string, value: string): Promise<void>
}

/**
 * A spool on any key-value store (React Native's AsyncStorage, the browser's
 * localStorage): the live records as one JSON value under `key`, at most
 * `maxBytes` (default 1 MiB). Each write replaces the value, so a crash loses
 * at most the write in progress.
 */
export function kvSpool(store: KeyValueStore | AsyncStorageLike, opts: { key?: string; maxBytes?: number } = {}): Spool {
  const key = opts.key ?? 'addis.telemetry.spool'
  const get = (k: string) => ('getItem' in store ? store.getItem(k) : store.get(k))
  const set = (k: string, v: string) => ('setItem' in store ? store.setItem(k, v) : store.set(k, v))
  let live: SpoolItem[] = []
  return {
    maxBytes: opts.maxBytes ?? 1 << 20,
    async load() {
      const raw = await get(key)
      if (!raw) return []
      try {
        const parsed = JSON.parse(raw) as unknown
        live = Array.isArray(parsed) ? (parsed as SpoolItem[]) : []
      } catch {
        live = []
      }
      return live
    },
    async append(items) {
      live = live.concat(items)
      await set(key, JSON.stringify(live))
    },
    async replace(items) {
      live = items.slice()
      await set(key, JSON.stringify(live))
    },
  }
}

/**
 * Browsers: send what is queued with `navigator.sendBeacon` when the page is
 * hidden or unloaded (`pagehide`, `visibilitychange` to hidden). Returns a
 * function that removes the listeners.
 */
export function installPagehide(telemetry: Telemetry, target: EventTarget | undefined = (globalThis as { window?: EventTarget }).window): () => void {
  if (!target || typeof target.addEventListener !== 'function') return () => {}
  const doc = (globalThis as { document?: Document }).document
  const onHide = () => telemetry.beacon()
  const onVisibility = () => {
    if (doc?.visibilityState === 'hidden') telemetry.beacon()
  }
  target.addEventListener('pagehide', onHide)
  doc?.addEventListener('visibilitychange', onVisibility)
  return () => {
    target.removeEventListener('pagehide', onHide)
    doc?.removeEventListener('visibilitychange', onVisibility)
  }
}
