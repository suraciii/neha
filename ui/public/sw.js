/* Neha's installed shell.
 *
 * The Space is the only source of the record, so this worker caches the
 * client shell and nothing behind /api. A navigation goes to the network
 * first and falls back to the last complete shell; a content-named asset is
 * served from the cache first, and a stable name revalidates.
 */

const SHELL = "neha-shell-v4";

// A shell is a successful HTML page that carries the app's entry point. A
// proxy error or a maintenance page must never replace the last good shell.
async function isShell(response) {
  if (!response.ok) return false;
  const type = response.headers.get("content-type") || "";
  if (!type.toLowerCase().includes("text/html")) return false;
  const html = await response.clone().text();
  return (
    html.includes('id="root"') &&
    /<script[^>]+src="[^"]*\/assets\/[^"]+\.(?:js|mjs)"/i.test(html)
  );
}

async function publishShell(response) {
  const html = await response.clone().text();
  const assets = [
    ...new Set([...html.matchAll(/(?:src|href)="(\/assets\/[^"]+)"/g)].map((match) => match[1])),
  ];
  if (!assets.some((asset) => asset.endsWith(".js") || asset.endsWith(".mjs"))) {
    throw new Error("the shell has no entry script");
  }
  const cache = await caches.open(SHELL);
  const fetched = await Promise.all(
    assets.map(async (asset) => {
      const assetResponse = await fetch(asset);
      if (!assetResponse.ok) throw new Error(`cannot cache ${asset}`);
      const type = (assetResponse.headers.get("content-type") || "").toLowerCase();
      if ((asset.endsWith(".js") || asset.endsWith(".mjs")) && !type.includes("javascript")) {
        throw new Error(`${asset} is not JavaScript`);
      }
      if (asset.endsWith(".css") && !type.includes("text/css")) {
        throw new Error(`${asset} is not CSS`);
      }
      return [asset, assetResponse];
    }),
  );
  for (const [asset, assetResponse] of fetched) {
    await cache.put(asset, assetResponse);
  }
  await cache.put("/", response.clone());
}

async function cacheFirst(request) {
  const cached = await caches.match(request);
  if (cached) return cached;
  try {
    const response = await fetch(request);
    if (response.ok) {
      const cache = await caches.open(SHELL);
      await cache.put(request, response.clone());
    }
    return response;
  } catch {
    return new Response("", { status: 504 });
  }
}

async function networkFirst(request) {
  const cache = await caches.open(SHELL);
  try {
    // Revalidate: the server's font and icon max-age would otherwise keep an
    // old stable name for a week.
    const response = await fetch(request, { cache: "no-cache" });
    if (response.ok) await cache.put(request, response.clone());
    return response.ok ? response : ((await cache.match(request)) ?? response);
  } catch {
    return (await cache.match(request)) ?? new Response("", { status: 504 });
  }
}

async function precache(cache, paths) {
  await Promise.allSettled(
    paths.map(async (path) => {
      const response = await fetch(path, { cache: "no-cache" });
      if (response.ok) await cache.put(path, response);
    }),
  );
}

self.addEventListener("install", (event) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(SHELL);
      // A failed update must not activate and delete the last working cache.
      const page = await fetch("/", { cache: "no-cache" });
      if (!(await isShell(page))) {
        throw new Error("the Space did not serve the shell");
      }
      await publishShell(page);
      await precache(cache, [
        "/manifest.webmanifest",
        "/sprite.svg",
        "/fonts/GeistMono-latin.woff2",
        "/fonts/NotoSansSC-subset.woff2",
        "/fonts/SchibstedGrotesk-latin.woff2",
        "/icons/icon-192.png",
        "/icons/icon-512.png",
        "/icons/maskable-512.png",
        "/icons/apple-touch-icon.png",
      ]);
      await self.skipWaiting();
    })(),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      // Activation runs only after a successful install, but a worker with no
      // published shell must never delete the last working cache.
      const cache = await caches.open(SHELL);
      if (!(await cache.match("/"))) return;
      const names = await caches.keys();
      await Promise.all(
        names
          .filter((name) => name.startsWith("neha-shell-") && name !== SHELL)
          .map((name) => caches.delete(name)),
      );
      await self.clients.claim();
    })(),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (url.origin !== location.origin) return;
  // The record, Files, and health answer only from the Space.
  if (url.pathname.startsWith("/api") || url.pathname === "/healthz") return;

  if (request.mode === "navigate") {
    event.respondWith(
      (async () => {
        const cached = await caches.match("/");
        try {
          const response = await fetch(request);
          if (url.pathname !== "/desktop.html" && (await isShell(response))) {
            try {
              await publishShell(response);
              return response;
            } catch {
              // The candidate is incomplete; the last complete shell is safer.
              return cached ?? response;
            }
          }
          return cached ?? response;
        } catch {
          return (
            cached ??
            new Response("This Space cannot be reached.", {
              status: 503,
              headers: { "content-type": "text/plain; charset=utf-8" },
            })
          );
        }
      })(),
    );
    return;
  }

  // A content-named asset never changes; a stable name revalidates.
  event.respondWith(url.pathname.startsWith("/assets/") ? cacheFirst(request) : networkFirst(request));
});
