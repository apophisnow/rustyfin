// Rustyfin Service Worker: cache-first for content-hashed assets and for tagged images.
// Everything else (index.html, sw.js, logo.png, API, untagged images) goes to the network.
const CACHE_NAME = 'rustyfin-v__CACHE_VERSION__';
const HASHED_ASSET = /^\/web\/assets\/.+-[A-Za-z0-9_-]{8}\.[a-z0-9]+$/;
// Tagged images name one version of a picture, so they never change. Their cache outlives app
// updates, and is trimmed oldest first to IMAGE_CACHE_MAX entries.
const IMAGE_CACHE = 'rustyfin-images-v1';
const IMAGE_PATH = /^\/Items\/[^/]+\/Images\/[^/]+(\/\d+)?$/;
const IMAGE_CACHE_MAX = 3000;
let imagePuts = 0;

self.addEventListener('install', () => {
  self.skipWaiting();
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys().then((keys) =>
      Promise.all(
        keys
          .filter((key) => key !== CACHE_NAME && key !== IMAGE_CACHE)
          .map((key) => caches.delete(key))
      )
    )
  );
  self.clients.claim();
});

// The cache key of a tagged image request, or null. The key leaves out api_key, so a new sign-in
// reuses the pictures already on the device.
function imageKey(url) {
  if (url.origin !== self.location.origin || !IMAGE_PATH.test(url.pathname)) return null;
  if (!url.searchParams.get('tag')) return null;
  const key = new URL(url);
  key.searchParams.delete('api_key');
  key.searchParams.sort();
  return key.href;
}

async function trimImages(cache) {
  const keys = await cache.keys();
  for (const key of keys.slice(0, Math.max(0, keys.length - IMAGE_CACHE_MAX))) await cache.delete(key);
}

// Serves a tagged image from the cache, else from the network, storing it while it is shown. The
// request keeps its signal, so a picture the page stops loading is aborted here too.
async function cachedImage(event, key) {
  const cache = await caches.open(IMAGE_CACHE);
  const cached = await cache.match(key);
  if (cached) return cached;
  const response = await fetch(event.request);
  if (response.ok && response.type === 'basic') {
    event.waitUntil(
      cache.put(key, response.clone()).then(() => (++imagePuts % 100 === 0 ? trimImages(cache) : undefined))
    );
  }
  return response;
}

self.addEventListener('fetch', (event) => {
  const { request } = event;
  if (request.method !== 'GET') return;
  const url = new URL(request.url);
  const key = imageKey(url);
  if (key) {
    event.respondWith(cachedImage(event, key));
    return;
  }
  if (!HASHED_ASSET.test(url.pathname)) return;

  event.respondWith(
    caches.open(CACHE_NAME).then((cache) =>
      cache.match(request).then((cached) => {
        if (cached) return cached;
        return fetch(request).then((response) => {
          if (response.ok) cache.put(request, response.clone());
          return response;
        });
      })
    )
  );
});
