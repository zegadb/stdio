/**
 * Scrubbing before a record is queued (docs/telemetry-wire.md §7). The rules
 * are the collector's (addiscorp/infra services/telemetry/src/scrub.ts:
 * DROP_KEY, DROP_ATTR, URL_KEY and the customer-app allowlist of APS 25 §7);
 * the collector applies them again with the kind it has registered.
 */

import type { Attrs, Kind, WireRecord } from './wire.js'

/** Verbatim from the collector's src/scrub.ts. */
export const DROP_KEY =
  /cookie|authorization|auth[_-]?token|password|passwd|secret|api[_-]?key|access[_-]?token|refresh[_-]?token|session|csrf|^user$|^user[._-]?(id|name|email|ip|segment)|^enduser|email|phone|ip_address|^ip$|^vars$|^extra$|^body$|^data$|^query_string$|^query$|query_params|^cookies$|^env$|^dsn$/i
/** Verbatim from the collector's src/scrub.ts. */
export const DROP_ATTR =
  /^(http\.request\.header\.|http\.response\.header\.|http\.request\.body|http\.response\.body|url\.query|enduser\.|user\.|client\.address|net\.peer\.ip|net\.sock\.peer\.addr|server\.address)/i
/** The collector's URL_KEY, plus `url.original`, which it strips on spans too. */
export const URL_KEY =
  /^(url|uri|href|target|full_url|http\.url|url\.full|http\.target|url\.original|referer|referrer|location|redirect|redirect_url)$/i

/** Attribute keys a customer app may send (the collector's CUSTOMER_SPAN_KEYS). */
export const CUSTOMER_ATTR_KEYS: ReadonlySet<string> = new Set([
  'http.request.method',
  'http.response.status_code',
  'http.status_code',
  'rpc.method',
  'rpc.service',
  'error.type',
  'network.protocol.name',
])
/** Also kept on the client's own `telemetry.*` records. */
const TELEMETRY_ATTR_KEYS: ReadonlySet<string> = new Set(['reason', 'consent.version'])

const stripQuery = (s: string) => s.replace(/[?#].*$/, '')

/** Attributes after §7, or undefined when none are left. */
export function scrubAttrs(attrs: Attrs | undefined, kind: Kind, reserved: boolean): Attrs | undefined {
  if (!attrs) return undefined
  const out: Attrs = {}
  let n = 0
  for (const [k, v] of Object.entries(attrs)) {
    if (DROP_KEY.test(k) || DROP_ATTR.test(k)) continue
    if (kind === 'customer-app' && !CUSTOMER_ATTR_KEYS.has(k) && !(reserved && TELEMETRY_ATTR_KEYS.has(k))) continue
    out[k] = typeof v === 'string' && URL_KEY.test(k) ? stripQuery(v) : v
    n++
  }
  return n > 0 ? out : undefined
}

/**
 * The record as it may be sent for `kind`, or null when it may not be sent at
 * all (a customer app's logs). Returns a new object.
 */
export function scrubRecord(record: WireRecord, kind: Kind): WireRecord | null {
  switch (record.type) {
    case 'log':
      return kind === 'customer-app' ? null : { ...record }
    case 'error': {
      if (kind !== 'customer-app') return { ...record }
      const { message: _m, stack: _s, ...kept } = record
      return kept
    }
    case 'metric':
    case 'event': {
      const reserved = record.name.startsWith('telemetry.')
      const { attrs, ...rest } = record
      const scrubbed = scrubAttrs(attrs, kind, reserved)
      return scrubbed ? { ...rest, attrs: scrubbed } : rest
    }
  }
}
