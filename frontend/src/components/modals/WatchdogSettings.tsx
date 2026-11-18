import { useEffect, useState } from 'react'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import { Field, FieldContent, FieldDescription, FieldError, FieldLabel } from '@/components/ui/field'
import { Input } from '@/components/ui/input'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Switch } from '@/components/ui/switch'
import { apiCall } from '@/lib/api'
import { validateWatchdog, watchdogNumbers, watchdogPatch } from '@/lib/watchdogSettings'
import type { ScheduleWindow, WatchdogConfig } from '@/lib/watchdogSettings'

type ConfigResponse = {
  success: boolean
  enabled: boolean
  config?: WatchdogConfig | null
  editable_fields?: string[]
  secret_configured?: boolean
  needs_configuration?: boolean
}
type SaveResponse = { success: boolean; error?: string }
const textFields = [
  { key: 'controller', label: 'API Mihomo', description: 'URL HTTP(S), например http://127.0.0.1:9090' },
  { key: 'target_group', label: 'Рабочая группа', description: 'Production-группа для выбора узла' },
  { key: 'benchmark_group', label: 'Тестовая группа', description: 'Отдельная группа для измерений, отличная от рабочей' },
  { key: 'benchmark_proxy', label: 'Прокси для измерений', description: 'URL HTTP(S), например http://127.0.0.1:17890' },
]
const fieldKeys = [...textFields.map(field => field.key), ...watchdogNumbers.map(field => field.key), 'speed_test_provider', 'schedule']

export function WatchdogSettings() {
  const [enabled, setEnabled] = useState(true)
  const [draft, setDraft] = useState<WatchdogConfig>({})
  const [editable, setEditable] = useState<string[]>([])
  const [secretConfigured, setSecretConfigured] = useState(false)
  const [secret, setSecret] = useState('')
  const [removeSecret, setRemoveSecret] = useState(false)
  const [needsConfiguration, setNeedsConfiguration] = useState(false)
  const [loading, setLoading] = useState(true)
  const [loaded, setLoaded] = useState(false)
  const [saving, setSaving] = useState(false)
  const [toggling, setToggling] = useState(false)
  const [reload, setReload] = useState(0)
  const [error, setError] = useState('')
  const [saved, setSaved] = useState(false)
  const [errors, setErrors] = useState<Record<string, string>>({})

  useEffect(() => {
    let active = true
    setLoading(true)
    setError('')
    apiCall<ConfigResponse>('GET', 'watchdog/config').then(result => {
      if (!active) return
      if (!result.success) throw new Error('load_failed')
      const config = result.config ?? {}
      // Only public supported fields may enter the form or a subsequent patch.
      setDraft(Object.fromEntries(fieldKeys.map(key => [key, config[key] ?? (key === 'schedule' ? { windows: [] } : '')])))
      setEnabled(result.enabled !== false)
      setEditable(result.editable_fields ?? [...fieldKeys, 'secret'])
      setSecretConfigured(result.secret_configured === true)
      setNeedsConfiguration(result.needs_configuration === true || !result.config || Object.keys(config).length === 0)
      setLoaded(true)
    }).catch(() => { if (active) setError('Не удалось загрузить настройки watchdog.') })
      .finally(() => { if (active) setLoading(false) })
    return () => { active = false }
  }, [reload])

  const busy = loading || saving || toggling
  const windows = (draft.schedule as { windows?: ScheduleWindow[] } | undefined)?.windows ?? []
  const change = (key: string, value: unknown) => {
    setDraft(current => ({ ...current, [key]: value }))
    setErrors({})
    setSaved(false)
    setError('')
  }
  const changeWindow = (index: number, key: keyof ScheduleWindow, value: string) => {
    change('schedule', { windows: windows.map((window, i) => i === index ? { ...window, [key]: value } : window) })
  }

  async function toggle(value: boolean) {
    if (busy) return
    setToggling(true)
    setError('')
    try {
      // The switch never submits the unsaved form.
      const result = await apiCall<SaveResponse>('PATCH', 'watchdog/config', { enabled: value })
      if (!result.success) throw new Error('toggle_failed')
      setEnabled(value)
    } catch { setError('Не удалось изменить состояние watchdog. Прежнее состояние сохранено.') }
    finally { setToggling(false) }
  }

  async function save() {
    if (busy) return
    const nextErrors = validateWatchdog(draft)
    setErrors(nextErrors)
    setSaved(false)
    setError('')
    if (Object.keys(nextErrors).length > 0) return
    const config = Object.fromEntries(Object.entries(draft).filter(([key]) => editable.includes(key)))
    for (const field of watchdogNumbers) if (field.key in config) config[field.key] = Number(config[field.key])
    if ('schedule' in config) config.schedule = { windows: windows.map(window => ({ ...window, every_minutes: Number(window.every_minutes) })) }
    setSaving(true)
    try {
      const result = await apiCall<SaveResponse>('PATCH', 'watchdog/config', watchdogPatch(config, secret, removeSecret))
      if (!result.success) {
        const key = result.error?.replace(/^invalid_/, '')
        if (key && fieldKeys.includes(key)) setErrors({ [key]: 'Проверьте значение: сервер отклонил настройку.' })
        throw new Error('save_failed')
      }
      setSecretConfigured(removeSecret ? false : secret !== '' ? true : secretConfigured)
      setSecret('')
      setRemoveSecret(false)
      setNeedsConfiguration(false)
      setSaved(true)
    } catch { setError('Не удалось сохранить настройки. Изменения не применены; черновик оставлен в форме.') }
    finally { setSaving(false) }
  }

  return (
    <div className="space-y-4 py-3">
      <Field orientation="horizontal">
        <FieldContent>
          <FieldLabel htmlFor="watchdog-enabled">Включить watchdog</FieldLabel>
          <FieldDescription>Автоматические проверки по сохранённому расписанию. Watchdog обновляется вместе с панелью.</FieldDescription>
        </FieldContent>
        <Switch id="watchdog-enabled" checked={enabled} disabled={busy || !loaded} onCheckedChange={value => void toggle(value)} />
      </Field>
      {loading && <p className="text-muted-foreground text-sm" role="status">Загрузка настроек…</p>}
      {error && <Alert variant="destructive"><AlertDescription role="alert">{error}</AlertDescription></Alert>}
      {!loading && !loaded && <Button variant="outline" onClick={() => setReload(value => value + 1)}>Повторить загрузку</Button>}
      {loaded && <>
        {needsConfiguration && <Alert><AlertDescription>Watchdog требует настройки. Заполните поля и сохраните конфигурацию; до этого автоматические проверки не выполняются.</AlertDescription></Alert>}
        <form className="space-y-4" onSubmit={event => { event.preventDefault(); void save() }} noValidate>
          <fieldset disabled={busy} className="min-w-0 space-y-4">
            {textFields.map(field => <Field key={field.key}>
              <FieldLabel htmlFor={`watchdog-${field.key}`}>{field.label}</FieldLabel>
              <Input id={`watchdog-${field.key}`} value={String(draft[field.key] ?? '')} disabled={!editable.includes(field.key)} onChange={event => change(field.key, event.target.value)} aria-invalid={!!errors[field.key]} aria-describedby={`watchdog-${field.key}-hint`} autoComplete="off" />
              <FieldDescription id={`watchdog-${field.key}-hint`}>{field.description}</FieldDescription>
              {errors[field.key] && <FieldError>{errors[field.key]}</FieldError>}
            </Field>)}
            <Field>
              <FieldLabel htmlFor="watchdog-secret">Секрет API Mihomo</FieldLabel>
              <Input id="watchdog-secret" type="password" autoComplete="new-password" value={secret} disabled={removeSecret || !editable.includes('secret')} onChange={event => { setSecret(event.target.value); setSaved(false) }} />
              <FieldDescription>{secretConfigured ? 'Секрет настроен.' : 'Секрет не настроен.'} Пустое поле оставляет существующий секрет. Введите новый для замены.</FieldDescription>
            </Field>
            <Field orientation="horizontal">
              <FieldContent><FieldLabel htmlFor="watchdog-remove-secret">Удалить сохранённый секрет при сохранении</FieldLabel></FieldContent>
              <Switch id="watchdog-remove-secret" checked={removeSecret} disabled={!editable.includes('secret')} onCheckedChange={value => { setRemoveSecret(value); setSaved(false) }} />
            </Field>
            <Field>
              <FieldLabel htmlFor="watchdog-provider">Сервис измерения скорости</FieldLabel>
              <Select value={String(draft.speed_test_provider ?? '')} disabled={!editable.includes('speed_test_provider')} items={{ cloudflare: 'Cloudflare', speedtest: 'speedtest' }} onValueChange={value => change('speed_test_provider', value)}>
                <SelectTrigger id="watchdog-provider" className="w-full" aria-invalid={!!errors.speed_test_provider}><SelectValue placeholder="Выберите сервис" /></SelectTrigger>
                <SelectContent><SelectItem value="cloudflare">Cloudflare</SelectItem><SelectItem value="speedtest">speedtest</SelectItem></SelectContent>
              </Select>
              {errors.speed_test_provider && <FieldError>{errors.speed_test_provider}</FieldError>}
            </Field>
            {watchdogNumbers.map(field => <Field key={field.key}>
              <FieldLabel htmlFor={`watchdog-${field.key}`}>{field.label}</FieldLabel>
              <Input id={`watchdog-${field.key}`} type="number" min={field.min} max={field.max} step={field.fractional ? 'any' : 1} value={String(draft[field.key] ?? '')} disabled={!editable.includes(field.key)} onChange={event => change(field.key, event.target.value)} aria-invalid={!!errors[field.key]} aria-describedby={`watchdog-${field.key}-hint`} />
              <FieldDescription id={`watchdog-${field.key}-hint`}>{field.unit}</FieldDescription>
              {errors[field.key] && <FieldError>{errors[field.key]}</FieldError>}
            </Field>)}
            <div className="space-y-3">
              <p className="text-sm font-medium">Окна расписания</p>
              <p className="text-muted-foreground text-xs">Время роутера, HH:MM. Допустим переход через полночь. Интервал — 1–1440 минут.</p>
              {windows.map((window, index) => <fieldset key={index} className="min-w-0 space-y-3 rounded-lg border p-3" disabled={!editable.includes('schedule')}>
                <legend className="px-1 text-xs">Окно {index + 1}</legend>
                <div className="grid grid-cols-2 gap-3">
                  {(['start', 'end'] as const).map(key => <Field key={key}>
                    <FieldLabel htmlFor={`watchdog-window-${index}-${key}`}>{key === 'start' ? 'Начало' : 'Конец'}</FieldLabel>
                    <Input id={`watchdog-window-${index}-${key}`} type="time" value={window[key]} onChange={event => changeWindow(index, key, event.target.value)} aria-invalid={!!errors[`schedule.${index}.${key}`]} />
                    {errors[`schedule.${index}.${key}`] && <FieldError>{errors[`schedule.${index}.${key}`]}</FieldError>}
                  </Field>)}
                </div>
                <Field>
                  <FieldLabel htmlFor={`watchdog-window-${index}-interval`}>Интервал, минуты</FieldLabel>
                  <Input id={`watchdog-window-${index}-interval`} type="number" min={1} max={1440} step={1} value={window.every_minutes} onChange={event => changeWindow(index, 'every_minutes', event.target.value)} aria-invalid={!!errors[`schedule.${index}.every_minutes`]} />
                  {errors[`schedule.${index}.every_minutes`] && <FieldError>{errors[`schedule.${index}.every_minutes`]}</FieldError>}
                </Field>
                <Button type="button" variant="outline" onClick={() => change('schedule', { windows: windows.filter((_, i) => i !== index) })}>Удалить окно {index + 1}</Button>
              </fieldset>)}
              {errors.schedule && <FieldError>{errors.schedule}</FieldError>}
              <Button type="button" variant="outline" disabled={!editable.includes('schedule')} onClick={() => change('schedule', { windows: [...windows, { start: '00:00', end: '23:59', every_minutes: 1 }] })}>Добавить окно</Button>
            </div>
          </fieldset>
          <p className="text-muted-foreground text-xs">Параметры применяются только кнопкой «Сохранить». Переключатель выше применяется отдельно.</p>
          <Button type="submit" disabled={busy}>{saving ? 'Сохранение…' : 'Сохранить'}</Button>
          {saved && <p className="text-sm text-emerald-500" role="status">Настройки сохранены.</p>}
        </form>
      </>}
    </div>
  )
}
