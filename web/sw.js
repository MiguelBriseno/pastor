// Guarda la página para que abra sin red; la API nunca se guarda.
// Primero la red: así una versión nueva de pastor se ve al recargar. Si la
// red no contesta en 3 s se usa lo guardado, para no esperar el timeout del
// navegador; la respuesta que llegue después igual actualiza la caché.
const CACHE = "pastor-v3";
const WAIT = 3000;
const API = new URL("api/", self.registration.scope).pathname;
const SHELL = ["./", "app.js", "style.css", "manifest.webmanifest", "icon-192.png", "icon-512.png"];

self.addEventListener("install", (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)));
  self.skipWaiting();
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys().then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
  );
  self.clients.claim();
});

self.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== "GET" || url.origin !== location.origin || url.pathname.startsWith(API)) return;
  const net = fetch(e.request);
  e.waitUntil(
    net
      .then((res) => {
        if (!res.ok) return;
        const copy = res.clone();
        return caches.open(CACHE).then((c) => c.put(e.request, copy));
      })
      .catch(() => {})
  );
  // Lo guardado solo gana si existe; si no, se sigue esperando a la red.
  const cached = () => caches.match(e.request, { ignoreSearch: true }).then((r) => r || net);
  const slow = new Promise((ok) => setTimeout(ok, WAIT)).then(cached);
  e.respondWith(Promise.race([net.catch(cached), slow]));
});
