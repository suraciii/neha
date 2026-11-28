import { createSignal, onSettled, Show } from "solid-js";

import { t } from "../i18n";
import { api } from "../lib/api";
import {
  currentPushEnvironment,
  isMuted,
  pushCapability,
  serviceWorkerReady,
  subscriptionBody,
  urlBase64ToUint8Array,
} from "../lib/push";
import Icon from "./Icon";

// What the control has to say right now. "unsupported" is not rendered.
type State = "loading" | "unsupported" | "ios" | "unavailable" | "off" | "on" | "muted" | "denied";

// The quiet footer control that turns a browser into a notification device.
// The Space holds the subscriptions and the one VAPID key; this only asks the
// browser and stores what it answered.
export default function NotificationSetting() {
  const [open, setOpen] = createSignal(false);
  const [state, setState] = createSignal<State>("loading");
  const [mutedUntil, setMutedUntil] = createSignal<number | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  let root: HTMLDivElement | undefined;
  let muteTimer: ReturnType<typeof setTimeout> | undefined;

  const support = () => pushCapability(currentPushEnvironment());
  const muted = () => isMuted(mutedUntil());

  // Keeps the panel's mute state and its expiry in one place.
  function rememberMute(until: number | null) {
    setMutedUntil(until);
    if (muteTimer !== undefined) {
      clearTimeout(muteTimer);
      muteTimer = undefined;
    }
    if (until === null || until * 1000 <= Date.now()) return;
    muteTimer = setTimeout(
      () => {
        muteTimer = undefined;
        setMutedUntil(null);
        setState((current) => (current === "muted" ? "on" : current));
      },
      until * 1000 - Date.now() + 1000,
    );
  }
  const muteTime = () => {
    const until = mutedUntil();
    if (until === null) return "";
    return new Date(until * 1000).toLocaleTimeString(undefined, {
      hour: "2-digit",
      minute: "2-digit",
    });
  };

  async function initialise() {
    if (support() === "unsupported") {
      setState("unsupported");
      return;
    }
    if (support() === "ios-install") {
      setState("ios");
      return;
    }
    try {
      const status = await api.pushStatus();
      rememberMute(status.muted_until);
      if (Notification.permission === "denied") {
        setState("denied");
        return;
      }
      if (Notification.permission === "granted") {
        const registration = await serviceWorkerReady();
        const subscription = await registration.pushManager.getSubscription();
        if (subscription) {
          const body = subscriptionBody(subscription);
          // A server restart keeps its state file, but re-posting makes the
          // device and the Space agree after either one lost the other.
          if (body) await api.subscribePush(body);
          setState(isMuted(status.muted_until) ? "muted" : "on");
          return;
        }
      }
      setState("off");
    } catch {
      setState("unavailable");
    }
  }

  async function enable() {
    setBusy(true);
    setError(null);
    try {
      const permission = await Notification.requestPermission();
      if (permission !== "granted") {
        setState(permission === "denied" ? "denied" : "off");
        return;
      }
      const status = await api.pushStatus();
      rememberMute(status.muted_until);
      const registration = await serviceWorkerReady();
      let subscription = await registration.pushManager.getSubscription();
      if (!subscription) {
        subscription = await registration.pushManager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: urlBase64ToUint8Array(status.key),
        });
      }
      const body = subscriptionBody(subscription);
      if (!body) throw new Error("the browser returned no subscription keys");
      await api.subscribePush(body);
      setState(isMuted(status.muted_until) ? "muted" : "on");
    } catch {
      setError(t("notify.error"));
    } finally {
      setBusy(false);
    }
  }

  async function mute() {
    setBusy(true);
    setError(null);
    try {
      const result = await api.mutePush(60);
      rememberMute(result.muted_until);
      setState("muted");
    } catch {
      setError(t("notify.error"));
    } finally {
      setBusy(false);
    }
  }

  async function unmute() {
    setBusy(true);
    setError(null);
    try {
      const result = await api.unmutePush();
      rememberMute(result.muted_until);
      setState("on");
    } catch {
      setError(t("notify.error"));
    } finally {
      setBusy(false);
    }
  }

  async function turnOff() {
    setBusy(true);
    setError(null);
    try {
      const registration = await serviceWorkerReady();
      const subscription = await registration.pushManager.getSubscription();
      if (subscription) {
        // Delete the server record first. If that fails the device stays
        // subscribed, so the user can retry instead of leaving a stale entry.
        await api.unsubscribePush(subscription.endpoint);
        await subscription.unsubscribe();
      }
      rememberMute(null);
      setState("off");
    } catch {
      setError(t("notify.error"));
    } finally {
      setBusy(false);
    }
  }

  function retry() {
    setError(null);
    setState("loading");
    void initialise();
  }

  async function toggleOpen() {
    const next = !open();
    setOpen(next);
    if (!next || support() !== "ready" || state() === "loading") return;
    try {
      const status = await api.pushStatus();
      rememberMute(status.muted_until);
      if (state() === "muted" && !isMuted(status.muted_until)) setState("on");
      else if (state() === "on" && isMuted(status.muted_until)) setState("muted");
    } catch {
      // The panel keeps what it knows; an action reports the error.
    }
  }

  onSettled(() => {
    void initialise();
    const dismiss = (event: MouseEvent) => {
      if (open() && root && event.target instanceof Node && !root.contains(event.target)) {
        setOpen(false);
      }
    };
    const onKeydown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", dismiss);
    document.addEventListener("keydown", onKeydown);
    return () => {
      document.removeEventListener("mousedown", dismiss);
      document.removeEventListener("keydown", onKeydown);
      if (muteTimer !== undefined) clearTimeout(muteTimer);
    };
  });

  return (
    <Show when={state() !== "unsupported"}>
      <div class="notify" ref={root}>
        <button
          class="notify__button"
          type="button"
          aria-label={t("notify.button")}
          aria-expanded={open() ? "true" : "false"}
          aria-controls="notifications-panel"
          title={t("notify.button")}
          onClick={() => void toggleOpen()}
        >
          <Icon name="bell" size="sm" />
          <span
            class={`notify__dot${muted() ? " is-muted" : ""}${state() === "on" ? " is-on" : ""}`}
          />
        </button>

        <Show when={open()}>
          <section
            class="notify__panel"
            id="notifications-panel"
            role="dialog"
            aria-label={t("notify.title")}
          >
            <header class="notify__head">
              <h2 class="notify__title">{t("notify.title")}</h2>
              <button
                class="notify__close"
                type="button"
                aria-label={t("notify.close")}
                onClick={() => setOpen(false)}
              >
                <Icon name="x" size="sm" />
              </button>
            </header>

            <Show when={state() === "loading"}>
              <p class="notify__text">{t("notify.loading")}</p>
            </Show>
            <Show when={state() === "ios"}>
              <p class="notify__text">{t("notify.ios")}</p>
            </Show>
            <Show when={state() === "denied"}>
              <p class="notify__text">{t("notify.denied")}</p>
            </Show>
            <Show when={state() === "unavailable"}>
              <p class="notify__text">{t("notify.unavailable")}</p>
              <button
                class="btn btn--quiet btn--wide notify__quiet"
                type="button"
                disabled={busy()}
                onClick={() => retry()}
              >
                {t("notify.retry")}
              </button>
            </Show>
            <Show when={state() === "off"}>
              <p class="notify__text">{t("notify.off.text")}</p>
              <button
                class="btn btn--primary btn--wide notify__action"
                type="button"
                disabled={busy()}
                onClick={() => void enable()}
              >
                {t("notify.enable")}
              </button>
            </Show>
            <Show when={state() === "on" || state() === "muted"}>
              <p class="notify__text">
                {state() === "muted" ? t("notify.muted", { time: muteTime() }) : t("notify.on")}
              </p>
              <button
                class="btn btn--primary btn--wide notify__action"
                type="button"
                disabled={busy()}
                onClick={() => void (state() === "muted" ? unmute() : mute())}
              >
                {state() === "muted" ? t("notify.unmute") : t("notify.mute")}
              </button>
              <button
                class="btn btn--quiet btn--wide notify__quiet"
                type="button"
                disabled={busy()}
                onClick={() => void turnOff()}
              >
                {t("notify.turnOff")}
              </button>
            </Show>

            <Show when={error()}>{(message) => <p class="notify__error">{message()}</p>}</Show>
            <p class="notify__hint">{t("notify.hint")}</p>
          </section>
        </Show>
      </div>
    </Show>
  );
}
