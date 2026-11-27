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

module.exports = { isLoopbackHost, normalizeRemoteUrl };
