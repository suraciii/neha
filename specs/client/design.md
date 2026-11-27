# Client

[product.md](product.md) states what the browser client shows. This page states the boundaries the Space and the client keep. [design-system.md](design-system.md) states the UI/UX design that implements that behavior.

## Authority

Owned by client. The Space serves the client, and the command surface carries every action a client takes except the transfer of a File's bytes.

## Laws

1. A browser client reaches a Space through the Space's origin only.
2. The Space serves client files only from the configured client directory, and a File's bytes only from its File store.
3. A same-origin request carries the session cookie; a foreign origin requires a Bearer token.
4. The session cookie is `HttpOnly` and `SameSite=Strict`, and its name is unique to the Space's home.
5. The client holds no second copy of the record; the Space's record is the source of truth.

## Serving

The Space serves the built client from a directory. The directory defaults to `ui/dist` and can be set with `--ui` or `NEHA_UI_DIR`.

A Space without a public origin listens on `127.0.0.1` and serves the browser client from a hostname stable for its home, under `.localhost`. The session cookie is host-only on that hostname. The Space does not accept a browser session on `127.0.0.1` or `localhost`.

A Space that a browser reaches over a network sits behind a trusted TLS proxy that forwards to loopback. The public origin is named by `NEHA_PUBLIC_URL`, a full `https://` origin, or by `--domain`/`NEHA_DOMAIN`, a host with an optional port.

That origin is the browser hostname. The Space accepts its `Host` and `https` `Origin`, issues a `Secure` session cookie, and redirects a loopback browser request to it. The proxy MUST preserve the public `Host` and `Origin` and terminate TLS; the Space still listens on loopback only.

The Space sends HSTS on the public origin, a Content Security Policy limited to its own origin, and cache rules that keep the shell fresh and content-named assets immutable.

Without a public origin, the browser client uses its local hostname over `http`. An Origin is accepted only when it names the request's own host with the scheme the Space serves.

A path that does not name a file returns `index.html`, the browser entry point. The build also carries `desktop.html`, the desktop chooser entry point. A missing or escaping entry point is a start error, not a partial page.

The Space does not serve its home directory or any file outside the client directory, except a File's bytes from the File store. A client directory that contains the Space home, or sits inside it, is a start error.

## Desktop host

The desktop shell serves the same built client. A private `neha://ui` protocol
serves the chooser from the client directory; no remote content is loaded there.

The launcher is the only page with a preload bridge to the shell. A Space page
loads over its own origin with the same sandbox, no Node integration, and no
desktop bridge.

The local choice starts the bundled Space. The remote choice does not start a
sidecar and loads only the chosen `https://` origin, or a loopback `http://`
origin for development. The window MUST NOT navigate outside the chosen
origin, except the narrow loopback-to-canonical-`*.localhost` redirect a local
Space uses for its browser host.

The shell MAY read the local Space's `HttpOnly` cookie from its own Electron
session to ask the Space for a Local Computer ticket after sign-in. It sends the
ticket to the bundled CLI over loopback; it MUST NOT expose the token to the
launcher or to a remote Space page.

## Sessions

Joining and signing in set the session cookie. The cookie carries the same device token the CLI keeps in its credential file.

The `Authorization` header remains for the CLI and other native clients. The browser client does not use it.

## Limits

The client has one command connection. It loads its view, then follows the record from a cursor. A reconnect resumes from the last cursor; a cursor the Space can no longer serve gives the client a fresh snapshot.

The model reply is not streamed. The client shows the working state for the duration of the Run, driven by record events as well as the command result.
