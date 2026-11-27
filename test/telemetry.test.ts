// The telemetry client against a fake intake (test/telemetry-intake.ts) that
// checks every request against docs/telemetry-wire.md.
import { afterEach, beforeEach, describe, expect, test } from 'bun:test'
import { mkdtempSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import golden from './fixtures/telemetry-golden.json'
import { startIntake, type Intake } from './telemetry-intake.ts'
import pkg from '../package.json'
import { DROP_KEY, SDK_NAME, SDK_VERSION, backoffMs, createTelemetry, kvSpool, memoryStore, retryAfterMs, type Telemetry, type TelemetryConfig } from '../src/telemetry/index.ts'
import { fileSpool, fileStore } from '../src/telemetry/node.ts'

const KEY = '0123456789abcdef0123456789abcdef'
const T = Date.parse('2026-11-27T18:04:01.000Z')

/** Random bytes: these hex values first, in order, then real randomness. */
function fixedRandom(...hexes: string[]): (n: number) => Uint8Array {
  const queue = [...hexes]
  return (n) => {
    const h = queue.shift()
    if (!h) return crypto.getRandomValues(new Uint8Array(n))
    return Uint8Array.from(h.match(/../g)!.map((b) => parseInt(b, 16)))
  }
}

let intake: Intake
let clients: Telemetry[] = []
// The OS temporary directory (TMPDIR; AGENTS.md sets it inside the checkout).
const tmp = tmpdir()

function client(overrides: Partial<TelemetryConfig> = {}): Telemetry {
  const t = createTelemetry({ client: 'ava-desktop', key: KEY, kind: 'service', release: '1.4.2', platform: 'node', endpoint: intake.url, interval: 0, ...overrides })
  clients.push(t)
  return t
}

const records = (i = intake) => i.received.flatMap((r) => r.body.records as Record<string, unknown>[])
const names = (i = intake) => records(i).map((r) => (r.name ?? r.type) as string)

beforeEach(() => {
  intake = startIntake()
  clients = []
})
afterEach(async () => {
  for (const c of clients) await Promise.race([c.shutdown(), Bun.sleep(200)])
  intake.stop()
  for (const r of intake.received) expect(r.problems).toEqual([])
})

/** The calls the golden fixture is made from (the Rust test makes the same ones). */
function goldenCalls(t: Telemetry): void {
  const c = golden.calls
  t.counter(c.counter.name, c.counter.value, { unit: c.counter.unit, attrs: c.counter.attrs })
  t.log(c.log.level as 'info', c.log.scope, c.log.message)
  t.error(c.error)
  t.event(c.event.name, c.event.attrs)
  t.gauge(c.gauge.name, c.gauge.value, { trace: c.gauge.trace })
}

describe('the wire format', () => {
  for (const kind of ['service', 'customer-app'] as const) {
    test(`a ${kind} batch is the golden fixture`, async () => {
      const t = client({ kind, platform: 'native', os: { name: 'macOS', version: '26.1' }, deviceClass: 'Mac', environment: 'production', now: () => T, randomBytes: fixedRandom(golden[kind].resource.install_id, golden[kind].batch) })
      await t.ready
      goldenCalls(t)
      await t.flush()
      expect(intake.received).toHaveLength(1)
      const got = intake.received[0]
      expect(got.problems).toEqual([])
      const { sdk, ...resource } = got.body.resource as Record<string, unknown>
      expect(sdk).toEqual({ name: SDK_NAME, version: SDK_VERSION })
      expect({ ...got.body, resource }).toEqual(golden[kind])
      expect(got.headers.authorization).toBe(`Bearer ${KEY}`)
      expect(got.headers['content-type']).toBe('application/json')
    })
  }

  test('a body over 1 KiB is gzipped', async () => {
    const t = client()
    for (let i = 0; i < 40; i++) t.log('info', 'sync', `line ${i} `.repeat(10))
    await t.flush()
    expect(intake.received[0].headers['content-encoding']).toBe('gzip')
    expect(records()).toHaveLength(40)
  })

  test('the drop_key vectors (which the Rust test reads) are what DROP_KEY says', () => {
    for (const [key, want] of Object.entries(golden.drop_key)) expect([key, DROP_KEY.test(key)]).toEqual([key, want])
  })

  test('the sdk version is the package version', () => {
    expect(SDK_VERSION).toBe(pkg.version)
  })

  test('a reserved or malformed name is not sent, and counted as invalid', async () => {
    const t = client()
    t.event('telemetry.consent.revoked')
    t.event('Not A Name')
    t.counter('ok.count', -1)
    t.gauge('ok.gauge', Number.NaN)
    t.event('ok.event')
    await t.flush()
    expect(names()).toEqual(['ok.event', 'telemetry.dropped'])
    const drop = records().find((r) => r.name === 'telemetry.dropped')!
    expect(drop).toMatchObject({ kind: 'counter', value: 4, unit: '{record}', attrs: { reason: 'invalid' } })
  })

  test('strings over their limit are cut on a UTF-8 boundary', async () => {
    const t = client()
    t.log('info', 'sync', 'é'.repeat(1500)) // 3,000 bytes
    await t.flush()
    const m = records()[0].message as string
    expect(new TextEncoder().encode(m).length).toBe(2000)
    expect(m).toBe('é'.repeat(1000))
  })

  test('the main entry pulls in no telemetry or network code', async () => {
    const out = await Bun.build({ entrypoints: [join(import.meta.dir, '..', 'src', 'index.ts')], target: 'node', external: ['chalk', 'figlet'] })
    expect(out.success).toBe(true)
    const code = await out.outputs[0].text()
    expect(code).not.toContain('telemetry')
    expect(code).not.toContain('fetch(')
    expect(code).not.toContain('sendBeacon')
  })
})

test('the telemetry entry bundles for a browser, a Worker or React Native: no node: imports', async () => {
  const out = await Bun.build({ entrypoints: [join(import.meta.dir, '..', 'src', 'telemetry', 'index.ts')], target: 'browser' })
  expect(out.success).toBe(true)
  expect(await out.outputs[0].text()).not.toContain('node:')
})

describe('consent', () => {
  const grant = { version: '2026-11-27', grantedAt: '2026-11-27T17:59:00.000Z', identity: { sub: 'acc_01J9Z5V7X2', deviceName: "Maya's MacBook" } }

  test('no identity is sent without a grant', async () => {
    const t = client()
    t.event('app.open')
    t.error(new TypeError('boom'))
    await t.flush()
    expect(intake.received.length).toBeGreaterThan(0)
    for (const r of intake.received) expect(r.body.identity).toBeUndefined()
    expect(JSON.stringify(intake.received.map((r) => r.body))).not.toContain('acc_01J9Z5V7X2')
  })

  test('identity is sent after a grant, and only on records captured under it', async () => {
    const t = client()
    t.event('before.grant')
    const handle = t.consent.grant(grant)
    t.event('after.grant')
    await t.flush()
    expect(intake.received).toHaveLength(2)
    const [anon, identified] = intake.received
    expect(anon.body.identity).toBeUndefined()
    expect((anon.body.records as { name: string }[]).map((r) => r.name)).toEqual(['before.grant'])
    expect(identified.body.identity).toEqual({ sub: 'acc_01J9Z5V7X2', device_name: "Maya's MacBook", consent: { version: '2026-11-27', granted_at: '2026-11-27T17:59:00.000Z' } })
    expect((identified.body.records as { name: string }[]).map((r) => r.name)).toEqual(['telemetry.consent.granted', 'after.grant'])
    expect(handle.active).toBe(true)
  })

  test('identity is gone after revoke, even for records captured under consent', async () => {
    const t = client()
    const handle = t.consent.grant(grant)
    t.event('during.consent')
    expect(handle.revoke()).toBe(true)
    t.event('after.revoke')
    await t.flush()
    for (const r of intake.received) expect(r.body.identity).toBeUndefined()
    expect(names()).toEqual(['telemetry.consent.granted', 'during.consent', 'telemetry.consent.revoked', 'after.revoke'])
    expect(records().find((r) => r.name === 'telemetry.consent.revoked')!.attrs).toEqual({ 'consent.version': '2026-11-27' })
    expect(handle.active).toBe(false)
    expect(handle.revoke()).toBe(false)
    expect(t.consent.current).toBeNull()
  })

  test('a retry after revoke is rebuilt without identity', async () => {
    const t = client()
    t.consent.grant(grant)
    t.event('during.consent')
    intake.answers.push({ status: 503, retryAfter: '1' })
    await t.flush() // fails once, then once more ignoring the wait
    expect(intake.received[0].body.identity).toBeDefined()
    const handle = t.consent.grant({ ...grant, version: '2026-11-28' })
    handle.revoke()
    await t.flush()
    const retries = intake.received.slice(1).filter((r) => r.body.batch === intake.received[0].body.batch)
    expect(retries.length).toBeGreaterThan(0)
    for (const r of retries) expect(r.body.identity).toBeUndefined()
  })

  test('identity is never written to the spool', async () => {
    const dir = mkdtempSync(join(tmp, 'consent-'))
    const t = client({ spool: fileSpool(join(dir, 'spool.ndjson')), endpoint: 'http://127.0.0.1:9/v1/ingest' })
    await t.ready
    t.consent.grant({ ...grant, identity: { ...grant.identity, email: 'maya@example.com' } })
    t.event('during.consent')
    const onDisk = readFileSync(join(dir, 'spool.ndjson'), 'utf8')
    expect(onDisk).toContain('during.consent')
    expect(onDisk).not.toContain('acc_01J9Z5V7X2')
    expect(onDisk).not.toContain('maya@example.com')
    expect(onDisk).not.toContain('MacBook')
  })

  test('there is no way to attach identity but a grant', async () => {
    const t = client()
    t.event('sneaky', { user: 'maya', 'user.id': 'u1', email: 'maya@example.com', user_name: 'm', 'enduser.id': 'x', session_id: 's', ok: 1 })
    await t.flush()
    expect(records()[0].attrs).toEqual({ ok: 1 })
  })
})

describe('spool', () => {
  test('records survive a restart (a crash: no shutdown, no flush)', async () => {
    const dir = mkdtempSync(join(tmp, 'spool-'))
    const spoolPath = join(dir, 'spool.ndjson')
    const storePath = join(dir, 'store.json')
    const first = createTelemetry({ client: 'ava-desktop', key: KEY, kind: 'service', release: '1.4.2', platform: 'node', endpoint: 'http://127.0.0.1:9/v1/ingest', interval: 0, spool: fileSpool(spoolPath), store: fileStore(storePath) })
    await first.ready
    first.event('before.crash', { n: 1 })
    first.counter('jobs.done', 2)
    // The process dies here: the client is never flushed or shut down.
    const second = client({ release: '1.4.3', spool: fileSpool(spoolPath), store: fileStore(storePath) })
    await second.ready
    second.event('after.restart')
    await second.flush()
    expect(names()).toEqual(['before.crash', 'jobs.done', 'after.restart'])
    // Each kept the resource it was captured under.
    expect(intake.received.map((r) => (r.body.resource as { release: string }).release)).toEqual(['1.4.2', '1.4.3'])
    // One install id across the restart.
    expect(new Set(intake.received.map((r) => (r.body.resource as { install_id: string }).install_id)).size).toBe(1)
    // Sent records leave the spool.
    expect(readFileSync(spoolPath, 'utf8')).toBe('')
  })

  test('the spool is capped: the oldest go and are counted', async () => {
    const dir = mkdtempSync(join(tmp, 'cap-'))
    const spoolPath = join(dir, 'spool.ndjson')
    const t = client({ endpoint: 'http://127.0.0.1:9/v1/ingest', spool: fileSpool(spoolPath, { maxBytes: 4096 }) })
    await t.ready
    for (let i = 0; i < 200; i++) t.event('tick', { i })
    await t.shutdown()
    const live = readFileSync(spoolPath, 'utf8').trim().split('\n')
    expect(live.length).toBeLessThan(200)
    expect(new TextEncoder().encode(live.join('\n')).length).toBeLessThanOrEqual(4096)
    // Newest kept.
    expect(JSON.parse(live.at(-1)!).r.attrs.i).toBe(199)
    expect(t.stats().dropped.spool_full).toBe(200 - live.length)
  })

  test('kvSpool (React Native) survives a restart', async () => {
    const store = memoryStore()
    const first = client({ endpoint: 'http://127.0.0.1:9/v1/ingest', store, spool: kvSpool(store) })
    await first.ready
    first.event('queued.offline')
    await Bun.sleep(10)
    const second = client({ store, spool: kvSpool(store) })
    await second.ready
    await second.flush()
    expect(names()).toEqual(['queued.offline'])
  })
})

describe('never in the way', () => {
  test('capture does not block or throw while the intake is down; drops are counted and sent', async () => {
    const t = client({ endpoint: 'http://127.0.0.1:9/v1/ingest', maxQueue: 100, maxBatch: 50 })
    await t.ready
    const start = performance.now()
    for (let i = 0; i < 5000; i++) t.event('tick', { i })
    const ms = performance.now() - start
    expect(ms).toBeLessThan(1000)
    await t.flush() // connection refused: resolves, never rejects
    const s = t.stats()
    expect(s.queued).toBe(100)
    expect(s.dropped.queue_full).toBe(4900)
    expect(s.lastError).not.toBeNull()
  })

  test('capture does not wait on a request that hangs', async () => {
    intake.fallback = 'hang'
    const t = client({ maxBatch: 10 })
    await t.ready
    for (let i = 0; i < 10; i++) t.event('first', { i }) // fills a batch: a send starts and hangs
    await intake.waitFor(1)
    const start = performance.now()
    for (let i = 0; i < 1000; i++) t.event('more', { i })
    expect(performance.now() - start).toBeLessThan(500)
    // 10 in the hanging batch and 990 queued: the bound counts both.
    expect(t.stats().queued).toBe(1000)
    expect(t.stats().dropped.queue_full).toBe(10)
  })

  test('drops reach the intake as telemetry.dropped once it is back', async () => {
    intake.answers.push({ status: 503, retryAfter: '1' })
    const t = client({ maxQueue: 5, maxBatch: 100 })
    await t.ready
    for (let i = 0; i < 8; i++) t.event('tick', { i })
    await t.flush()
    await t.flush()
    const ok = intake.received.at(-1)!
    const recs = ok.body.records as { name: string; value?: number; attrs?: Record<string, unknown> }[]
    expect(recs.filter((r) => r.name === 'tick').map((r) => r.attrs!.i)).toEqual([3, 4, 5, 6, 7])
    expect(recs.find((r) => r.name === 'telemetry.dropped')).toMatchObject({ value: 3, attrs: { reason: 'queue_full' } })
  })
})

describe('responses', () => {
  test('429 waits for Retry-After and retries the same batch', async () => {
    intake.answers.push({ status: 429, retryAfter: '1' })
    const t = client({ interval: 50 })
    await t.ready
    t.event('limited')
    await t.flush() // one attempt: 429
    await intake.waitFor(2, 3000)
    expect(intake.received[1].body.batch).toBe(intake.received[0].body.batch)
    expect(names().filter((n) => n === 'limited').length).toBe(2)
  })

  test('a scheduled retry honours Retry-After', async () => {
    intake.answers.push({ status: 503, retryAfter: '1' })
    const t = client({ interval: 50 })
    await t.ready
    t.event('busy')
    await intake.waitFor(1, 2000)
    await intake.waitFor(2, 3000)
    const gap = intake.received[1].at - intake.received[0].at
    expect(gap).toBeGreaterThanOrEqual(950)
    expect(intake.received[1].body.batch).toBe(intake.received[0].body.batch)
  })

  test('413 splits the batch in two, each a new batch, oldest first', async () => {
    intake.answers.push({ status: 413 })
    const t = client()
    for (let i = 0; i < 4; i++) t.event('part', { i })
    await t.flush()
    expect(intake.received).toHaveLength(3)
    const [big, a, b] = intake.received
    expect(new Set([big.body.batch, a.body.batch, b.body.batch]).size).toBe(3)
    expect((a.body.records as { attrs: { i: number } }[]).map((r) => r.attrs.i)).toEqual([0, 1])
    expect((b.body.records as { attrs: { i: number } }[]).map((r) => r.attrs.i)).toEqual([2, 3])
  })

  test('400 discards the batch and counts it', async () => {
    intake.answers.push({ status: 400 })
    const t = client()
    t.event('bad')
    await t.flush()
    t.event('next')
    await t.flush()
    // The drop count goes out on its own as soon as the batch is discarded.
    expect(names()).toEqual(['bad', 'telemetry.dropped', 'next'])
    expect(t.stats().dropped.invalid).toBe(1)
  })

  test('Retry-After and backoff follow §9', () => {
    expect(retryAfterMs('60', 0)).toBe(60_000)
    expect(retryAfterMs('0', 0)).toBe(1000)
    expect(retryAfterMs('99999', 0)).toBe(3_600_000)
    expect(retryAfterMs(new Date(30_000).toUTCString(), 0)).toBe(30_000)
    expect(retryAfterMs('soon', 0)).toBeNull()
    expect(backoffMs(1, () => 0)).toBe(500)
    expect(backoffMs(1, () => 1)).toBe(1000)
    expect(backoffMs(4, () => 0)).toBe(4000)
    expect(backoffMs(20, () => 1)).toBe(300_000)
  })
})

describe('runtimes and sampling', () => {
  test('a Worker-style client (no timer) sends on flush, for ctx.waitUntil', async () => {
    const t = client({ interval: 0, platform: 'worker' })
    t.event('request.done')
    await Bun.sleep(30)
    expect(intake.received).toHaveLength(0)
    await t.flush()
    expect((intake.received[0].body.resource as { platform: string }).platform).toBe('worker')
  })

  test('the interval timer sends without a flush', async () => {
    const t = client({ interval: 30 })
    await t.ready
    t.event('timed')
    await intake.waitFor(1, 2000)
    expect(names()).toEqual(['timed'])
  })

  test('a beacon carries the key in the query and a text/plain body', async () => {
    const t = client()
    await t.ready
    t.event('page.hidden')
    let sent: { url: string; body: Blob } | null = null
    t.beacon((url, body) => ((sent = { url, body }), true))
    expect(sent).not.toBeNull()
    const s = sent! as { url: string; body: Blob }
    expect(s.body.type).toBe('text/plain;charset=utf-8')
    // Replay it to the fake intake as a browser would.
    await fetch(s.url, { method: 'POST', headers: { 'content-type': 'text/plain;charset=UTF-8' }, body: await s.body.text() })
    expect(intake.received[0].url.searchParams.get('key')).toBe(KEY)
    expect(names()).toEqual(['page.hidden'])
    expect(t.stats().queued).toBe(0)
  })

  test('sampling is per record type and marks what it kept', async () => {
    const t = client({ sampling: { log: 0, event: 0.5 } })
    for (let i = 0; i < 200; i++) {
      t.log('info', 'sync', 'never')
      t.event('half')
    }
    t.error(new Error('always'))
    await t.flush()
    const recs = records()
    expect(recs.some((r) => r.type === 'log')).toBe(false)
    const half = recs.filter((r) => r.name === 'half')
    expect(half.length).toBeGreaterThan(50)
    expect(half.length).toBeLessThan(150)
    for (const r of half) expect(r.sample_rate).toBe(0.5)
    expect(recs.find((r) => r.type === 'error')!.sample_rate).toBeUndefined()
  })

  test('the install id is random, kept, and resettable', async () => {
    const store = memoryStore()
    const a = client({ store })
    await a.ready
    a.event('one')
    await a.resetInstallId()
    a.event('two')
    await a.flush()
    const [first, second] = intake.received.map((r) => (r.body.resource as { install_id: string }).install_id)
    expect(first).not.toBe(second)
    expect(store.get('addis.telemetry.install_id')).toBe(second)
  })

  test('a config that can never send is refused at construction', () => {
    const base = { client: 'ava-desktop', key: KEY, kind: 'service' as const, release: '1.0.0' }
    expect(() => createTelemetry({ ...base, client: 'Not Registered' })).toThrow(TypeError)
    expect(() => createTelemetry({ ...base, key: 'secret' })).toThrow(TypeError)
    expect(() => createTelemetry({ ...base, endpoint: 'http://telemetry.addis.gg/v1/ingest' })).toThrow(TypeError)
  })
})
