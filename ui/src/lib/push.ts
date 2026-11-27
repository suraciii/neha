import type { PushSubscriptionBody } from "../types";
import { shouldRegisterServiceWorker } from "./pwa";

// What this browser can do with Web Push. A phone that has not installed the
// web app cannot receive it on iOS, even when the browser supports the API.
export type PushCapability = "unsupported" | "ios-install" | "ready";

export interface PushEnvironment {
  userAgent: string;
  platform: string;
  maxTouchPoints: number;
  standalone: boolean;
  registration: boolean;
  notification: boolean;
  pushManager: boolean;
  serviceWorker: boolean;
}

/** Whether this is an iPhone, iPad, or an iPad reporting as a Mac. */
export function isIos(environment: Pick<PushEnvironment, "userAgent" | "platform" | "maxTouchPoints">): boolean {
  return (
    /iPad|iPhone|iPod/.test(environment.userAgent) ||
    (environment.platform === "MacIntel" && environment.maxTouchPoints > 1)
  );
}

/** Whether the client runs as an installed Home Screen app. */
export function isStandalone(): boolean {
  if (typeof window === "undefined") return false;
  const navigatorWithStandalone = navigator as Navigator & { standalone?: boolean };
  return (
    navigatorWithStandalone.standalone === true ||
    (typeof window.matchMedia === "function" && window.matchMedia("(display-mode: standalone)").matches)
  );
}

/** Classifies what the current browser can offer. */
export function pushCapability(environment: PushEnvironment): PushCapability {
  // iOS exposes no Web Push APIs outside an installed Home Screen app. The
  // install hint has to come before the API checks, or the control vanishes.
  if (isIos(environment) && !environment.standalone) return "ios-install";
  if (
    !environment.registration ||
    !environment.notification ||
    !environment.pushManager ||
    !environment.serviceWorker
  ) {
    return "unsupported";
  }
  return "ready";
}

/** Reads the browser facts the capability needs. */
export function currentPushEnvironment(): PushEnvironment {
  const navigatorValue = navigator as Navigator & { standalone?: boolean };
  return {
    userAgent: navigatorValue.userAgent,
    platform: navigatorValue.platform,
    maxTouchPoints: navigatorValue.maxTouchPoints ?? 0,
    standalone: isStandalone(),
    registration: shouldRegisterServiceWorker(window.location),
    notification: "Notification" in window,
    pushManager: "PushManager" in window,
    serviceWorker: "serviceWorker" in navigator,
  };
}

/** Converts the VAPID public key to the bytes `subscribe` expects. */
export function urlBase64ToUint8Array(value: string): Uint8Array<ArrayBuffer> {
  const padding = "=".repeat((4 - (value.length % 4)) % 4);
  const base64 = (value + padding).replace(/-/g, "+").replace(/_/g, "/");
  const raw = atob(base64);
  const bytes = new Uint8Array(new ArrayBuffer(raw.length));
  for (let index = 0; index < raw.length; index += 1) {
    bytes[index] = raw.charCodeAt(index);
  }
  return bytes;
}

/** The JSON the Space stores for one browser. */
export function subscriptionBody(subscription: PushSubscription): PushSubscriptionBody | null {
  const json = subscription.toJSON();
  if (!json.endpoint || !json.keys?.p256dh || !json.keys.auth) return null;
  return {
    endpoint: json.endpoint,
    keys: { p256dh: json.keys.p256dh, auth: json.keys.auth },
  };
}

/** Whether a stored mute is still in effect. */
export function isMuted(mutedUntil: number | null): boolean {
  return mutedUntil !== null && mutedUntil * 1000 > Date.now();
}
