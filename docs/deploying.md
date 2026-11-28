# Serve a Space on a domain

A Space runs as a Local Space on the computer where the desktop app starts it, or as a Hosted Space on a server. This guide deploys a Hosted Space.

A Space listens on loopback on the machine that runs it. To reach it from a phone or another computer, put a TLS proxy in front of it and name the public origin with `--domain` or `NEHA_DOMAIN` (a host with an optional port), or with `NEHA_PUBLIC_URL` (a full `https://` origin). The same proxy exposes a Space started with `neha serve`; the desktop app's Local Space does not offer this setup yet.

The proxy is the only public port. The Space never opens one itself.

This guide runs the Space directly on the host, with its Hosted Computers in the Space process. To run the Space, its Caddy proxy, and one worker container per Hosted Computer under Docker Compose instead, see [Run a Space in containers](container.md).

## Requirements

- A machine that runs the Space.
- A domain whose DNS points at that machine.
- A TLS proxy. This guide uses Caddy; another proxy works when it preserves `Host` and passes WebSocket upgrades.
- The built browser client. [Quick start](quickstart.md) builds it with `pnpm --dir ui build`.

## Start the Space

Run the Space with its home, its client, and the domain users reach:

```bash
NEHA_HOME=/srv/neha/home target/debug/neha serve \
  --domain neha.example.com \
  --ui ui/dist
```

The command prints the loopback address, the public origin, and the invitation link on the domain:

```text
Neha is listening on http://127.0.0.1:7337
Public: https://neha.example.com
Invite: https://neha.example.com/invite#<secret>
```

Keep the process running. A full origin does the same through the environment:

```bash
NEHA_PUBLIC_URL=https://neha.example.com neha serve --ui ui/dist
```

A service manager keeps the process running:

```ini
[Unit]
Description=Neha Space
After=network-online.target

[Service]
User=neha
Environment=NEHA_HOME=/srv/neha/home
ExecStart=/usr/local/bin/neha serve --domain neha.example.com --ui /srv/neha/ui/dist
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

## Terminate TLS

Point Caddy at the loopback port:

```caddyfile
neha.example.com {
    reverse_proxy 127.0.0.1:7337
}
```

Caddy obtains a certificate, preserves `Host`, forwards WebSocket upgrades, and sets the client address in `X-Forwarded-For`.

For nginx, set these headers on the same origin:

```nginx
proxy_set_header Host $http_host;
proxy_set_header X-Forwarded-For $remote_addr;
proxy_http_version 1.1;
proxy_set_header Upgrade $http_upgrade;
proxy_set_header Connection "upgrade";
```

Overwrite `X-Forwarded-For` with the client address at the proxy. The Space reads the final value only from a proxy it trusts: a loopback peer, or an address named in `NEHA_TRUSTED_PROXIES`. Name a proxy on another machine or in another container there as a comma-separated list of addresses, CIDR ranges, or hostnames; a hostname is resolved once at start, so give the proxy a stable address or restart the Space after replacing it. The forwarded chain is single-hop: the proxy must overwrite `X-Forwarded-For`, or write its final value from the request's TCP peer. The Space cannot tell who wrote a well-formed value, so a proxy that only passes an earlier header through defeats this check. The public origin declares that TLS proxy; keep the Space's own listener on loopback or a private network so nothing reaches it directly.

Check the public origin:

```bash
curl --fail https://neha.example.com/healthz
```

The answer is `ok`.

## Join and install

Open the invitation link on the phone. The first User joins as the Space Owner.

Then install the client from the same URL:

| Device | Install |
| --- | --- |
| iPhone, iPad | Open the Space in Safari, tap Share, then Add to Home Screen. |
| Android | Open the Space in Chrome, open the menu, then tap Install app or Add to Home screen. |
| Desktop | Use the install icon in the browser's address bar. |

An installed client opens in its own window. It keeps its shell when the Space cannot be reached and offers to retry; it holds no Messages offline. A new build is used the next time the client opens.

The CLI signs in to the same origin, and its commands use `wss://`:

```bash
neha login --name Ada --server https://neha.example.com
```

Notifications are available in an installed client on the public HTTPS origin. On iPhone and iPad, install the client first: Web Push works only in a Home Screen app. The Space needs outbound HTTPS to reach the browser vendor's push service; it connects directly and does not use HTTP proxy environment variables. [Notifications](notifications.md) covers enabling, muting, and what the push service can see.

## Other ways to terminate TLS

- **Tailscale.** Run `tailscale serve --bg 7337`, and pass `--domain <machine>.<tailnet>.ts.net`.
- **Cloudflare Tunnel.** Run `cloudflared tunnel --url http://127.0.0.1:7337`, and pass `--domain <public host>`.
- **Another proxy.** Forward the public `Host` and the WebSocket upgrade to `127.0.0.1:7337`. Keep that port off the public network.

## Operate

- **Address.** One Space serves one public origin. `--domain` takes the host only, such as `neha.example.com` or `neha.example.com:8443`; `NEHA_PUBLIC_URL` takes the full `https://` origin. A scheme or path passed to `--domain` is refused.
- **Health.** `GET /healthz` answers `ok` on the public origin.
- **Upgrade.** Replace the binary and the built client, then restart the Space. [Upgrade a Space](upgrading.md) covers the backup, the schema migration, and the version check. Open clients load the new build on their next open.
- **Backup.** Back up the Space's home directory, and keep the Vault's master key with it; [Storage](storage.md) covers the File store. The same home holds sealed notification keys and device subscriptions, so a backup restores notifications too.
- **Notifications.** The Space reaches each browser's push service directly over outbound HTTPS; it does not use HTTP proxy environment variables. A phone must install the client from the public origin; on iOS and iPadOS, Web Push works only from the Home Screen. See [Notifications](notifications.md).
- **Logs.** The Space writes JSON diagnostics to stderr; read them from the service manager, for example `journalctl -u neha -o cat | jq .`. [Diagnostics](diagnostics.md) covers `NEHA_LOG`, the fields, and the report a failed start leaves.

## Limits

- The listener stays on loopback. Do not expose port 7337.
- The Space has no built-in TLS listener; a proxy always terminates TLS.
- A loopback proxy is trusted without configuration. A proxy on another machine or in another container must be named in `NEHA_TRUSTED_PROXIES`; otherwise every client is counted as that proxy's address for sign-in limits.
- The proxy must preserve the public `Host`, including a non-default port, and pass WebSocket upgrades under the same authority.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| `403` with `the Host header must name this Space` | The proxy changed `Host`; preserve the original header. |
| `403` with `a browser Origin must name this Space` | The public origin names a different host, or the proxy forwards plain HTTP; use the same host over HTTPS. |
| Sign-in reports too many attempts | The proxy does not set `X-Forwarded-For`, is not named in `NEHA_TRUSTED_PROXIES`, or forwards a chain instead of overwriting it with the client address. |
| The WebSocket disconnects | The proxy does not pass the upgrade headers. |
| The browser offers no install | The page is not HTTPS, or its origin does not match the public origin. |
