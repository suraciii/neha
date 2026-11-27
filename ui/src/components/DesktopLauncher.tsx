import { Show, createSignal } from "solid-js";

import { t } from "../i18n";
import Brand from "./Brand";
import Icon from "./Icon";
import LocaleSwitch from "./LocaleSwitch";

interface DesktopBridge {
  chooseLocal(useLocalComputer: boolean): Promise<{ ok: boolean; message?: string }>;
  connectRemote(url: string): Promise<{ ok: boolean; message?: string }>;
}

declare global {
  interface Window {
    nehaDesktop?: DesktopBridge;
  }
}

export default function DesktopLauncher() {
  const [remoteUrl, setRemoteUrl] = createSignal("");
  const [useLocalComputer, setUseLocalComputer] = createSignal(false);
  const [busy, setBusy] = createSignal<"local" | "remote" | null>(null);
  const [error, setError] = createSignal<string | null>(null);

  const bridge = () => window.nehaDesktop;
  const localComputerText = () =>
    /Windows/i.test(navigator.userAgent)
      ? t("desktop.localComputer.text.windows")
      : t("desktop.localComputer.text.other");

  async function chooseLocal() {
    if (busy() || !bridge()) return;
    setError(null);
    setBusy("local");
    try {
      const result = await bridge()!.chooseLocal(useLocalComputer());
      if (!result.ok) setError(t("desktop.error.local"));
    } catch {
      setError(t("desktop.error.local"));
    } finally {
      setBusy(null);
    }
  }

  async function connectRemote(event: SubmitEvent) {
    event.preventDefault();
    const url = remoteUrl().trim();
    if (!url || busy() || !bridge()) return;
    setError(null);
    setBusy("remote");
    try {
      const result = await bridge()!.connectRemote(url);
      if (!result.ok) setError(t("desktop.error.remote"));
    } catch {
      setError(t("desktop.error.remote"));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div class="auth">
      <section class="auth__intro">
        <Brand />
        <div class="auth__copy">
          <h1>{t("desktop.title")}</h1>
          <p class="auth__lede">{t("desktop.lede")}</p>
        </div>
        <ul class="auth__points">
          <li>
            <span class="auth__point-dot" />
            {t("desktop.point.local")}
          </li>
          <li>
            <span class="auth__point-dot" />
            {t("desktop.point.remote")}
          </li>
          <li>
            <span class="auth__point-dot" />
            {t("desktop.point.login")}
          </li>
        </ul>
      </section>

      <section class="auth__card-wrap">
        <div class="auth__card desktop__card">
          <div class="auth__locale">
            <LocaleSwitch />
          </div>
          <h2>{t("desktop.choose")}</h2>
          <p class="auth__hint">{t("desktop.hint")}</p>

          <Show
            when={bridge()}
            fallback={<p class="form__error">{t("desktop.unavailable")}</p>}
          >
            <button
              class="desktop__choice"
              type="button"
              disabled={busy() !== null}
              onClick={chooseLocal}
            >
              <span class="desktop__choice-icon">
                <Icon name="monitor" size="md" />
              </span>
              <span class="desktop__choice-copy">
                <span class="desktop__choice-title">{t("desktop.local.title")}</span>
                <span class="desktop__choice-text">{t("desktop.local.text")}</span>
              </span>
              <span class="desktop__choice-action">
                {busy() === "local" ? t("desktop.starting") : t("desktop.local.action")}
              </span>
            </button>

            <label class="desktop__consent">
              <input
                type="checkbox"
                checked={useLocalComputer()}
                onChange={(event) => setUseLocalComputer(event.currentTarget.checked)}
              />
              <span>
                <span class="desktop__consent-title">
                  {t("desktop.localComputer.title")}
                </span>
                <span class="desktop__consent-text">
                  {localComputerText()}
                </span>
              </span>
            </label>

            <div class="desktop__divider">
              <span>{t("desktop.or")}</span>
            </div>

            <form class="form" onSubmit={connectRemote}>
              <label class="field">
                <span class="field__label">{t("desktop.remote.label")}</span>
                <span class="desktop__remote">
                  <Icon name="link" size="sm" />
                  <input
                    class="input"
                    name="space_url"
                    inputmode="url"
                    autocomplete="url"
                    placeholder={t("desktop.remote.placeholder")}
                    value={remoteUrl()}
                    onInput={(event) => setRemoteUrl(event.currentTarget.value)}
                  />
                </span>
                <span class="field__hint">{t("desktop.remote.hint")}</span>
              </label>
              <button
                class="btn btn--primary btn--wide"
                type="submit"
                disabled={busy() !== null || remoteUrl().trim() === ""}
              >
                {busy() === "remote" ? t("desktop.connecting") : t("desktop.connect")}
              </button>
            </form>
          </Show>

          <Show when={error()}>
            <p class="form__error" role="alert">
              {error()}
            </p>
          </Show>

          <p class="auth__fine">{t("desktop.fine")}</p>
        </div>
      </section>
    </div>
  );
}
