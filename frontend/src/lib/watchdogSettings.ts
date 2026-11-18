export type WatchdogConfig = Record<string, unknown>
export type ScheduleWindow = { start: string; end: string; every_minutes: number | string }
export const watchdogNumbers = [
  { key: 'top_n', label: 'Число кандидатов', min: 1, max: 50, unit: '1–50 узлов' },
  { key: 'healthcheck_parallelism', label: 'Параллельные проверки', min: 1, max: 32, unit: '1–32 проверки' },
  { key: 'healthcheck_timeout_ms', label: 'Таймаут проверки доступности', min: 250, max: 10000, unit: '250–10000 мс' },
  { key: 'speed_connect_timeout_seconds', label: 'Таймаут соединения замера', min: 1, max: 30, unit: '1–30 с' },
  { key: 'switch_wait_seconds', label: 'Пауза после переключения', min: 0, unit: 'секунды, от 0', fractional: true },
  { key: 'download_bytes', label: 'Объём на поток', min: 1000000, max: 200000000, unit: '1000000–200000000 байт' },
  { key: 'parallel_streams', label: 'Параллельные потоки', min: 1, max: 8, unit: '1–8 потоков' },
  { key: 'multi_rounds', label: 'Раунды измерения', min: 1, max: 4, unit: '1–4 раунда' },
  { key: 'log_cleanup_days', label: 'Интервал очистки журнала', min: 1, max: 3650, unit: '1–3650 дней' },
]

export function watchdogPatch(config: WatchdogConfig, secret: string, remove: boolean) {
  const safeConfig = { ...config }
  delete safeConfig.secret
  if (remove || secret !== '') safeConfig.secret = remove ? '' : secret
  return { config: safeConfig }
}

export function validateWatchdog(config: WatchdogConfig): Record<string, string> {
  const errors: Record<string, string> = {}
  for (const field of watchdogNumbers) {
    if (!(field.key in config)) continue
    const raw = config[field.key]
    const n = Number(raw)
    if (raw === '' || !Number.isFinite(n) || n < field.min || (field.max !== undefined && n > field.max) || (!field.fractional && !Number.isInteger(n))) {
      errors[field.key] = `Допустимо: ${field.unit}`
    }
  }
  for (const key of ['controller', 'benchmark_proxy']) {
    if (!(key in config)) continue
    try {
      const text = String(config[key])
      const url = new URL(text)
      if (!['http:', 'https:'].includes(url.protocol) || !url.hostname || url.username || url.password || /\s/.test(text)) throw new Error()
    } catch { errors[key] = 'Укажите URL HTTP(S) без логина и пароля' }
  }
  for (const key of ['target_group', 'benchmark_group']) {
    if (key in config && !String(config[key]).trim()) errors[key] = 'Укажите имя группы'
  }
  if (config.target_group && config.target_group === config.benchmark_group) errors.benchmark_group = 'Тестовая и рабочая группы должны отличаться'
  if ('speed_test_provider' in config && !['cloudflare', 'speedtest'].includes(String(config.speed_test_provider))) errors.speed_test_provider = 'Выберите Cloudflare или speedtest'
  if ('schedule' in config) {
    const windows = (config.schedule as { windows?: ScheduleWindow[] })?.windows
    if (!Array.isArray(windows) || windows.length === 0) errors.schedule = 'Добавьте хотя бы одно окно расписания'
    else windows.forEach((window, index) => {
      for (const key of ['start', 'end'] as const) {
        if (!/^([01]\d|2[0-3]):[0-5]\d$/.test(window[key])) errors[`schedule.${index}.${key}`] = 'Время в формате HH:MM (00:00–23:59)'
      }
      const n = Number(window.every_minutes)
      if (!Number.isInteger(n) || n < 1 || n > 1440) errors[`schedule.${index}.every_minutes`] = 'Интервал 1–1440 минут'
    })
  }
  return errors
}
