import { createSignal, Show } from "solid-js";

import { t } from "../i18n";
import { ApiError, api } from "../lib/api";
import type { Account } from "../types";
import Brand from "./Brand";
import LocaleSwitch from "./LocaleSwitch";

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
  const [error, setError] = createSignal<unknown>(null);

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
      setError(reason);
    } finally {
      setPending(false);
    }
  }

  return (
    <div class="auth">
      <section class="auth__intro">
        <Brand />
        <div class="auth__copy">
          <h1>{t("auth.headline")}</h1>
          <p class="auth__lede">{t("auth.lede")}</p>
        </div>
        <ul class="auth__points">
          <li>
            <span class="auth__point-dot" />
            {t("auth.point.space")}
          </li>
          <li>
            <span class="auth__point-dot" />
            {t("auth.point.provider")}
          </li>
          <li>
            <span class="auth__point-dot" />
            {t("auth.point.computer")}
          </li>
        </ul>
      </section>

      <section class="auth__card-wrap">
        <div class="auth__card">
          <div class="auth__locale">
            <LocaleSwitch />
          </div>
          <Show when={props.mode === "missing-invite"}>
            <h2>{t("auth.missing.title")}</h2>
            <p class="muted">
              {t("auth.missing.lead")} <code>neha serve</code> {t("auth.missing.mid")}{" "}
              <code>#</code>
              {t("auth.missing.tail")}
            </p>
          </Show>

          <Show when={props.mode !== "missing-invite"}>
            <h2>{isJoin() ? t("auth.join.title") : t("auth.login.title")}</h2>
            <p class="auth__hint">{isJoin() ? t("auth.join.hint") : t("auth.login.hint")}</p>

            <form class="form" onSubmit={submit}>
              <label class="field">
                <span class="field__label">{t("auth.name")}</span>
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
                <span class="field__label">{t("auth.password")}</span>
                <input
                  class="input"
                  name="password"
                  type="password"
                  autocomplete={isJoin() ? "new-password" : "current-password"}
                  required
                  value={password()}
                  onInput={(event) => setPassword(event.currentTarget.value)}
                />
                <span class="field__hint">{t("auth.password.hint")}</span>
              </label>

              <Show when={error()}>
                {(reason) => (
                  <p class="form__error" role="alert">
                    {messageFor(reason())}
                  </p>
                )}
              </Show>

              <button class="btn btn--primary btn--wide" type="submit" disabled={pending()}>
                {pending()
                  ? t("auth.working")
                  : isJoin()
                    ? t("auth.join.action")
                    : t("auth.login.action")}
              </button>
            </form>
          </Show>

          <p class="auth__fine">{t("auth.fine")}</p>
        </div>
      </section>
    </div>
  );
}

function messageFor(reason: unknown): string {
  if (reason instanceof ApiError) {
    if (reason.code === "not_found") return t("error.invitation");
    if (reason.code === "unauthorized") return t("error.credentials");
    if (reason.code === "rate_limited") return t("error.rate");
    if (reason.code === "conflict") return t("error.conflict");
    if (reason.code === "invalid_argument" && /password/i.test(reason.message)) {
      return t("error.password");
    }
    if (reason.code === "invalid_argument") return t("error.invalid");
    return t("error.generic");
  }
  if (reason instanceof TypeError) return t("error.unreachable");
  return t("error.generic");
}
