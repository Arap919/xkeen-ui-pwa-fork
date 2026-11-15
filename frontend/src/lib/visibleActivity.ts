type VisiblePage = Pick<Document, 'visibilityState' | 'addEventListener' | 'removeEventListener'>

/** Poll only while visible; a visibility refresh waits for any in-flight request. */
export function startVisiblePolling(refresh: () => unknown | Promise<unknown>, interval: number, page: VisiblePage = document) {
  let active = true
  let visible = page.visibilityState === 'visible'
  let running = false
  let refreshPending = false
  let timer: ReturnType<typeof setTimeout> | undefined
  const clear = () => { clearTimeout(timer); timer = undefined }
  const run = async () => {
    if (!active || !visible) return
    if (running) { refreshPending = true; return }
    clear()
    running = true
    refreshPending = false
    try { await refresh() } catch { /* Retry at the next visible interval. */ }
    finally {
      running = false
      if (active && visible) {
        if (refreshPending) void run()
        else timer = setTimeout(() => void run(), interval)
      }
    }
  }
  const changed = () => {
    const next = page.visibilityState === 'visible'
    if (next === visible) return
    visible = next
    clear()
    refreshPending = false
    if (visible) void run()
  }
  page.addEventListener('visibilitychange', changed)
  if (visible) void run()
  return () => { active = false; clear(); page.removeEventListener('visibilitychange', changed) }
}

/** Own exactly one connections socket and all of its delayed work. */
export function startVisibleSocket(
  createSocket: () => WebSocket,
  connected: (value: boolean) => void,
  message: (event: MessageEvent) => void,
  page: VisiblePage = document,
  touchRestore = false,
) {
  let active = true
  let socket: WebSocket | null = null
  let timer: ReturnType<typeof setTimeout> | undefined
  let retries = 0
  const clear = () => { clearTimeout(timer); timer = undefined }
  const close = () => {
    clear()
    if (socket) {
      socket.onopen = socket.onclose = socket.onerror = socket.onmessage = null
      if (socket.readyState === 0 || socket.readyState === 1) socket.close()
      socket = null
    }
    connected(false)
  }
  const connect = () => {
    if (!active || page.visibilityState !== 'visible' || socket?.readyState === 0 || socket?.readyState === 1) return
    clear()
    const current = createSocket()
    socket = current
    current.onopen = () => { retries = 0; connected(true) }
    current.onmessage = message
    current.onerror = () => current.close()
    current.onclose = () => {
      if (socket !== current) return
      socket = null
      connected(false)
      if (active && page.visibilityState === 'visible' && retries < 12) {
        retries++
        timer = setTimeout(connect, Math.min(800 * Math.pow(1.4, retries), 12000))
      }
    }
  }
  const changed = () => {
    if (page.visibilityState !== 'visible') close()
    else { retries = 0; connect() }
  }
  page.addEventListener('visibilitychange', changed)
  if (touchRestore) page.addEventListener('touchstart', connect)
  if (page.visibilityState === 'visible') timer = setTimeout(connect, 200)
  return () => {
    active = false
    close()
    page.removeEventListener('visibilitychange', changed)
    if (touchRestore) page.removeEventListener('touchstart', connect)
  }
}
