# Client

## Authority

Owned by client. Every client reaches a Space through the [CLI](../cli/design.md) command surface. Identity owns the session token, and messaging owns the record a Chat shows.

The **browser client** is the Space-served client a User reaches over HTTP and one WebSocket. It joins a Space, signs in, and carries one-to-one Chat.

## Laws

1. The Space serves the browser client from its own origin.
2. The browser client keeps its session token only in the Space's HttpOnly cookie.
3. The browser client never puts the token in JavaScript state, local storage, or a URL.
4. The browser client shows only the signed-in User's Chats and Bots.
5. A browser client reaches one Space.

## Joining

A fresh Space prints one invitation link. The link carries the secret in its fragment, so the page request never sends it to the Space. The client sends the secret only in the join request.

The invitation page asks for a display name and a password, then joins the first User as the Admin. A used invitation is not usable again.

When the Space already has an account, the client asks for the display name and password of an existing account. A successful sign-in sets the session cookie and opens the client.

A client without a valid session shows the sign-in page. A client with a valid session opens the Chats view.

## The shell

The client has one primary navigation rail, one sidebar, and one main area.

The rail selects what the sidebar contains. It has Chats, and the sidebar lists the User's Chats in record order.

The main area shows the selected Chat. On a viewport 900 CSS pixels or narrower, the client shows the Chat list until a User opens a Chat, then shows that Chat alone with a Back action.

When the User has no Chats, the sidebar shows the Chat-list empty state. When the User has Chats and none is selected, the main area asks the User to select one.

## First run

A new Space has no Provider and no Bot. The main area then shows two steps.

The shell remains during first run. On a viewport 900 CSS pixels or narrower, the steps fill the main area.

1. The Admin connects an OpenAI-compatible Provider with a base URL, an API key, and one Model name. The API key is never displayed again.
2. The User creates a Bot with a name. A new Bot has one owned Chat and one Hosted Computer.

A User who is not the Admin sees that an Admin MUST connect the Provider. The client creates no Bot before a Provider exists.

After the Bot is created, the client opens its Chat.

## A Chat

A Chat shows its Messages oldest first, grouped by day. A User Message aligns to the end of the stream; a Bot Message aligns to the start.

The Chat header names the Bot, the Computer kind (Local Computer or Hosted Computer), and whether the Computer is online. While an Approval waits, the header marks the wait; the Approval Card remains in the stream. A Chat with no Messages shows the Bot's name and starting prompts.

The composer sends one Message. Enter sends; Shift and Enter insert a new line. The composer does not accept an empty Message.

While a Run is active, the Chat shows that the Bot is working and does not accept another Message from the same User.

A failed Run leaves a Bot Message that states the failure. The Chat keeps the Message in its place.

## Language

The client supports English (en-US) and Simplified Chinese (zh-CN).

A first visit follows the browser language. The User can switch language from the sign-in card and from the signed-in client. The choice belongs to the browser and never changes the record.

The client shows one language switch at a time. A count of Chats uses the product term Chat.

Product terms stay in English in both locales: Space, Bot, Chat, Computer, Provider, Model. Dates follow the active locale; times use a 24-hour clock in both locales.

## Accessibility

- Every control has an accessible name; focus is visible.
- A waiting Approval is exposed as a status, not by color alone.
- On a narrow viewport, controls meet the touch targets in [design-system.md](design-system.md).

## Page states

Every page distinguishes loading, empty, ready, and failed. An error states what failed and what the User can do next.

A failed command keeps the record the client already shows. The client retries nothing on its own.