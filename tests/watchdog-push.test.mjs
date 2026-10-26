import test from 'node:test'
import assert from 'node:assert/strict'
import vm from 'node:vm'
import fs from 'node:fs'

function worker() {
  const handlers = {}
  const shown = []
  const opened = []
  const self = {
    location: { origin: 'https://router.test' },
    addEventListener: (name, handler) => { handlers[name] = handler },
    registration: { showNotification: async (title, options) => { shown.push({ title, options }) } },
    clients: { matchAll: async () => [], openWindow: async (url) => { opened.push(url) } },
  }
  vm.runInNewContext(fs.readFileSync(new URL('../frontend/public/sw.js', import.meta.url), 'utf8'), { self, URL, console })
  return { handlers, shown, opened }
}

test('push shows a notification without fetching or installing an update', async () => {
  const { handlers, shown } = worker()
  let pending
  assert.equal(typeof handlers.push, 'function')
  handlers.push({ data: { json: () => ({ title: 'Watchdog', body: '1.0.11-1', url: 'https://evil.test' }) }, waitUntil: (promise) => { pending = promise } })
  await pending
  assert.equal(shown[0].options.data.url, '/#watchdog-update')
  assert.equal(shown[0].options.body, '1.0.11-1')
})

test('notification click opens only the local update panel', async () => {
  const { handlers, opened } = worker()
  let closed = false
  let pending
  assert.equal(typeof handlers.notificationclick, 'function')
  handlers.notificationclick({ notification: { close: () => { closed = true }, data: { url: 'https://evil.test' } }, waitUntil: (promise) => { pending = promise } })
  await pending
  assert.ok(closed)
  assert.equal(opened[0], 'https://router.test/#watchdog-update')
})


test('missing or malformed push data still shows a safe update notification', async () => {
  for (const data of [null, { json: () => null }, { json: () => { throw new Error('malformed') } }]) {
    const { handlers, shown } = worker()
    let pending
    handlers.push({ data, waitUntil: (promise) => { pending = promise } })
    await pending
    assert.equal(shown[0].options.data.url, '/#watchdog-update')
    assert.equal(typeof shown[0].options.body, 'string')
  }
})
