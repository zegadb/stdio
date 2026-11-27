/**
 * The telemetry client (docs/telemetry-wire.md): a bounded in-memory queue,
 * batches by count and interval, one batch in flight, retries with
 * Retry-After and backoff, consent-gated identity, an optional spool.
 *
 * Runtime-agnostic: the network is a Transport (fetch by default), storage is
 * a KeyValueStore and a Spool the app passes in. Nothing here touches a file,
 * and nothing here throws into the app once the client is constructed.
 */

import { scrubRecord } from './scrub.js'
import {
  CLIENT_ID,
  CODE,
  CONSENT_VERSION,
  DEFAULT_ENDPOINT,
  DROP_REASONS,
  ENVIRONMENT,
  HEX32,
  KEY,
  MAX_BATCH_RECORDS,
  MAX_BEACON,
  MAX_BODY,
  MAX_RECORD,
  NAME,
  RELEASE,
  REQUEST_TIMEOUT_MS,
  SCOPE,
  SDK_NAME,
  SDK_VERSION,
  UNIT,
  cleanAttrs,
  text,
  timestamp,
  traceContext,
  utf8Length,
  type Batch,
  type DropReason,
  type Identity,
  type Kind,
  type LogLevel,
  type MetricKind,
  type Platform,
  type RecordType,
  type Resource,
  type TraceContext,
  type WireRecord,
} from './wire.js'

// ── What the app plugs in ────────────────────────────────

/** Somewhere to keep small strings (the install id). Sync or async. */
export interface KeyValueStore {
  get(key: string): string | null | undefined | Promise<string | null | undefined>
  set(key: string, value: string): void | Promise<void>
}

/** One queued record as a spool keeps it: never an identity, only the consent version it was captured under. */
export type SpoolItem = { r: WireRecord; res: Resource; cv: string | null }

/**
 * Where queued records survive a crash or an offline period. `append` is
 * called once per captured record, `replace` with every live record after a
 * batch is accepted, after a consent change, and to compact after drops.
 * `maxBytes` caps the spool: past it, the oldest records are dropped.
 */
export interface Spool {
  readonly maxBytes: number
  load(): SpoolItem[] | Promise<SpoolItem[]>
  append(items: SpoolItem[]): void | Promise<void>
  replace(items: SpoolItem[]): void | Promise<void>
}

export type TransportRequest = { url: string; headers: Record<string, string>; body: Uint8Array | string; timeoutMs: number }
export type TransportResponse = { status: number; retryAfter: string | null }
/** Sends one request; rejects on a network error or a timeout. */
export type Transport = (request: TransportRequest) => Promise<TransportResponse>

export type TelemetryConfig = {
  /** The registered client id (the collector's projects.json `service`), e.g. `ava-desktop`. */
  client: string
  /** The project's key, 32 hex, from `bun run project`. Pass it explicitly; never read from the environment here. */
  key: string
  /** The kind it is registered as; decides what is scrubbed (wire §7). */
  kind: Kind
  /** The app's version. */
  release: string
  /** Detected when left out. */
  platform?: Platform
  os?: { name?: string; version?: string }
  /** A model class (`iPhone`, `Mac`), never a model number or name. */
  deviceClass?: string
  environment?: string
  /** Default `https://telemetry.addis.gg/v1/ingest`. `http://` only to loopback. */
  endpoint?: string
  /** Records per batch, default 100 (at most 500). */
  maxBatch?: number
  /** Milliseconds between scheduled sends, default 10,000. 0: no timer (Workers: call flush()). */
  interval?: number
  /** Queued records, default 1,000. */
  maxQueue?: number
  /** Probability each type is kept, default 1. */
  sampling?: Partial<Record<RecordType, number>>
  /** Keeps the install id; without one it lives for this process. */
  store?: KeyValueStore
  /** Keeps queued records across restarts. */
  spool?: Spool
  /** Default: fetch. */
  transport?: Transport
  /** For tests: the clock (ms since the epoch). */
  now?: () => number
  /** For tests: a source of random bytes. Default: crypto.getRandomValues. */
  randomBytes?: (n: number) => Uint8Array
}

export type ErrorInput = {
  type: string
  message?: string
  stack?: string
  fingerprint?: string
  code?: string
  trace?: TraceContext
}

export type MetricOptions = { unit?: string; attrs?: Record<string, unknown>; trace?: TraceContext }

/** What the user approved. `grantedAt` is when, so a stored approval re-presented after a restart keeps its time. */
export type ConsentGrant = {
  version: string
  grantedAt?: Date | string | number
  identity: { sub: string; deviceName?: string; email?: string }
}

/** Returned by `consent.grant`: the only thing that can stop the identity it attached. */
export interface ConsentHandle {
  readonly version: string
  readonly grantedAt: string
  /** True while this grant is the one in force. */
  readonly active: boolean
  /** Stops sending identity at once and sends `telemetry.consent.revoked`. False when this grant was no longer in force. */
  revoke(): boolean
}

export type TelemetryStats = {
  queued: number
  sent: number
  dropped: Record<DropReason, number>
  lastStatus: number | null
  lastError: string | null
}

const INSTALL_ID_KEY = 'addis.telemetry.install_id'
const BACKOFF_MAX_S = 300

/** The batch's records, its resource and the consent version they were captured under. */
type Formed = { id: string; items: Item[]; extra: WireRecord[]; res: Resource; cv: string | null }
type Item = SpoolItem & { bytes: number }

function hex(bytes: Uint8Array): string {
  let s = ''
  for (const b of bytes) s += b.toString(16).padStart(2, '0')
  return s
}

function detectPlatform(): Platform {
  const g = globalThis as Record<string, unknown> & { navigator?: { product?: string; userAgent?: string }; process?: { versions?: Record<string, string> } }
  if (g.navigator?.product === 'ReactNative') return 'react-native'
  if (g.navigator?.userAgent === 'Cloudflare-Workers' || typeof g.WebSocketPair === 'function') return 'worker'
  if (g.Bun) return 'bun'
  if (g.Deno) return 'deno'
  if (g.process?.versions?.node) return 'node'
  return 'browser'
}

function isLoopback(url: URL): boolean {
  return url.hostname === '127.0.0.1' || url.hostname === '[::1]' || url.hostname === 'localhost'
}

/** Retry-After as milliseconds, clamped to 1–3,600 s (wire §9); null when absent or unreadable. */
export function retryAfterMs(value: string | null, nowMs: number): number | null {
  if (value === null) return null
  const v = value.trim()
  let s: number
  if (/^\d+$/.test(v)) s = Number(v)
  else {
    const at = Date.parse(v)
    if (Number.isNaN(at)) return null
    s = (at - nowMs) / 1000
  }
  return Math.min(3600, Math.max(1, s)) * 1000
}

/** The wait after the n-th consecutive failure (wire §9): d/2 + random(0, d/2), d = min(300 s, 2^(n-1) s). */
export function backoffMs(n: number, random: () => number = Math.random): number {
  const d = Math.min(BACKOFF_MAX_S, 2 ** Math.max(0, n - 1)) * 1000
  return d / 2 + random() * (d / 2)
}

/** Sends with fetch: keepalive when the body is small enough for it, a 10 s timeout. */
export const fetchTransport: Transport = async (req) => {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), req.timeoutMs)
  try {
    const size = typeof req.body === 'string' ? utf8Length(req.body) : req.body.length
    const res = await fetch(req.url, {
      method: 'POST',
      headers: req.headers,
      body: req.body as BodyInit,
      keepalive: size <= MAX_BEACON,
      signal: controller.signal,
    })
    // Drain, so the connection is reused.
    await res.arrayBuffer().catch(() => undefined)
    return { status: res.status, retryAfter: res.headers.get('retry-after') }
  } finally {
    clearTimeout(timer)
  }
}

async function gzip(body: string): Promise<Uint8Array | null> {
  if (typeof CompressionStream !== 'function') return null
  try {
    const stream = new Blob([body]).stream().pipeThrough(new CompressionStream('gzip') as unknown as ReadableWritablePair<Uint8Array, Uint8Array>)
    return new Uint8Array(await new Response(stream).arrayBuffer())
  } catch {
    return null
  }
}

/** Throws on a config that can never send; everything after construction is silent. */
function checkConfig(c: TelemetryConfig): void {
  if (!CLIENT_ID.test(c.client ?? '')) throw new TypeError(`telemetry: client must be a registered client id (${CLIENT_ID})`)
  if (!KEY.test(c.key ?? '')) throw new TypeError('telemetry: key must be the 32-hex key from `bun run project`')
  if (c.kind !== 'service' && c.kind !== 'customer-app') throw new TypeError("telemetry: kind must be 'service' or 'customer-app'")
  if (!RELEASE.test(c.release ?? '')) throw new TypeError(`telemetry: release must match ${RELEASE}`)
  if (c.environment !== undefined && !ENVIRONMENT.test(c.environment)) throw new TypeError(`telemetry: environment must match ${ENVIRONMENT}`)
  const url = new URL(c.endpoint ?? DEFAULT_ENDPOINT)
  if (url.protocol !== 'https:' && !(url.protocol === 'http:' && isLoopback(url))) throw new TypeError('telemetry: endpoint must be https (http only to loopback)')
}

export class Telemetry {
  readonly consent: {
    /** Attach identity from now on; the only way to. */
    grant(grant: ConsentGrant): ConsentHandle
    /** The grant in force, without its identity. */
    readonly current: { version: string; grantedAt: string } | null
  }
  /** Resolves once the install id and the spool are loaded. Records captured before are kept. */
  readonly ready: Promise<void>

  private readonly config: TelemetryConfig
  private readonly kind: Kind
  private readonly url: string
  private readonly beaconUrl: string
  private readonly now: () => number
  private readonly random: (n: number) => Uint8Array
  private readonly transport: Transport
  private readonly maxBatch: number
  private readonly maxQueue: number
  private readonly sampling: Record<RecordType, number>

  private installId: string | null = null
  private resource: Resource | null = null
  private queue: Item[] = []
  private queuedBytes = 0
  /** Captured before `ready`: their resource is set then. */
  private early: { r: WireRecord; cv: string | null }[] = []
  private formed: Formed[] = []
  private inFlight: Promise<boolean> | null = null
  private failures = 0
  private retryAt = 0
  private retryTimer: ReturnType<typeof setTimeout> | null = null
  private timer: ReturnType<typeof setInterval> | null = null
  private pendingDrops = new Map<DropReason, number>()
  private spoolDirty = false
  private spoolChain: Promise<void> = Promise.resolve()
  private spoolPending = 0
  private grant: { version: string; grantedAt: string; identity: Identity; token: object } | null = null
  private closed = false
  private disabled: string | null = null
  private counters: TelemetryStats = {
    queued: 0,
    sent: 0,
    dropped: { queue_full: 0, spool_full: 0, too_large: 0, invalid: 0, refused: 0 },
    lastStatus: null,
    lastError: null,
  }

  constructor(config: TelemetryConfig) {
    checkConfig(config)
    this.config = config
    this.kind = config.kind
    this.url = new URL(config.endpoint ?? DEFAULT_ENDPOINT).toString()
    const beacon = new URL(this.url)
    beacon.search = `?key=${config.key}`
    this.beaconUrl = beacon.toString()
    this.now = config.now ?? Date.now
    this.transport = config.transport ?? fetchTransport
    this.maxBatch = Math.min(MAX_BATCH_RECORDS, Math.max(1, Math.floor(config.maxBatch ?? 100)))
    this.maxQueue = Math.max(1, Math.floor(config.maxQueue ?? 1000))
    const rate = (r: number | undefined) => (typeof r === 'number' && r >= 0 && r <= 1 ? r : 1)
    this.sampling = {
      metric: rate(config.sampling?.metric),
      log: rate(config.sampling?.log),
      error: rate(config.sampling?.error),
      event: rate(config.sampling?.event),
    }
    const random = config.randomBytes ?? secureRandom()
    if (!random) this.disabled = 'no secure random source (React Native: install a crypto.getRandomValues polyfill)'
    this.random = random ?? (() => new Uint8Array(0))

    const self = this
    this.consent = {
      grant: (g) => self.grantConsent(g),
      get current() {
        return self.grant ? { version: self.grant.version, grantedAt: self.grant.grantedAt } : null
      },
    }
    this.ready = this.disabled ? Promise.resolve() : this.load()

    const interval = config.interval ?? 10_000
    if (interval > 0 && !this.disabled) {
      this.timer = setInterval(() => void this.tick(), interval)
      ;(this.timer as { unref?: () => void }).unref?.()
    }
  }

  // ── Capture ─────────────────────────────────────────────

  counter(name: string, value = 1, opts: MetricOptions = {}): void {
    this.metric('counter', name, value, opts)
  }

  gauge(name: string, value: number, opts: MetricOptions = {}): void {
    this.metric('gauge', name, value, opts)
  }

  histogram(name: string, value: number, opts: MetricOptions = {}): void {
    this.metric('histogram', name, value, opts)
  }

  log(level: LogLevel, scope: string, message: string, opts: { trace?: TraceContext } = {}): void {
    this.capture('log', () => {
      if (!['debug', 'info', 'warn', 'error'].includes(level) || !SCOPE.test(scope)) return null
      const m = text(message, 2000)
      if (!m) return null
      return { type: 'log', ts: this.ts(), ...this.trace(opts.trace), level, scope, message: m }
    })
  }

  /** An error: a thrown value (its name, message and stack), or the fields given. */
  error(err: unknown, opts: { fingerprint?: string; code?: string; trace?: TraceContext } = {}): void {
    this.capture('error', () => {
      const input = toErrorInput(err)
      const type = text(input.type, 128)
      if (!type) return null
      const code = opts.code ?? input.code
      const fingerprint = text(opts.fingerprint ?? input.fingerprint, 128)
      const message = text(input.message, 2000)
      const stack = text(input.stack, 16_384)
      return {
        type: 'error',
        ts: this.ts(),
        ...this.trace(opts.trace ?? input.trace),
        error_type: type,
        ...(message ? { message } : {}),
        ...(stack ? { stack } : {}),
        ...(fingerprint ? { fingerprint } : {}),
        ...(code !== undefined && CODE.test(code) ? { code } : {}),
      }
    })
  }

  event(name: string, attrs?: Record<string, unknown>, opts: { trace?: TraceContext } = {}): void {
    this.capture('event', () => {
      if (!this.validName(name)) return null
      const a = cleanAttrs(attrs)
      return { type: 'event', ts: this.ts(), ...this.trace(opts.trace), name, ...(a ? { attrs: a } : {}) }
    })
  }

  // ── Control ─────────────────────────────────────────────

  /** Sends everything queued now. Resolves when the queue is empty or an attempt failed; never rejects. */
  async flush(): Promise<void> {
    if (this.disabled) return
    try {
      await this.ready
      for (let guard = 0; guard < 10_000; guard++) {
        if (this.inFlight) {
          await this.inFlight
          continue
        }
        if (!this.hasWork()) return
        if (!(await this.sendOne(true))) return
      }
    } catch (e) {
      this.counters.lastError = String((e as Error)?.message ?? e)
    }
  }

  /** Stops the timers and flushes. Records captured after are ignored. */
  async shutdown(): Promise<void> {
    if (this.timer) clearInterval(this.timer)
    if (this.retryTimer) clearTimeout(this.retryTimer)
    this.timer = this.retryTimer = null
    await this.flush()
    this.closed = true
    await this.compactSpool()
    await this.spoolChain
  }

  /**
   * Sends what is queued with `navigator.sendBeacon` (or the function given),
   * for `pagehide`: no headers, so the key goes in the query and the body is
   * `text/plain`, at most 60 KiB per beacon. A beacon that was queued counts
   * as sent.
   */
  beacon(send?: (url: string, body: Blob) => boolean): void {
    if (this.disabled || !this.resource) return
    try {
      const beacon = send ?? beaconFunction()
      if (!beacon) return
      for (let guard = 0; guard < 100 && this.hasWork(); guard++) {
        const f = this.formed.length ? this.formed[0] : this.form(MAX_BEACON)
        if (!f) return
        const body = this.serialize(f)
        if (utf8Length(body) > MAX_BEACON) return
        if (!beacon(this.beaconUrl, new Blob([body], { type: 'text/plain;charset=UTF-8' }))) return
        this.formed.shift()
        this.counters.sent += f.items.length + f.extra.length
        this.saveSpool()
      }
    } catch (e) {
      this.counters.lastError = String((e as Error)?.message ?? e)
    }
  }

  /** A new random install id; records captured from now on carry it, and it is kept in the store. */
  async resetInstallId(): Promise<void> {
    if (this.disabled) return
    await this.ready
    this.installId = hex(this.random(16))
    this.resource = this.makeResource()
    try {
      await this.config.store?.set(INSTALL_ID_KEY, this.installId)
    } catch (e) {
      this.counters.lastError = String((e as Error)?.message ?? e)
    }
  }

  stats(): TelemetryStats & { disabled: string | null } {
    return { ...this.counters, queued: this.queue.length + this.early.length + this.formed.reduce((n, f) => n + f.items.length, 0), dropped: { ...this.counters.dropped }, disabled: this.disabled }
  }

  // ── Internals ───────────────────────────────────────────

  private metric(kind: MetricKind, name: string, value: number, opts: MetricOptions): void {
    this.capture('metric', () => {
      if (!this.validName(name) || typeof value !== 'number' || !Number.isFinite(value) || (kind === 'counter' && value < 0)) return null
      if (opts.unit !== undefined && !UNIT.test(opts.unit)) return null
      const a = cleanAttrs(opts.attrs)
      return { type: 'metric', ts: this.ts(), ...this.trace(opts.trace), kind, name, value, ...(opts.unit ? { unit: opts.unit } : {}), ...(a ? { attrs: a } : {}) }
    })
  }

  private validName(name: string): boolean {
    return typeof name === 'string' && name.length <= 128 && NAME.test(name) && !name.startsWith('telemetry.')
  }

  private ts(): string {
    return timestamp(this.now())
  }

  private trace(t: TraceContext | undefined): { trace?: TraceContext } {
    const tc = traceContext(t)
    return tc ? { trace: tc } : {}
  }

  /** Builds, samples, scrubs and queues one record. Never throws. */
  private capture(type: RecordType, build: () => WireRecord | null, reserved = false): void {
    if (this.disabled || this.closed) return
    try {
      const rate = reserved ? 1 : this.sampling[type]
      if (rate < 1 && Math.random() >= rate) return
      const built = build()
      if (!built) return this.drop('invalid', 1)
      if (rate < 1) built.sample_rate = rate
      const record = scrubRecord(built, this.kind)
      if (!record) return
      this.enqueue(record, this.grant?.version ?? null)
    } catch (e) {
      this.counters.lastError = String((e as Error)?.message ?? e)
    }
  }

  private enqueue(r: WireRecord, cv: string | null): void {
    if (utf8Length(JSON.stringify(r)) > MAX_RECORD) return this.drop('too_large', 1)
    if (!this.resource) {
      this.early.push({ r, cv })
      if (this.early.length > this.maxQueue) {
        this.early.shift()
        this.drop('queue_full', 1)
      }
      return
    }
    const item = this.item({ r, res: this.resource, cv })
    this.push([item])
    this.persist(() => this.config.spool!.append([{ r: item.r, res: item.res, cv: item.cv }]))
    if (this.queue.length >= this.maxBatch) void this.pump()
  }

  private item(s: SpoolItem): Item {
    return { ...s, bytes: utf8Length(JSON.stringify(s)) + 1 }
  }

  /**
   * Adds to the back. Past the bounds (records queued and in batches being
   * sent, and the spool's bytes), the oldest record not already in a batch
   * is dropped.
   */
  private push(items: Item[]): void {
    for (const it of items) {
      this.queue.push(it)
      this.queuedBytes += it.bytes
    }
    let formedCount = 0
    let formedBytes = 0
    for (const f of this.formed) {
      formedCount += f.items.length
      for (const it of f.items) formedBytes += it.bytes
    }
    const maxBytes = this.config.spool?.maxBytes ?? Infinity
    while (this.queue.length > 0) {
      const overCount = this.queue.length + formedCount > this.maxQueue
      const overBytes = this.queuedBytes + formedBytes > maxBytes
      if (!overCount && !overBytes) break
      const old = this.queue.shift()!
      this.queuedBytes -= old.bytes
      this.drop(overCount ? 'queue_full' : 'spool_full', 1)
      this.spoolDirty = true
    }
  }

  private drop(reason: DropReason, n: number): void {
    this.counters.dropped[reason] += n
    this.pendingDrops.set(reason, (this.pendingDrops.get(reason) ?? 0) + n)
  }

  private makeResource(): Resource {
    const c = this.config
    const os = c.os ? { ...(text(c.os.name, 32) ? { name: text(c.os.name, 32) } : {}), ...(text(c.os.version, 32) ? { version: text(c.os.version, 32) } : {}) } : undefined
    const deviceClass = text(c.deviceClass, 32)
    return {
      release: c.release,
      platform: c.platform ?? detectPlatform(),
      ...(os && Object.keys(os).length ? { os } : {}),
      ...(deviceClass ? { device_class: deviceClass } : {}),
      install_id: this.installId!,
      sdk: { name: SDK_NAME, version: SDK_VERSION },
      ...(c.environment ? { environment: c.environment } : {}),
    }
  }

  /** Loads the install id and the spool: synchronously when the store and spool are, so nothing waits on a microtask. */
  private load(): Promise<void> {
    const fail = (e: unknown) => {
      this.counters.lastError = String((e as Error)?.message ?? e)
    }
    const afterSpool = (spooled: SpoolItem[] | null | undefined): void => {
      try {
        this.finishLoad(spooled ?? [])
      } catch (e) {
        fail(e)
      }
    }
    const afterId = (stored: string | null | undefined): void | Promise<void> => {
      this.installId = typeof stored === 'string' && HEX32.test(stored) ? stored : null
      if (!this.installId) {
        this.installId = hex(this.random(16))
        try {
          const w = this.config.store?.set(INSTALL_ID_KEY, this.installId)
          if (isThenable(w)) w.then(undefined, fail)
        } catch (e) {
          fail(e)
        }
      }
      let spooled: SpoolItem[] | Promise<SpoolItem[]> | undefined
      try {
        spooled = this.config.spool?.load()
      } catch (e) {
        fail(e)
      }
      if (isThenable(spooled)) return spooled.then(afterSpool, (e) => (fail(e), afterSpool([])))
      afterSpool(spooled)
    }
    try {
      const stored = this.config.store?.get(INSTALL_ID_KEY)
      if (isThenable(stored)) return stored.then(afterId, (e) => (fail(e), afterId(null))).then(() => undefined)
      const r = afterId(stored)
      return isThenable(r) ? r : Promise.resolve()
    } catch (e) {
      fail(e)
      const r = afterId(null)
      return isThenable(r) ? r : Promise.resolve()
    }
  }

  private finishLoad(spooled: SpoolItem[]): void {
    this.resource = this.makeResource()
    const valid = spooled.filter((s) => s && typeof s === 'object' && s.r && s.res && HEX32.test(s.res.install_id ?? ''))
    const early = this.early.map((e) => this.item({ r: e.r, res: this.resource!, cv: e.cv }))
    this.early = []
    this.push(valid.map((s) => this.item({ r: s.r, res: s.res, cv: typeof s.cv === 'string' ? s.cv : null })))
    if (valid.length !== spooled.length || this.spoolDirty) this.saveSpool()
    this.push(early)
    if (early.length) this.persist(() => this.config.spool!.append(early.map(({ r, res, cv }) => ({ r, res, cv }))))
    if (this.queue.length >= this.maxBatch) void this.pump()
  }

  /**
   * Runs a spool write after the ones before it: at once when none is
   * pending (a synchronous spool has the record on disk before capture
   * returns). Failures are recorded, never thrown.
   */
  private persist(write: () => void | Promise<void>): void {
    if (!this.config.spool) return
    const fail = (e: unknown) => {
      this.counters.lastError = String((e as Error)?.message ?? e)
    }
    const run = (): void | Promise<void> => {
      try {
        const r = write()
        if (isThenable(r)) return r.then(undefined, fail)
      } catch (e) {
        fail(e)
      }
    }
    if (this.spoolPending === 0) {
      const r = run()
      if (!isThenable(r)) return
      this.spoolPending++
      this.spoolChain = r.finally(() => {
        this.spoolPending--
      })
      return
    }
    this.spoolPending++
    this.spoolChain = this.spoolChain.then(run).finally(() => {
      this.spoolPending--
    })
  }

  /** Rewrites the spool with every live record. */
  private saveSpool(): void {
    if (!this.config.spool) return
    this.spoolDirty = false
    const live = [...this.formed.flatMap((f) => f.items), ...this.queue].map(({ r, res, cv }) => ({ r, res, cv }))
    this.persist(() => this.config.spool!.replace(live))
  }

  private async compactSpool(): Promise<void> {
    if (this.spoolDirty) this.saveSpool()
    await this.spoolChain
  }

  private hasWork(): boolean {
    return this.formed.length > 0 || this.queue.length > 0 || this.pendingDrops.size > 0
  }

  /** The next batch from the front of the queue: one resource, one consent state, within the byte budget. */
  private form(budget: number): Formed | null {
    if (!this.resource) return null
    const first = this.queue[0]
    const res = first ? first.res : this.resource
    const cv = first ? this.effective(first.cv) : null
    const resKey = JSON.stringify(res)
    const items: Item[] = []
    let bytes = 2048
    for (const it of this.queue) {
      if (items.length >= this.maxBatch) break
      if (this.effective(it.cv) !== cv || JSON.stringify(it.res) !== resKey) break
      const size = utf8Length(JSON.stringify(it.r)) + 1
      if (bytes + size > budget && items.length > 0) break
      items.push(it)
      bytes += size
    }
    const extra: WireRecord[] = []
    if (items.length + this.pendingDrops.size <= this.maxBatch) {
      for (const reason of DROP_REASONS) {
        const n = this.pendingDrops.get(reason)
        if (!n) continue
        extra.push({ type: 'metric', ts: this.ts(), kind: 'counter', name: 'telemetry.dropped', value: n, unit: '{record}', attrs: { reason } })
      }
      this.pendingDrops.clear()
    }
    if (!items.length && !extra.length) return null
    this.queue.splice(0, items.length)
    for (const it of items) this.queuedBytes -= it.bytes
    const f: Formed = { id: hex(this.random(16)), items, extra, res, cv }
    this.formed.push(f)
    return f
  }

  /** The consent version a record is sent under now: its own, while that grant is still in force. */
  private effective(cv: string | null): string | null {
    return cv !== null && this.grant?.version === cv ? cv : null
  }

  private serialize(f: Formed): string {
    const cv = this.effective(f.cv)
    const batch: Batch = {
      v: 1,
      client: this.config.client,
      batch: f.id,
      sent_at: this.ts(),
      resource: f.res,
      ...(cv !== null && this.grant ? { identity: this.grant.identity } : {}),
      records: [...f.items.map((i) => i.r), ...f.extra],
    }
    return JSON.stringify(batch)
  }

  private async tick(): Promise<void> {
    if (this.spoolDirty) this.saveSpool()
    await this.pump()
  }

  /** Sends one batch unless one is in flight or a retry is waiting. */
  private async pump(): Promise<void> {
    if (this.inFlight || this.closed || this.now() < this.retryAt) return
    if (!this.resource || !this.hasWork()) return
    await this.sendOne(false)
  }

  /** One attempt at the oldest batch. True when it is done with (sent or discarded). */
  private sendOne(ignoreBackoff: boolean): Promise<boolean> {
    if (this.inFlight) return this.inFlight
    if (!ignoreBackoff && this.now() < this.retryAt) return Promise.resolve(false)
    const f = this.formed[0] ?? this.form(MAX_BODY)
    if (!f) return Promise.resolve(true)
    this.inFlight = this.attempt(f).finally(() => {
      this.inFlight = null
    })
    return this.inFlight
  }

  private async attempt(f: Formed): Promise<boolean> {
    const json = this.serialize(f)
    const size = utf8Length(json)
    let body: string | Uint8Array = json
    const headers: Record<string, string> = { authorization: `Bearer ${this.config.key}`, 'content-type': 'application/json' }
    if (size > 1024) {
      const z = await gzip(json)
      if (z) {
        body = z
        headers['content-encoding'] = 'gzip'
      }
    }
    let status = 0
    let retryAfter: string | null = null
    try {
      const res = await this.transport({ url: this.url, headers, body, timeoutMs: REQUEST_TIMEOUT_MS })
      status = res.status
      retryAfter = res.retryAfter
      this.counters.lastStatus = status
    } catch (e) {
      this.counters.lastError = String((e as Error)?.message ?? e)
    }
    const n = f.items.length + f.extra.length
    if (status >= 200 && status < 300) {
      this.done(f)
      this.counters.sent += n
      this.failures = 0
      this.retryAt = 0
      return true
    }
    if (status === 413) {
      this.formed.shift()
      if (n > 1) {
        const all = [...f.items.map((i) => ({ kind: 'item' as const, i })), ...f.extra.map((r) => ({ kind: 'extra' as const, r }))]
        const half = Math.ceil(all.length / 2)
        const part = (xs: typeof all): Formed => ({
          id: hex(this.random(16)),
          items: xs.flatMap((x) => (x.kind === 'item' ? [x.i] : [])),
          extra: xs.flatMap((x) => (x.kind === 'extra' ? [x.r] : [])),
          res: f.res,
          cv: f.cv,
        })
        this.formed.unshift(part(all.slice(0, half)), part(all.slice(half)))
      } else {
        this.discard(f, 'too_large')
      }
      return true
    }
    const retry = status === 0 || status === 429 || status >= 500
    if (!retry) {
      this.formed.shift()
      this.discard(f, status === 400 ? 'invalid' : 'refused')
      return true
    }
    this.failures++
    const wait = (status === 429 || status === 503 ? retryAfterMs(retryAfter, this.now()) : null) ?? backoffMs(this.failures)
    this.retryAt = this.now() + wait
    if (this.retryTimer) clearTimeout(this.retryTimer)
    if (!this.closed) {
      this.retryTimer = setTimeout(() => {
        this.retryTimer = null
        void this.pump()
      }, wait)
      ;(this.retryTimer as { unref?: () => void }).unref?.()
    }
    return false
  }

  private done(f: Formed): void {
    const at = this.formed.indexOf(f)
    if (at >= 0) this.formed.splice(at, 1)
    if (f.items.length) this.saveSpool()
  }

  private discard(f: Formed, reason: DropReason): void {
    // The drop counts the batch carried are lost with it; they are not re-counted.
    this.counters.dropped[reason] += f.items.length
    if (f.items.length) this.pendingDrops.set(reason, (this.pendingDrops.get(reason) ?? 0) + f.items.length)
    if (f.items.length) this.saveSpool()
  }

  private grantConsent(g: ConsentGrant): ConsentHandle {
    if (!g || !CONSENT_VERSION.test(g.version ?? '')) throw new TypeError(`telemetry: consent version must match ${CONSENT_VERSION}`)
    const sub = text(g.identity?.sub, 255)
    if (!sub) throw new TypeError('telemetry: consent needs identity.sub, the account the user approved sending')
    const at = g.grantedAt === undefined ? this.now() : new Date(g.grantedAt).getTime()
    if (!Number.isFinite(at)) throw new TypeError('telemetry: consent grantedAt is not a time')
    const deviceName = text(g.identity.deviceName, 64)
    const email = typeof g.identity.email === 'string' && g.identity.email.includes('@') && utf8Length(g.identity.email) >= 3 && utf8Length(g.identity.email) <= 254 ? g.identity.email : undefined
    const grantedAt = timestamp(at)
    const token = {}
    const previous = this.grant
    this.grant = {
      version: g.version,
      grantedAt,
      token,
      identity: { sub, ...(deviceName ? { device_name: deviceName } : {}), ...(email ? { email } : {}), consent: { version: g.version, granted_at: grantedAt } },
    }
    if (previous && previous.version !== g.version) this.forgetConsent()
    this.capture('event', () => ({ type: 'event', ts: this.ts(), name: 'telemetry.consent.granted', attrs: { 'consent.version': g.version } }), true)
    const self = this
    return {
      version: g.version,
      grantedAt,
      get active() {
        return self.grant?.token === token
      },
      revoke(): boolean {
        if (self.grant?.token !== token) return false
        self.grant = null
        self.forgetConsent()
        self.capture('event', () => ({ type: 'event', ts: self.ts(), name: 'telemetry.consent.revoked', attrs: { 'consent.version': g.version } }), true)
        return true
      },
    }
  }

  /** Nothing queued, formed or spooled is sent with identity any more. */
  private forgetConsent(): void {
    for (const it of this.queue) it.cv = null
    for (const e of this.early) e.cv = null
    for (const f of this.formed) {
      f.cv = null
      for (const it of f.items) it.cv = null
    }
    this.saveSpool()
  }
}

function isThenable<T>(v: unknown): v is Promise<T> {
  return !!v && typeof (v as { then?: unknown }).then === 'function'
}

function toErrorInput(err: unknown): ErrorInput {
  if (err instanceof Error) {
    return { type: err.name || err.constructor?.name || 'Error', message: err.message, stack: err.stack }
  }
  if (err && typeof err === 'object' && typeof (err as ErrorInput).type === 'string') return err as ErrorInput
  return { type: 'Error', message: typeof err === 'string' ? err : undefined }
}

function secureRandom(): ((n: number) => Uint8Array) | null {
  const c = (globalThis as { crypto?: { getRandomValues?: (a: Uint8Array) => Uint8Array } }).crypto
  if (!c || typeof c.getRandomValues !== 'function') return null
  return (n) => c.getRandomValues!(new Uint8Array(n))
}

function beaconFunction(): ((url: string, body: Blob) => boolean) | null {
  const nav = (globalThis as { navigator?: { sendBeacon?: (url: string, body: Blob) => boolean } }).navigator
  if (!nav || typeof nav.sendBeacon !== 'function') return null
  return (url, body) => nav.sendBeacon!(url, body)
}
