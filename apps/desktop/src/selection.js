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
  // Keep the path and fragment: a remote invitation is an /invite#secret URL,
  // not just the origin. The desktop loads this exact page so the Space's own
  // join screen receives the secret.
  return url.href;
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

/**
 * Returns the settled remote page to remember after a navigation, or null when
 * the page is still an invitation or belongs to another origin.
 *
 * The current origin, not the saved invitation origin, is authoritative: a
 * loopback address may redirect to the Space's canonical `*.localhost` host
 * before the User joins, and the one-use invite must not survive that join.
 */
function settledRemoteUrl(selection, currentOrigin, url) {
  if (selection?.mode !== "remote" || !currentOrigin) return null;
  try {
    const current = new URL(currentOrigin);
    const next = new URL(url);
    if (next.origin !== current.origin) return null;
    if (next.pathname.startsWith("/invite")) return null;
    if (selection.spaceUrl === next.href) return null;
    return next.href;
  } catch {
    return null;
  }
}

module.exports = { isLoopbackHost, normalizeRemoteUrl, probeUrl, settledRemoteUrl };
