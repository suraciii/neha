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

## Live updates

The client loads its view, then follows the record over the same WebSocket from a cursor. The cursor lives in local storage, so a reload resumes instead of replaying the whole record. Chat, File, Run, and preview changes arrive as record events; the client does not poll.

## Language

The client supports English (en-US) and Simplified Chinese (zh-CN). The switch lives on the sign-in card, the setup header, the Chat list footer, and the Chat header. The first visit follows the browser language; the chosen locale is kept in local storage and never changes the record.

## Test

```bash
pnpm --dir ui test
pnpm --dir ui run check
```

The repository gate runs both through `just ci`.
