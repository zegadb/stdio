// A fake telemetry intake for the tests: a real HTTP server on 127.0.0.1 that
// checks every request against docs/telemetry-wire.md, strictly (unknown
// fields, limits, formats), and records what it was sent. It answers what the
// test tells it to (202 by default).
import { gunzipSync } from 'node:zlib'

export type Received = {
  headers: Record<string, string>
  url: URL
  body: Record<string, unknown>
  /** Every way the request breaks the spec; [] when it keeps it. */
  problems: string[]
  at: number
}

export type Answer = { status: number; retryAfter?: string; body?: unknown } | 'hang'

const TS = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/
const NAME = /^[a-z][a-z0-9_]*(\.[a-z0-9_]+)*$/
const HEX32 = /^[0-9a-f]{32}$/
const HEX16 = /^[0-9a-f]{16}$/
const RELEASE = /^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$/
const enc = new TextEncoder()
const bytes = (s: string) => enc.encode(s).length

type Obj = Record<string, unknown>
const isObj = (v: unknown): v is Obj => !!v && typeof v === 'object' && !Array.isArray(v)

function only(o: Obj, allowed: string[], where: string, out: string[]): void {
  for (const k of Object.keys(o)) if (!allowed.includes(k)) out.push(`${where}: unknown field ${k}`)
}

function str(v: unknown, min: number, max: number): boolean {
  return typeof v === 'string' && bytes(v) >= min && bytes(v) <= max
}

function attrs(v: unknown, where: string, out: string[]): void {
  if (v === undefined) return
  if (!isObj(v)) return void out.push(`${where}.attrs: not an object`)
  const entries = Object.entries(v)
  if (entries.length === 0) out.push(`${where}.attrs: empty (must be omitted)`)
  if (entries.length > 32) out.push(`${where}.attrs: more than 32`)
  for (const [k, x] of entries) {
    if (!NAME.test(k) || bytes(k) > 64) out.push(`${where}.attrs: bad key ${k}`)
    const ok = (typeof x === 'string' && bytes(x) <= 256) || (typeof x === 'number' && Number.isFinite(x)) || typeof x === 'boolean'
    if (!ok) out.push(`${where}.attrs.${k}: bad value`)
  }
}

/** Every way `record` breaks §5. */
export function recordProblems(r: unknown, where: string): string[] {
  const out: string[] = []
  if (!isObj(r)) return [`${where}: not an object`]
  if (bytes(JSON.stringify(r)) > 32_768) out.push(`${where}: over 32 KiB`)
  if (typeof r.ts !== 'string' || !TS.test(r.ts)) out.push(`${where}.ts: ${String(r.ts)}`)
  if (r.trace !== undefined) {
    const t = r.trace as Obj
    if (!isObj(t) || !HEX32.test(String(t.trace_id)) || !HEX16.test(String(t.span_id)) || /^0+$/.test(String(t.trace_id)) || /^0+$/.test(String(t.span_id))) out.push(`${where}.trace: bad`)
    else only(t, ['trace_id', 'span_id'], `${where}.trace`, out)
  }
  if (r.sample_rate !== undefined && !(typeof r.sample_rate === 'number' && r.sample_rate > 0 && r.sample_rate < 1)) out.push(`${where}.sample_rate: bad`)
  const common = ['type', 'ts', 'trace', 'sample_rate']
  switch (r.type) {
    case 'metric':
      only(r, [...common, 'kind', 'name', 'value', 'unit', 'attrs'], where, out)
      if (!['counter', 'gauge', 'histogram'].includes(String(r.kind))) out.push(`${where}.kind`)
      if (typeof r.name !== 'string' || !NAME.test(r.name) || bytes(r.name) > 128) out.push(`${where}.name`)
      if (typeof r.value !== 'number' || !Number.isFinite(r.value) || (r.kind === 'counter' && r.value < 0)) out.push(`${where}.value`)
      if (r.unit !== undefined && !/^[A-Za-z0-9%/_.{}-]{1,32}$/.test(String(r.unit))) out.push(`${where}.unit`)
      attrs(r.attrs, where, out)
      break
    case 'log':
      only(r, [...common, 'level', 'scope', 'message'], where, out)
      if (!['debug', 'info', 'warn', 'error'].includes(String(r.level))) out.push(`${where}.level`)
      if (!/^[A-Za-z0-9._:/-]{1,64}$/.test(String(r.scope))) out.push(`${where}.scope`)
      if (!str(r.message, 1, 2000)) out.push(`${where}.message`)
      break
    case 'error':
      only(r, [...common, 'error_type', 'message', 'stack', 'fingerprint', 'code'], where, out)
      if (!str(r.error_type, 1, 128)) out.push(`${where}.error_type`)
      if (r.message !== undefined && !str(r.message, 1, 2000)) out.push(`${where}.message`)
      if (r.stack !== undefined && !str(r.stack, 1, 16_384)) out.push(`${where}.stack`)
      if (r.fingerprint !== undefined && !str(r.fingerprint, 1, 128)) out.push(`${where}.fingerprint`)
      if (r.code !== undefined && !/^[A-Za-z0-9_.:-]{1,64}$/.test(String(r.code))) out.push(`${where}.code`)
      break
    case 'event':
      only(r, [...common, 'name', 'attrs'], where, out)
      if (typeof r.name !== 'string' || !NAME.test(r.name) || bytes(r.name) > 128) out.push(`${where}.name`)
      attrs(r.attrs, where, out)
      break
    default:
      out.push(`${where}.type: ${String(r.type)}`)
  }
  return out
}

/** Every way a request breaks §2–§6. */
export function batchProblems(headers: Record<string, string>, url: URL, raw: Uint8Array): { problems: string[]; body: Obj } {
  const out: string[] = []
  const auth = headers.authorization
  const queryKey = url.searchParams.get('key')
  if (auth && queryKey) out.push('key both in the header and the query')
  if (!auth && !queryKey) out.push('no key')
  if (auth && !/^Bearer [0-9a-f]{32}$/.test(auth)) out.push(`authorization: ${auth}`)
  if (queryKey && (!HEX32.test(queryKey) || [...url.searchParams.keys()].length !== 1)) out.push('query: only key=<32 hex>')
  const type = headers['content-type']
  if (type !== 'application/json' && !(queryKey && type === 'text/plain;charset=UTF-8')) out.push(`content-type: ${type}`)
  const encoding = headers['content-encoding']
  if (raw.length > 524_288) out.push('body over 512 KiB')
  let decoded = raw
  if (encoding === 'gzip') decoded = gunzipSync(raw)
  else if (encoding !== undefined) out.push(`content-encoding: ${encoding}`)
  if (decoded.length > 524_288) out.push('decoded body over 512 KiB')
  let body: Obj = {}
  try {
    body = JSON.parse(new TextDecoder().decode(decoded)) as Obj
  } catch {
    out.push('not JSON')
    return { problems: out, body }
  }
  only(body, ['v', 'client', 'batch', 'sent_at', 'resource', 'identity', 'records'], 'batch', out)
  if (body.v !== 1) out.push('v')
  if (!/^[a-z0-9][a-z0-9._-]{0,63}$/.test(String(body.client))) out.push('client')
  if (!HEX32.test(String(body.batch))) out.push('batch id')
  if (!TS.test(String(body.sent_at))) out.push('sent_at')
  const res = body.resource as Obj
  if (!isObj(res)) out.push('resource')
  else {
    only(res, ['release', 'platform', 'os', 'device_class', 'install_id', 'sdk', 'environment'], 'resource', out)
    if (!RELEASE.test(String(res.release))) out.push('resource.release')
    if (!['browser', 'worker', 'node', 'bun', 'deno', 'react-native', 'native'].includes(String(res.platform))) out.push('resource.platform')
    if (res.os !== undefined) {
      const os = res.os as Obj
      if (!isObj(os)) out.push('resource.os')
      else {
        only(os, ['name', 'version'], 'resource.os', out)
        if (os.name !== undefined && !str(os.name, 1, 32)) out.push('resource.os.name')
        if (os.version !== undefined && !str(os.version, 1, 32)) out.push('resource.os.version')
      }
    }
    if (res.device_class !== undefined && !str(res.device_class, 1, 32)) out.push('resource.device_class')
    if (!HEX32.test(String(res.install_id))) out.push('resource.install_id')
    const sdk = res.sdk as Obj
    if (!isObj(sdk) || !/^[a-z0-9@/._-]{1,64}$/.test(String(sdk.name)) || !RELEASE.test(String(sdk.version))) out.push('resource.sdk')
    else only(sdk, ['name', 'version'], 'resource.sdk', out)
    if (res.environment !== undefined && !/^[a-z0-9_-]{1,32}$/.test(String(res.environment))) out.push('resource.environment')
  }
  if (body.identity !== undefined) {
    const id = body.identity as Obj
    if (!isObj(id)) out.push('identity')
    else {
      only(id, ['sub', 'device_name', 'email', 'consent'], 'identity', out)
      if (!str(id.sub, 1, 255)) out.push('identity.sub')
      if (id.device_name !== undefined && !str(id.device_name, 1, 64)) out.push('identity.device_name')
      if (id.email !== undefined && !(str(id.email, 3, 254) && String(id.email).includes('@'))) out.push('identity.email')
      const c = id.consent as Obj
      if (!isObj(c) || !/^[A-Za-z0-9._-]{1,64}$/.test(String(c.version)) || !TS.test(String(c.granted_at))) out.push('identity.consent')
      else only(c, ['version', 'granted_at'], 'identity.consent', out)
    }
  }
  const records = body.records
  if (!Array.isArray(records) || records.length < 1 || records.length > 500) out.push('records: 1 to 500')
  else records.forEach((r, i) => out.push(...recordProblems(r, `records[${i}]`)))
  return { problems: out, body }
}

export type Intake = {
  url: string
  received: Received[]
  /** What to answer next; when empty, `fallback`. */
  answers: Answer[]
  fallback: Answer
  stop(): void
  /** Resolves when `n` requests have arrived (or rejects after `ms`). */
  waitFor(n: number, ms?: number): Promise<void>
}

export function startIntake(): Intake {
  const received: Received[] = []
  const waiters: { n: number; resolve: () => void }[] = []
  const intake: Intake = {
    url: '',
    received,
    answers: [],
    fallback: { status: 202 },
    stop: () => server.stop(true),
    waitFor(n, ms = 5000) {
      if (received.length >= n) return Promise.resolve()
      return new Promise((resolve, reject) => {
        const t = setTimeout(() => reject(new Error(`intake: ${received.length} of ${n} requests after ${ms} ms`)), ms)
        waiters.push({ n, resolve: () => (clearTimeout(t), resolve()) })
      })
    },
  }
  const server = Bun.serve({
    port: 0,
    hostname: '127.0.0.1',
    async fetch(req) {
      const url = new URL(req.url)
      const headers: Record<string, string> = {}
      req.headers.forEach((v, k) => (headers[k] = v))
      const raw = new Uint8Array(await req.arrayBuffer())
      const answer = intake.answers.shift() ?? intake.fallback
      const { problems, body } = url.pathname === '/v1/ingest' && req.method === 'POST' ? batchProblems(headers, url, raw) : { problems: [`${req.method} ${url.pathname}`], body: {} }
      received.push({ headers, url, body, problems, at: Date.now() })
      for (const w of waiters.splice(0)) (received.length >= w.n ? w.resolve() : waiters.push(w))
      if (answer === 'hang') return new Promise<Response>(() => {})
      const n = Array.isArray(body.records) ? body.records.length : 0
      const payload = answer.body ?? (answer.status === 202 ? { accepted: n, rejected: [], duplicate: false } : { error: 'test' })
      return new Response(JSON.stringify(payload), { status: answer.status, headers: { 'content-type': 'application/json', ...(answer.retryAfter ? { 'retry-after': answer.retryAfter } : {}) } })
    },
  })
  intake.url = `http://127.0.0.1:${server.port}/v1/ingest`
  return intake
}
