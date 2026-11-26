# Browser client

The Solid client the Space serves at its own origin.

## Develop

Start a Space in one terminal:

```bash
export NEHA_HOME="$HOME/.local/share/neha/dev"
cargo run --bin neha -- serve
```

Start the client dev server in another:

```bash
pnpm --dir ui install --frozen-lockfile
NEHA_SPACE='http://neha-<hash>.localhost:7337' pnpm --dir ui dev
```

Use the browser URL `neha serve` prints. The Vite server listens on that hostname at port 5173 and proxies `/api` and its WebSocket to the Space. Open `http://neha-<hash>.localhost:5173` and use the browser invitation.

## Build

```bash
pnpm --dir ui build
```

`neha serve` serves `ui/dist` by default. Pass `--ui <directory>` or set `NEHA_UI_DIR` to serve another build.

## Test

```bash
pnpm --dir ui test
pnpm --dir ui run check
```

The repository gate runs both through `just ci`.
