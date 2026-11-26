import { Show, createSignal } from "solid-js";

import { api, ApiError } from "../lib/api";
import type { Account } from "../types";
import Brand from "./Brand";

type Mode = "join" | "login" | "missing-invite";

interface AuthViewProps {
  mode: Mode;
  secret: string;
  onAuthed: (account: Account) => void;
}

export default function AuthView(props: AuthViewProps) {
  const [displayName, setDisplayName] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [pending, setPending] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const isJoin = () => props.mode === "join";

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (pending()) return;
    setError(null);
    setPending(true);
    try {
      const account = isJoin()
        ? await api.join(props.secret, displayName().trim(), password())
        : await api.login(displayName().trim(), password());
      history.replaceState(null, "", "/");
      props.onAuthed(account);
    } catch (reason) {
      setError(messageFor(reason));
    } finally {
      setPending(false);
    }
  }

  return (
    <div class="auth">
      <section class="auth__intro">
        <Brand />
        <div class="auth__copy">
          <p class="eyebrow">Neha · Visual design, implemented</p>
          <h1>Put work in the hands of a team you can see.</h1>
          <p class="auth__lede">
            A Bot is a teammate. It has its own Computer, memory, and identity. Work lands in the Chat, and
            authority always stays with you.
          </p>
        </div>
        <ul class="auth__points">
          <li>
            <span class="auth__point-dot" />
            One Space, on this machine
          </li>
          <li>
            <span class="auth__point-dot" />
            Your Provider, your Models
          </li>
          <li>
            <span class="auth__point-dot" />
            A managed Computer for each Bot
          </li>
        </ul>
      </section>

      <section class="auth__card-wrap">
        <div class="auth__card">
          <Show when={props.mode === "missing-invite"}>
            <p class="eyebrow">Invitation required</p>
            <h2>Open the whole invitation link</h2>
            <p class="muted">
              The link printed by <code>neha serve</code> carries a secret after the <code>#</code>. Paste the
              complete link into this browser.
            </p>
          </Show>

          <Show when={props.mode !== "missing-invite"}>
            <p class="eyebrow">{isJoin() ? "Admin invitation" : "Welcome back"}</p>
            <h2>{isJoin() ? "Join this Space" : "Sign in to Neha"}</h2>
            <p class="auth__hint">
              {isJoin()
                ? "Become the Admin of this Space. The invitation works once, while the Space runs."
                : "Use the account that already belongs to this Space."}
            </p>

            <form class="form" onSubmit={submit}>
              <label class="field">
                <span class="field__label">Display name</span>
                <input
                  class="input"
                  name="display_name"
                  autocomplete="username"
                  maxlength={64}
                  required
                  value={displayName()}
                  onInput={(event) => setDisplayName(event.currentTarget.value)}
                />
              </label>
              <label class="field">
                <span class="field__label">Password</span>
                <input
                  class="input"
                  name="password"
                  type="password"
                  autocomplete={isJoin() ? "new-password" : "current-password"}
                  required
                  value={password()}
                  onInput={(event) => setPassword(event.currentTarget.value)}
                />
                <span class="field__hint">At least eight bytes. A short phrase is enough.</span>
              </label>

              <Show when={error()}>
                <p class="form__error" role="alert">
                  {error()}
                </p>
              </Show>

              <button class="btn btn--primary btn--wide" type="submit" disabled={pending()}>
                {pending() ? "Working…" : isJoin() ? "Join Space" : "Sign in"}
              </button>
            </form>
          </Show>

          <p class="auth__fine">
            The Space listens on this machine only. The browser keeps the session in an HttpOnly cookie.
          </p>
        </div>
      </section>
    </div>
  );
}

function messageFor(reason: unknown): string {
  if (reason instanceof ApiError) {
    if (reason.code === "not_found") return "This invitation is not usable. Start a new Space or ask the Admin.";
    if (reason.code === "unauthorized") return "The display name or password is not correct.";
    if (reason.code === "rate_limited") return "Too many sign-in attempts. Wait a few minutes.";
    if (reason.code === "conflict") return "This Space already has an Admin. Sign in instead.";
    return reason.message;
  }
  if (reason instanceof TypeError) return "The browser cannot reach this Space. Is `neha serve` still running?";
  return "The Space cannot complete this request.";
}
