/**
 * The telemetry wire format, version 1 (docs/telemetry-wire.md): the record
 * and batch types, the limits, and the helpers that make a record valid
 * before it is queued (truncation, names, attributes, timestamps).
 *
 * No runtime imports: this file is safe in a browser, a Worker, Node, Bun or
 * React Native.
 */

export const WIRE_VERSION = 1
export const DEFAULT_ENDPOINT = 'https://telemetry.addis.gg/v1/ingest'
export const SDK_NAME = '@addiscorp/stdio'
/** The package's version (package.json; test/telemetry.test.ts checks they agree). */
export const SDK_VERSION = '0.1.0'

/** §3 */
export const MAX_BODY = 524_288
export const MAX_BATCH_RECORDS = 500
export const MAX_RECORD = 32_768
export const MAX_BEACON = 61_440
/** §9: no response within this is a failure, retried with backoff. */
export const REQUEST_TIMEOUT_MS = 10_000

export type Kind = 'service' | 'customer-app'
export type Platform = 'browser' | 'worker' | 'node' | 'bun' | 'deno' | 'react-native' | 'native'
export type RecordType = 'metric' | 'log' | 'error' | 'event'
export type MetricKind = 'counter' | 'gauge' | 'histogram'
export type LogLevel = 'debug' | 'info' | 'warn' | 'error'
export type DropReason = 'queue_full' | 'spool_full' | 'too_large' | 'invalid' | 'refused'
export const DROP_REASONS: readonly DropReason[] = ['queue_full', 'spool_full', 'too_large', 'invalid', 'refused']

export type AttrValue = string | number | boolean
export type Attrs = Record<string, AttrValue>

/** W3C trace context ids. */
export type TraceContext = { trace_id: string; span_id: string }

type Common = { ts: string; trace?: TraceContext; sample_rate?: number }

export type MetricRecord = Common & { type: 'metric'; kind: MetricKind; name: string; value: number; unit?: string; attrs?: Attrs }
export type LogRecord = Common & { type: 'log'; level: LogLevel; scope: string; message: string }
export type ErrorRecord = Common & { type: 'error'; error_type: string; message?: string; stack?: string; fingerprint?: string; code?: string }
export type EventRecord = Common & { type: 'event'; name: string; attrs?: Attrs }
export type WireRecord = MetricRecord | LogRecord | ErrorRecord | EventRecord

export type Resource = {
  release: string
  platform: Platform
  os?: { name?: string; version?: string }
  device_class?: string
  install_id: string
  sdk: { name: string; version: string }
  environment?: string
}

export type Identity = {
  sub: string
  device_name?: string
  email?: string
  consent: { version: string; granted_at: string }
}

export type Batch = {
  v: 1
  client: string
  batch: string
  sent_at: string
  resource: Resource
  identity?: Identity
  records: WireRecord[]
}

export const NAME = /^[a-z][a-z0-9_]*(\.[a-z0-9_]+)*$/
export const CLIENT_ID = /^[a-z0-9][a-z0-9._-]{0,63}$/
export const KEY = /^[0-9a-f]{32}$/
export const RELEASE = /^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$/
export const UNIT = /^[A-Za-z0-9%/_.{}-]{1,32}$/
export const SCOPE = /^[A-Za-z0-9._:/-]{1,64}$/
export const CODE = /^[A-Za-z0-9_.:-]{1,64}$/
export const ENVIRONMENT = /^[a-z0-9_-]{1,32}$/
export const CONSENT_VERSION = /^[A-Za-z0-9._-]{1,64}$/
export const HEX32 = /^[0-9a-f]{32}$/
export const HEX16 = /^[0-9a-f]{16}$/

const encoder = new TextEncoder()

/** UTF-8 length of a string. */
export function utf8Length(s: string): number {
  return encoder.encode(s).length
}

/**
 * The longest prefix of `s` that is at most `max` bytes of UTF-8, never
 * splitting a code point (§3).
 */
export function truncateUtf8(s: string, max: number): string {
  if (s.length * 3 <= max) return s
  let bytes = 0
  let end = 0
  for (const ch of s) {
    const cp = ch.codePointAt(0)!
    const n = cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4
    if (bytes + n > max) break
    bytes += n
    end += ch.length
  }
  return s.slice(0, end)
}

/** `YYYY-MM-DDTHH:MM:SS.sssZ` (§4). */
export function timestamp(ms: number): string {
  return new Date(ms).toISOString()
}

/** A trace context, or undefined when it is not two valid, non-zero W3C ids. */
export function traceContext(t: TraceContext | undefined): TraceContext | undefined {
  if (!t) return undefined
  const trace_id = String(t.trace_id).toLowerCase()
  const span_id = String(t.span_id).toLowerCase()
  if (!HEX32.test(trace_id) || !HEX16.test(span_id) || /^0+$/.test(trace_id) || /^0+$/.test(span_id)) return undefined
  return { trace_id, span_id }
}

/**
 * Attributes as the wire allows them (§5): at most 32, valid keys, string
 * values truncated to 256 bytes, finite numbers, booleans. Anything else is
 * left out. Returns undefined for an empty result.
 */
export function cleanAttrs(attrs: Record<string, unknown> | undefined): Attrs | undefined {
  if (!attrs || typeof attrs !== 'object') return undefined
  const out: Attrs = {}
  let n = 0
  for (const [k, v] of Object.entries(attrs)) {
    if (n >= 32) break
    if (k.length > 64 || !NAME.test(k)) continue
    if (typeof v === 'string') out[k] = truncateUtf8(v, 256)
    else if (typeof v === 'number' && Number.isFinite(v)) out[k] = v
    else if (typeof v === 'boolean') out[k] = v
    else continue
    n++
  }
  return n > 0 ? out : undefined
}

/** A non-empty string truncated to `max` bytes, or undefined. */
export function text(value: unknown, max: number): string | undefined {
  if (typeof value !== 'string' || value.length === 0) return undefined
  return truncateUtf8(value, max)
}
