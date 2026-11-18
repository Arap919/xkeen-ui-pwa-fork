import { test } from 'node:test'
import assert from 'node:assert/strict'
import { startVisiblePolling, startVisibleSocket } from '../src/lib/visibleActivity.ts'
import { watchdogPatch, validateWatchdog } from '../src/lib/watchdogSettings.ts'

class Page extends EventTarget {
  visibilityState = 'visible'
  show(value) { this.visibilityState = value; this.dispatchEvent(new Event('visibilitychange')) }
}
const flush = async () => { await Promise.resolve(); await Promise.resolve(); await Promise.resolve() }

test('polling pauses hidden and refreshes once on return without overlap', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] })
  const page = new Page()
  let calls = 0
  let finish
  const stop = startVisiblePolling(() => { calls++; return new Promise(resolve => { finish = resolve }) }, 3000, page)
  assert.equal(calls, 1)
  t.mock.timers.tick(10000)
  assert.equal(calls, 1)
  page.show('hidden'); page.show('visible'); page.show('visible')
  assert.equal(calls, 1)
  finish(); await flush()
  assert.equal(calls, 2)
  page.show('hidden'); finish(); await flush(); t.mock.timers.tick(10000)
  assert.equal(calls, 2)
  page.show('visible'); assert.equal(calls, 3)
  stop(); finish(); await flush(); t.mock.timers.tick(10000)
  assert.equal(calls, 3)
})

test('connections cancel startup/retry, close hidden, and do not duplicate Safari restoration', (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] })
  const page = new Page()
  const sockets = []
  const connected = []
  const create = () => {
    const socket = { readyState: 0, close() { this.readyState = 3; this.closed = true } }
    sockets.push(socket); return socket
  }
  const stop = startVisibleSocket(create, value => connected.push(value), () => {}, page, true)
  page.show('hidden'); t.mock.timers.tick(1000); assert.equal(sockets.length, 0)
  page.show('visible'); page.dispatchEvent(new Event('touchstart')); page.show('visible')
  assert.equal(sockets.length, 1)
  sockets[0].readyState = 1; sockets[0].onopen()
  page.show('hidden'); assert.equal(sockets[0].closed, true); assert.equal(connected.at(-1), false)
  page.show('visible'); assert.equal(sockets.length, 2)
  sockets[1].readyState = 3; sockets[1].onclose()
  page.show('hidden'); t.mock.timers.tick(20000); assert.equal(sockets.length, 2)
  page.show('visible'); assert.equal(sockets.length, 3)
  stop(); t.mock.timers.tick(20000); page.show('visible'); assert.equal(sockets.length, 3)
  assert.equal(sockets[2].closed, true)
})

test('saving omits blank secret, replacement and explicit removal are distinct', () => {
  const config = { controller: 'http://localhost:9090', top_n: 3 }
  assert.deepEqual(watchdogPatch(config, '', false), { config })
  assert.deepEqual(watchdogPatch(config, 'new-secret', false), { config: { ...config, secret: 'new-secret' } })
  assert.deepEqual(watchdogPatch(config, '', true), { config: { ...config, secret: '' } })
  assert.equal('enabled' in watchdogPatch(config, '', false), false)
})

test('validation accepts overnight windows and flags each invalid range', () => {
  assert.deepEqual(validateWatchdog({ schedule: { windows: [{ start: '23:00', end: '02:00', every_minutes: 5 }] } }), {})
  const errors = validateWatchdog({ top_n: 51, switch_wait_seconds: Infinity, schedule: { windows: [{ start: '25:00', end: '01:00', every_minutes: 0 }] } })
  assert.ok(errors.top_n); assert.ok(errors.switch_wait_seconds)
  assert.ok(errors['schedule.0.start']); assert.ok(errors['schedule.0.every_minutes'])
})
