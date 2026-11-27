import { fireEvent, render, waitFor } from "@solidjs/testing-library";
import { afterEach, describe, expect, it, vi } from "vitest";

import { api } from "../lib/api";
import { isMuted } from "../lib/push";
import NotificationSetting from "./NotificationSetting";

const environment = vi.hoisted(() => ({
  current: {
    userAgent: "Mozilla/5.0",
    platform: "Linux",
    maxTouchPoints: 0,
    standalone: false,
    registration: true,
    notification: true,
    pushManager: true,
    serviceWorker: true,
  },
}));

vi.mock("../lib/push", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/push")>();
  return { ...actual, currentPushEnvironment: () => environment.current };
});

function browser(options: {
  permission?: NotificationPermission;
  subscription?: PushSubscription | null;
  ready?: Promise<ServiceWorkerRegistration>;
}) {
  const subscription = options.subscription ?? null;
  const registration = {
    pushManager: {
      getSubscription: vi.fn(async () => subscription),
      subscribe: vi.fn(async () => subscription),
    },
  } as unknown as ServiceWorkerRegistration;
  const requestPermission = vi.fn(async () => options.permission ?? "granted");
  vi.stubGlobal("Notification", {
    permission: options.permission ?? "default",
    requestPermission,
  });
  Object.defineProperty(navigator, "serviceWorker", {
    configurable: true,
    value: { ready: options.ready ?? Promise.resolve(registration) },
  });
  return { registration, requestPermission };
}

function fakeSubscription(endpoint = "https://push.example.com/device") {
  return {
    endpoint,
    toJSON: () => ({ endpoint, keys: { p256dh: "p256dh", auth: "auth" } }),
    unsubscribe: vi.fn(async () => true),
  } as unknown as PushSubscription;
}

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  Object.defineProperty(navigator, "serviceWorker", { configurable: true, value: undefined });
  environment.current = {
    userAgent: "Mozilla/5.0",
    platform: "Linux",
    maxTouchPoints: 0,
    standalone: false,
    registration: true,
    notification: true,
    pushManager: true,
    serviceWorker: true,
  };
});

describe("notification settings", () => {
  it("shows the install hint on an iPhone that is not on the Home Screen", async () => {
    environment.current = {
      ...environment.current,
      userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X)",
      platform: "iPhone",
      maxTouchPoints: 5,
      // Safari outside the Home Screen has no push APIs at all; the hint is
      // still the useful answer.
      notification: false,
      pushManager: false,
      serviceWorker: false,
      registration: false,
    };
    const status = vi.spyOn(api, "pushStatus");
    const view = render(() => <NotificationSetting />);

    fireEvent.click(view.container.querySelector(".notify__button")!);

    await waitFor(() =>
      expect(view.container.textContent).toContain(
        "Add Neha to your Home Screen to get notifications.",
      ),
    );
    expect(status).not.toHaveBeenCalled();
  });

  it("enables notifications and stores the browser subscription", async () => {
    const subscription = fakeSubscription();
    const { requestPermission } = browser({ subscription });
    vi.spyOn(api, "pushStatus").mockResolvedValue({ key: "BOr", muted_until: null });
    const subscribe = vi
      .spyOn(api, "subscribePush")
      .mockResolvedValue({ subscribed: true });
    const view = render(() => <NotificationSetting />);

    await waitFor(() => expect(view.container.querySelector(".notify__button")).toBeTruthy());
    fireEvent.click(view.container.querySelector(".notify__button")!);
    await waitFor(() => expect(view.container.textContent).toContain("Get a notification"));
    fireEvent.click(view.container.querySelector(".notify__action")!);

    await waitFor(() => expect(subscribe).toHaveBeenCalled());
    expect(requestPermission).toHaveBeenCalled();
    expect(subscribe).toHaveBeenCalledWith({
      endpoint: subscription.endpoint,
      keys: { p256dh: "p256dh", auth: "auth" },
    });
    await waitFor(() =>
      expect(view.container.textContent).toContain("Notifications are on."),
    );
  });

  it("shows a stored mute and unmutes the Space", async () => {
    const until = Math.floor(Date.now() / 1000) + 3600;
    const subscription = fakeSubscription();
    browser({ permission: "granted", subscription });
    vi.spyOn(api, "pushStatus").mockResolvedValue({ key: "BOr", muted_until: until });
    vi.spyOn(api, "subscribePush").mockResolvedValue({ subscribed: true });
    const unmute = vi
      .spyOn(api, "unmutePush")
      .mockResolvedValue({ muted_until: null });
    const view = render(() => <NotificationSetting />);

    await waitFor(() => expect(view.container.querySelector(".notify__button")).toBeTruthy());
    fireEvent.click(view.container.querySelector(".notify__button")!);
    await waitFor(() => expect(view.container.textContent).toContain("Muted until"));
    expect(isMuted(until)).toBe(true);
    expect(view.container.textContent).toContain("Unmute");

    fireEvent.click(view.container.querySelector(".notify__action")!);

    await waitFor(() => expect(unmute).toHaveBeenCalled());
    await waitFor(() =>
      expect(view.container.textContent).toContain("Notifications are on."),
    );
  });

  it("turns off notifications on this device", async () => {
    const subscription = fakeSubscription();
    browser({ permission: "granted", subscription });
    vi.spyOn(api, "pushStatus").mockResolvedValue({ key: "BOr", muted_until: null });
    vi.spyOn(api, "subscribePush").mockResolvedValue({ subscribed: true });
    const unsubscribe = vi
      .spyOn(api, "unsubscribePush")
      .mockResolvedValue({ subscribed: false });
    const view = render(() => <NotificationSetting />);

    await waitFor(() => expect(view.container.querySelector(".notify__button")).toBeTruthy());
    fireEvent.click(view.container.querySelector(".notify__button")!);
    await waitFor(() => expect(view.container.textContent).toContain("Notifications are on."));
    fireEvent.click(view.container.querySelector(".notify__quiet")!);

    await waitFor(() => expect(unsubscribe).toHaveBeenCalledWith(subscription.endpoint));
    expect(subscription.unsubscribe).toHaveBeenCalled();
    await waitFor(() => expect(view.container.textContent).toContain("Get a notification"));
  });

  it("reports a blocked permission and an unavailable Space", async () => {
    browser({ permission: "denied" });
    vi.spyOn(api, "pushStatus").mockResolvedValue({ key: "BOr", muted_until: null });
    const view = render(() => <NotificationSetting />);

    await waitFor(() => expect(view.container.querySelector(".notify__button")).toBeTruthy());
    fireEvent.click(view.container.querySelector(".notify__button")!);
    await waitFor(() =>
      expect(view.container.textContent).toContain(
        "Notifications are blocked in your browser settings.",
      ),
    );
  });

  it("stays quiet when the Space has no notification route", async () => {
    browser({});
    vi.spyOn(api, "pushStatus").mockRejectedValue(new Error("not configured"));
    const view = render(() => <NotificationSetting />);

    await waitFor(() => expect(view.container.querySelector(".notify__button")).toBeTruthy());
    fireEvent.click(view.container.querySelector(".notify__button")!);
    await waitFor(() =>
      expect(view.container.textContent).toContain(
        "Notifications are not available in this browser.",
      ),
    );
  });
});
