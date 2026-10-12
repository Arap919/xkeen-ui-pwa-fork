const CACHE_PREFIX = 'xkeen-ui-pwa-'
const CACHE_NAME = `${CACHE_PREFIX}v1`
const APP_SHELL = [
  '/',
  '/manifest.webmanifest',
  '/icons/apple-touch-icon.png',
  '/icons/icon-192.png',
  '/icons/icon-512.png',
]
const EXCLUDED_PATH = /^\/(api|ws|clash|clash-ws)(\/|$)/
const STATIC_DESTINATIONS = new Set(['font', 'image', 'script', 'style'])

function cacheResponse(request, response) {
  if (!response.ok) {
    return Promise.resolve(response)
  }

  return caches
    .open(CACHE_NAME)
    .then((cache) => cache.put(request, response.clone()))
    .then(() => response)
    .catch((error) => {
      console.error('Failed to cache a PWA response:', error)
      return response
    })
}

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(CACHE_NAME).then((cache) => cache.addAll(APP_SHELL)),
  )
  self.skipWaiting()
})

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((cacheNames) =>
        Promise.all(
          cacheNames
            .filter((cacheName) => cacheName.startsWith(CACHE_PREFIX) && cacheName !== CACHE_NAME)
            .map((cacheName) => caches.delete(cacheName)),
        ),
      )
      .then(() => self.clients.claim()),
  )
})

self.addEventListener('fetch', (event) => {
  const { request } = event
  const url = new URL(request.url)

  if (request.method !== 'GET' || url.origin !== self.location.origin || EXCLUDED_PATH.test(url.pathname)) {
    return
  }

  if (request.mode === 'navigate') {
    event.respondWith(
      fetch(request)
        .then((response) => cacheResponse('/', response))
        .catch(async () => (await caches.match('/')) ?? Response.error()),
    )
    return
  }

  if (!STATIC_DESTINATIONS.has(request.destination)) {
    return
  }

  event.respondWith(
    caches.match(request).then(
      (cachedResponse) =>
        cachedResponse ??
        fetch(request).then((response) => cacheResponse(request, response)),
    ),
  )
})
