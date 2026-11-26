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

A User sees a Needs attention view: the Questions and Approvals waiting on them, the runs that failed, the Group Messages that mention them, and the work an offline Computer blocks.

The view lists only what the User can read.

Opening an item opens its source Message in its Chat or Group.

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

A User can mute the whole Space for a time.

## Delivery

A Notification arrives as a system notification on the User's desktop and phone, and as a badge on the Chat's or Group's row.

Opening a Notification opens the Chat or Group at the Message that started it.
