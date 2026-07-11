// Minimal service worker: network-first with cache fallback. Online you
// always get the freshest bundle (no stale-PWA hell); offline the last-seen
// shell still opens. Nothing here is versioned on purpose — the network is
// the source of truth, the cache is only a fallback.
const CACHE = 'cb-shell-v1'

self.addEventListener('install', (e) => {
  self.skipWaiting()
})

self.addEventListener('activate', (e) => {
  e.waitUntil(self.clients.claim())
})

self.addEventListener('fetch', (e) => {
  const req = e.request
  if (req.method !== 'GET' || new URL(req.url).origin !== self.location.origin) return
  e.respondWith(
    fetch(req)
      .then((res) => {
        const copy = res.clone()
        caches.open(CACHE).then((c) => c.put(req, copy))
        return res
      })
      .catch(() =>
        caches.match(req).then((hit) => hit ?? caches.match('/index.html'))
      )
  )
})
