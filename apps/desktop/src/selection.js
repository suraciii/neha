// Pure validation for the Space address the owner enters in the desktop
// launcher. Keeping it Electron-free makes the remote/local boundary testable.
function isLoopbackHost(hostname) {
  return (
    hostname === "127.0.0.1" ||
    hostname === "localhost" ||
    hostname === "[::1]" ||
    hostname.endsWith(".localhost")
  );
}

function normalizeRemoteUrl(value) {
  let url;
  try {
    url = new URL(String(value).trim());
  } catch {
    throw new Error("enter a Space address such as https://space.example.com");
  }
  if (url.username || url.password) {
    throw new Error("the Space address must not carry credentials");
  }
  if (url.protocol !== "https:" && !(url.protocol === "http:" && isLoopbackHost(url.hostname))) {
    throw new Error("a remote Space must use https; only a loopback Space may use http");
  }
  return url.origin;
}

/**
 * The URL a trusted main-process request uses to reach a Space.
 *
 * Chromium resolves `*.localhost` for the window, but Node's Windows resolver
 * does not. A `.localhost` Space is on this machine, so its probe goes to the
 * same port on 127.0.0.1; the browser still loads the named origin.
 */
function probeUrl(origin) {
  const url = new URL(origin);
  if (url.hostname.endsWith(".localhost")) {
    url.hostname = "127.0.0.1";
  }
  return url.href;
}

module.exports = { isLoopbackHost, normalizeRemoteUrl, probeUrl };
