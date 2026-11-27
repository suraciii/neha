# Notification

## Authority

Owned by messaging. A Notification depends on [Chat](../chat/product.md) and [Group](../group/product.md) for what happened and on [Space](../../space/product.md) for reaching the client.

## Laws

1. A Notification names one Chat or Group and opens it.
2. A User is notified only of what they can read.
3. Nothing is sent by email.

## What starts a Notification

| Event | Who is notified |
| --- | --- |
| A Bot finishes work | The User it acts for, in a Chat; in a Group only at the Everything level |
| A Bot waits for a User: a Question or an Approval | The User it acts for |
| A User is addressed with `@` in a Group | That User |
| `@all` in a Group | Every member |
| A Bot posts in a Group without addressing anyone | Nobody |

A Bot's progress never starts a Notification.

## Needs attention

A User sees a Needs attention view: the Approvals waiting on them. A failed run and an offline Computer wait for no answer, so the view holds neither.

The view lists one entry for each waiting Approval, named by its id, and not one entry for a Chat.

The view lists only what the User can read.

Opening an item opens its source Message, the Card that asked, in its Chat or Group. An item leaves the view when the User answers it.

The view is a projection of the record, not a second stream of Messages.

A Notification for an item in the view points at that item.

## Default levels

| Conversation | By default, a User is notified of |
| --- | --- |
| Chat | Every result, Question, and Approval from the Bot |
| Group | Being addressed, and a Bot waiting for them |

## Overrides

A User sets a level on any Chat or Group.

| Level | What arrives |
| --- | --- |
| Everything | Every Message from a Bot |
| Addressed and waiting | Mentions, and a Bot waiting for the User |
| Off | Nothing |

A User can mute the whole Space for a time. A mute silences every device.

## Delivery

A Notification arrives as a system notification on the User's desktop and phone, and as a badge on the Chat's or Group's row.

A browser receives Web Push only after the User enables notifications on that device. On iOS and iPadOS, Web Push is available only to an installed Home Screen app; the client shows that prerequisite instead of asking for permission.

A subscription belongs to one device. The Space stores it and sends only to the devices of the User it acts for, over a public HTTPS push service. Redirects stay disabled and a private destination is refused.

The payload names the Bot and what happened, not the content of the work, and carries a same-origin address, never a token.

Opening a Notification opens the Chat or Group at the Message that started it. A result opens the Bot's reply Message; a waiting Approval opens its Card.

## Gaps

- A Question has no record event yet; the Needs attention view lists Approvals until the record holds one.
- Groups, `@` mentions, Questions, and per-conversation levels are not implemented. Web Push in this build covers one-to-one Chats.
- The client's Space mute lasts one hour.
- The Electron desktop shell does not use Web Push in this build; use the installed browser app.
