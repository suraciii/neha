# Chat

## Authority

Owned by messaging. A Chat depends on [Message](../message/product.md) for its stream and on [Bot](../../bot/product.md) for who can open one.

## Laws

1. A User has at most one Chat with a Bot.
2. The owner's Chat exists for as long as the Bot does.
3. A Chat shows at most one state at a time.

## Opening a Chat

The owner's Chat is created with the Bot. A User the owner allows gets a Chat with the Bot when they first open it.

A User who is not allowed to use a Bot sees no Chat with it.

## The stream

A Chat shows its Messages oldest first, and a new Message appears at the end.

A User sends a Message from the composer at the bottom of the Chat. Every Message in a Chat is for its Bot.

A Thread opens under the Message it replies to, and closes again.

## State

A Chat shows one of three states.

| State | When it starts | When it ends |
| --- | --- | --- |
| Working | The Bot starts working on a request | The work ends, or the Bot stops for the User |
| Waiting | A Question or an Approval from the Bot lands | The User answers |
| Unread | A Message lands while the User is not looking at the Chat | The User opens the Chat |

The state shows on the Chat's row in the sidebar, next to the Bot's name and the latest Message.

Working takes precedence over Unread, and Waiting over both.

## Answering

The User answers a Question or an Approval in the Chat where it landed. A Question's answer is a Message in that Chat, and an Approval's answer is recorded as its decision.

A Bot's question or approval is answered only by the User it acts for.

## End

A Chat ends when its Bot is deleted, and its Messages go with it.
