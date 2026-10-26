import { useCallback, useEffect, useState } from 'react'
import { apiCall } from '@/lib/api'
import { Button } from '@/components/ui/button'

type UpdateStatus = {
  installed_version: string | null
  latest_version: string
  update_available: boolean
  release_url: string
  release_notes?: string | null
  checked_at: string
}
type Result = { success: boolean; error?: string; data?: UpdateStatus; installing?: boolean; push_available?: boolean; public_key?: string }

async function request(method: string, endpoint: string, body?: unknown): Promise<Result> {
  const result = await apiCall<Result>(method, `watchdog/${endpoint}`, body)
  if (!result.success) throw new Error(result.error || 'Запрос не выполнен')
  return result
}

function applicationKey(key: string): Uint8Array<ArrayBuffer> {
  return Uint8Array.from(atob(key.replace(/-/g, '+').replace(/_/g, '/')), (char) => char.charCodeAt(0))
}

export function WatchdogUpdates() {
  const [status, setStatus] = useState<UpdateStatus | null>(null)
  const [expanded, setExpanded] = useState(window.location.hash === '#watchdog-update')
  const [busy, setBusy] = useState(false)
  const [installing, setInstalling] = useState(false)
  const [error, setError] = useState('')
  const [message, setMessage] = useState('')
  const [pushAvailable, setPushAvailable] = useState(false)
  const [subscription, setSubscription] = useState<PushSubscription | null>(null)
  const [pushBusy, setPushBusy] = useState(false)
  const pushSupported = window.isSecureContext && 'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window

  const refresh = useCallback(async () => {
    setBusy(true)
    setError('')
    try {
      const result = await request('GET', 'update')
      setStatus(result.data ?? null)
      setInstalling(result.installing ?? false)
      setPushAvailable(result.push_available ?? false)
      return true
    } catch (error) {
      setError(error instanceof Error ? error.message : 'Не удалось проверить обновление')
      return false
    } finally { setBusy(false) }
  }, [])

  useEffect(() => {
    void refresh()
    const timer = setInterval(() => void refresh(), 6 * 60 * 60 * 1000)
    const openFromNotification = () => {
      if (window.location.hash === '#watchdog-update') {
        setExpanded(true)
        document.getElementById('watchdog-update')?.scrollIntoView({ block: 'center' })
      }
    }
    window.addEventListener('hashchange', openFromNotification)
    if (pushSupported) {
      void navigator.serviceWorker.ready.then((registration) => registration.pushManager.getSubscription()).then(setSubscription).catch(() => {})
    }
    return () => { clearInterval(timer); window.removeEventListener('hashchange', openFromNotification) }
  }, [refresh, pushSupported])

  async function install() {
    if (!status?.update_available || !status.installed_version) return
    const version = status.latest_version
    if (!window.confirm(`Установить router-watchdog ${version} на локальном роутере?`)) return
    setInstalling(true)
    setError('')
    setMessage('Установка может занять несколько минут. Закрытие страницы не отменяет установку.')
    try {
      await request('POST', 'update', { expected_version: version })
      setMessage('Установка завершена. Проверяем установленную версию…')
      const verified = await refresh()
      setMessage(verified ? 'Установка завершена.' : 'Установка завершена. Не удалось проверить версию; нажмите «Проверить».')
    } catch (error) {
      setError(`${error instanceof Error ? error.message : 'Связь потеряна'}. Нажмите «Проверить», чтобы узнать результат.`)
    } finally { setInstalling(false) }
  }

  async function enablePush() {
    if (!pushSupported) return
    // Request permission directly inside the user's click handler.
    const permission = Notification.requestPermission()
    setPushBusy(true)
    setError('')
    try {
      if (await permission !== 'granted') throw new Error('Разрешите уведомления в настройках браузера, затем повторите подписку')
      const result = await request('POST', 'push-key', {})
      if (!result.public_key) throw new Error('Сервер не предоставил ключ Push')
      const registration = await navigator.serviceWorker.ready
      const current = await registration.pushManager.getSubscription()
      const next = current ?? await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: applicationKey(result.public_key) })
      await request('POST', 'subscription', next.toJSON())
      setSubscription(next)
      setMessage('Уведомления включены для этого браузера. Сервер проверяет релизы каждые 6 часов.')
    } catch (error) {
      setError(error instanceof Error ? error.message : 'Не удалось включить уведомления')
    } finally { setPushBusy(false) }
  }

  async function disablePush() {
    if (!subscription) return
    setPushBusy(true)
    setError('')
    try {
      await request('DELETE', 'subscription', { endpoint: subscription.endpoint })
      await subscription.unsubscribe()
      setSubscription(null)
      setMessage('Уведомления отключены для этого браузера.')
    } catch (error) {
      setError(error instanceof Error ? error.message : 'Не удалось отключить уведомления')
    } finally { setPushBusy(false) }
  }

  return (
    <section id="watchdog-update" className="bg-card rounded-xl border p-3 text-sm" aria-label="Обновления router-watchdog">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <button type="button" className="cursor-pointer text-left font-medium" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>
          router-watchdog · {status?.update_available ? `Доступна версия ${status.latest_version}` : status ? `Установлено ${status.installed_version ?? 'не установлено'}` : 'Проверка обновлений'}
        </button>
        <Button size="sm" variant="outline" onClick={() => setExpanded(!expanded)}>{expanded ? 'Свернуть' : 'Подробнее'}</Button>
      </div>
      {(expanded || status?.update_available) && (
        <div className="mt-3 flex flex-col gap-3">
          <p className="text-muted-foreground">Локальный роутер. {status ? `Установлено: ${status.installed_version ?? 'пакет не установлен'}. Последняя версия: ${status.latest_version}.` : 'Проверка пока недоступна.'}</p>
          {status && <p className="text-muted-foreground text-xs">Проверено: {new Date(status.checked_at).toLocaleString()}</p>}
          {status?.release_url && <a className="text-primary underline" href={status.release_url} target="_blank" rel="noopener noreferrer">Описание релиза {status.latest_version}</a>}
          {status?.release_notes && <pre className="max-h-48 overflow-y-auto font-sans whitespace-pre-wrap">{status.release_notes}</pre>}
          <div className="flex flex-wrap gap-2">
            <Button size="sm" variant="outline" disabled={busy} onClick={() => void refresh()}>{busy ? 'Проверка…' : 'Проверить'}</Button>
            {status?.update_available && <Button size="sm" disabled={installing || !status.installed_version} onClick={() => void install()}>{installing ? 'Установка…' : `Установить ${status.latest_version}`}</Button>}
            <Button size="sm" variant="outline" disabled={pushBusy || (!subscription && (!pushSupported || !pushAvailable))} onClick={() => void (subscription ? disablePush() : enablePush())}>
              {pushBusy ? 'Подождите…' : subscription ? 'Отключить уведомления' : 'Включить уведомления'}
            </Button>
          </div>
          <p className="text-muted-foreground text-xs">Уведомления включаются отдельно для каждого браузера. Нужен HTTPS; на iPhone/iPad откройте установленное на главный экран веб-приложение. Установка выполняется только по кнопке.</p>
          {!pushSupported && <p className="text-muted-foreground text-xs">Web Push недоступен в этом браузере или режиме. Проверка в панели работает независимо.</p>}
          {message && <p role="status">{message}</p>}
        </div>
      )}
      {error && <p role="alert" className="text-destructive mt-2">{error}</p>}
    </section>
  )
}
