// Registers the installed shell where a browser can own one.
//
// An installed Neha is served over HTTPS. A local Space uses
// `neha-<hash>.localhost` and the desktop shell loads it directly, so neither
// registers a worker. `localhost` and `127.0.0.1` stay testable without a
// certificate.
export function shouldRegisterServiceWorker(location: {
  protocol: string;
  hostname: string;
}): boolean {
  if (location.protocol === "https:") return true;
  return location.hostname === "localhost" || location.hostname === "127.0.0.1";
}

export function registerServiceWorker(
  target: Window = window,
  navigator: Navigator = window.navigator,
): void {
  if (!shouldRegisterServiceWorker(target.location) || !("serviceWorker" in navigator)) {
    return;
  }
  target.addEventListener(
    "load",
    () => {
      void navigator.serviceWorker.register("/sw.js").catch(() => undefined);
    },
    { once: true },
  );
}
