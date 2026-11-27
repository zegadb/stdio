/**
 * @addiscorp/stdio/telemetry/node: what the telemetry client needs from a disk,
 * for Node and Bun. Imports node:fs; never import it in a browser, a Worker or
 * React Native.
 *
 * - fileSpool(path): queued records in a capped NDJSON file. Each capture
 *   appends one line before it returns, so a crash or an offline period loses
 *   nothing; an accepted batch rewrites the file without it (write to a
 *   temporary file, then rename).
 * - fileStore(path): the install id (and any small value) in a JSON file.
 */

import { appendFileSync, mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import { dirname } from 'node:path'

import type { KeyValueStore, Spool, SpoolItem, Telemetry } from './client.js'

/** Default spool cap: 4 MiB. */
export const DEFAULT_SPOOL_BYTES = 4 << 20

function ensureDir(path: string): void {
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 })
}

/** A spool in one NDJSON file, at most `maxBytes` of live records (default 4 MiB). */
export function fileSpool(path: string, opts: { maxBytes?: number } = {}): Spool {
  return {
    maxBytes: opts.maxBytes ?? DEFAULT_SPOOL_BYTES,
    load(): SpoolItem[] {
      let raw: string
      try {
        raw = readFileSync(path, 'utf8')
      } catch {
        return []
      }
      const out: SpoolItem[] = []
      for (const line of raw.split('\n')) {
        if (!line) continue
        try {
          out.push(JSON.parse(line) as SpoolItem)
        } catch {
          // A line cut short by a crash: skipped, and gone at the next rewrite.
        }
      }
      return out
    },
    append(items: SpoolItem[]): void {
      ensureDir(path)
      appendFileSync(path, items.map((i) => `${JSON.stringify(i)}\n`).join(''), { mode: 0o600 })
    },
    replace(items: SpoolItem[]): void {
      ensureDir(path)
      const tmp = `${path}.${process.pid}.tmp`
      writeFileSync(tmp, items.map((i) => `${JSON.stringify(i)}\n`).join(''), { mode: 0o600 })
      renameSync(tmp, path)
    },
  }
}

/** Small values in one JSON file (the install id). */
export function fileStore(path: string): KeyValueStore {
  const read = (): Record<string, string> => {
    try {
      const v = JSON.parse(readFileSync(path, 'utf8')) as unknown
      return v && typeof v === 'object' ? (v as Record<string, string>) : {}
    } catch {
      return {}
    }
  }
  return {
    get: (k) => read()[k] ?? null,
    set(k, v) {
      ensureDir(path)
      const tmp = `${path}.${process.pid}.tmp`
      writeFileSync(tmp, JSON.stringify({ ...read(), [k]: v }), { mode: 0o600 })
      renameSync(tmp, path)
    },
  }
}

/** The OS name for `resource.os`, from process.platform (its version is the app's to give). */
export function nodeOsName(): string {
  const names: Record<string, string> = { darwin: 'macOS', linux: 'Linux', win32: 'Windows', freebsd: 'FreeBSD', openbsd: 'OpenBSD', android: 'Android' }
  return names[process.platform] ?? process.platform
}

/** Flushes when the event loop empties (`beforeExit`). Returns a function that removes the hook. */
export function flushOnExit(telemetry: Telemetry): () => void {
  let done = false
  const onExit = () => {
    if (done) return
    done = true
    void telemetry.shutdown()
  }
  process.on('beforeExit', onExit)
  return () => process.off('beforeExit', onExit)
}
