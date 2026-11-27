# Notifications

An installed Neha can tell you when a Bot finishes a run or waits for your approval. Notifications are opt-in on each device. They never carry the content of the work; they name the Bot and open the Chat.

## What sends one

- A Bot finishes a run, whether it succeeded or failed.
- A Bot waits for your approval.

Progress, steps, and ordinary Bot Messages do not send a notification. An approval that waits also shows in **Needs attention**; its notification opens the Approval Card.

## Enable

Notifications need the public HTTPS origin from [Serve a Space on a domain](deploying.md). A local Space uses a `*.localhost` browser host and does not register the service worker, so the notification control does not appear there. The Electron desktop shell does not use Web Push either; use the installed browser app on the public origin.

### Android and desktop browsers

1. Install Neha from its public origin, or open it in a supported browser.
2. Open **Notifications** in the bottom of the Chat list.
3. Choose **Enable notifications**, and allow them in the browser.

### iPhone and iPad

Web Push on iOS and iPadOS works only in an app added to the Home Screen (iOS 16.4 or later).

1. Open the Space in Safari.
2. Tap Share, then **Add to Home Screen**.
3. Open Neha from the Home Screen.
4. Open **Notifications** in the Chat list, choose **Enable notifications**, and allow them.

If you open the Space in Safari instead, Neha shows the Add to Home Screen hint and does not ask for permission.

## Mute and turn off

- **Mute for 1 hour** silences the whole Space on every subscribed device. Open the same control and choose **Unmute** to end it early.
- **Turn off on this device** removes this browser's subscription. Other devices keep theirs.
- The browser's own site settings can also block notifications; Neha reports when that is the case.

A Space that is not running cannot send a notification.

## Privacy

The Space encrypts each payload for the browser that subscribed. Your browser vendor's push service delivers the encrypted bytes and sees the endpoint, timing, and payload size, not the Bot's name or the Chat. The notification body says what happened, not what the Bot said; open it to read the Chat.

Subscriptions and the VAPID signing key stay in the Space's home. The key is sealed with the Space's secrets; keep the master key with your backup. Replacing that key silently would invalidate every subscription, so Neha never rotates it on its own.

## Troubleshooting

| What you see | What to check |
| --- | --- |
| No **Notifications** control | The Space is not on a public HTTPS origin, the browser does not support Web Push, or this is the Electron desktop shell. |
| Add to Home Screen hint on iPhone | Install from Safari's Share menu and open the installed app. |
| Notifications are blocked | Allow notifications for this site in the browser's settings. |
| Nothing arrives | The Space may be muted, this device may be turned off, or the Space may be stopped. |
| The control says notifications are unavailable | The Space could not create its VAPID key or read its push state; check its logs, the `secrets` directory, and `state/push.json`. A corrupt state file is left untouched; repair or remove it after a backup and restart. |
