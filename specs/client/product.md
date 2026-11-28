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

## The desktop entry

The desktop shell uses the same browser client and the same Space sign-in. It
adds only the choice of where the Space lives.

On first launch the shell shows **Choose a Space**:

- **This computer** starts a private bundled Space and opens that Space's
  invitation or sign-in page. The shell never creates the Admin or signs in for
  the User.
- **Existing Space** connects to an `https://` address, or a loopback `http://`
  address for development, and opens that Space's sign-in page.

The chosen Space is remembered. Later launches open it directly; a missing
session shows its sign-in page. **Change Space** forgets the choice, stops the
local Space, and returns to the chooser.

A remote Space starts no local Space and no Local Computer daemon. **This
computer** MAY start a Local Computer only when the User selects the explicit
Local Computer checkbox in the launcher.

On Windows that checkbox is the warning and consent for a daemon whose shell has
no OS sandbox. The daemon starts only after the local Space has a signed-in
session. Chat remains usable when it cannot start.

The shell archives data from an older desktop alpha whose Admin was created
automatically with an unknown generated password; it never silently signs that
account in.

## The shell

The client has one primary navigation rail, one sidebar, and one main area.

The rail selects what the client shows. It has Chats, Needs attention, Bots, Integrations, Skills, and Settings.

| Entry | What it opens |
| --- | --- |
| Chats | The sidebar lists the User's Chats in record order |
| Needs attention | The sidebar lists the items that wait for the User's answer |
| Bots | The sidebar lists the User's Bots; selecting one opens that Bot's page |
| Integrations | The MCP servers and installed packages, grouped by what brought them |
| Skills | The skills the Space holds and the skills of the User's Bots |
| Settings | The main area shows the Space's settings |

With Chats selected, the main area shows the selected Chat. On a viewport 900 CSS pixels or narrower, the client shows the Chat list until a User opens a Chat, then shows that Chat alone with a Back action.

When the User has no Chats, the sidebar shows the Chat-list empty state. When the User has Chats and none is selected, the main area asks the User to select one.

## The installed client

The browser client installs from its Space's public origin as a web app. An installed client opens in its own window and keeps the same pages, record, and session as the browser.

An installed client respects the device's safe areas and keeps the composer above the on-screen keyboard. On a phone, the system Back action closes an open Chat before it leaves the client.

An installed client keeps its shell when the Space cannot be reached and offers to retry. The shell holds no copy of the record.

An installed browser client can receive a system notification when a Bot finishes a run or waits for the User's Approval. Notifications are off until the User enables them on that device. On iPhone and iPad, Web Push requires the installed Home Screen app; the client shows that prerequisite instead of a permission prompt.

A notification names the Bot, not the content of the work, and opens the Chat at the Bot's reply or the Approval Card. The User can mute the whole Space for one hour, or turn notifications off for that device. A mute silences every subscribed device.

A new build of the client takes effect the next time the client opens.

## First run

A new Space has no Provider and no Bot. The main area then shows two steps.

The shell remains during first run. On a viewport 900 CSS pixels or narrower, the steps fill the main area.

1. The Admin connects an OpenAI-compatible Provider with a base URL, an API key, and one Model name. The API key is never displayed again.
2. The User creates a Bot with a name. A new Bot has one owned Chat and one Hosted Computer.

A User who is not the Admin sees that an Admin MUST connect the Provider. The client creates no Bot before a Provider exists.

After the Bot is created, the client opens its Chat.

## The Chat list

Each row in the Chat list shows the Chat's state: Working, Waiting, or Unread. [Chat](../messaging/chat/product.md#state) defines the states and their precedence.

Unread is the client's own memory of a Message that landed while the User was not looking at the Chat. The record holds no such state, and the badge clears when the User opens the Chat.

## A Chat

A Chat shows its Messages oldest first, grouped by day. A User Message aligns to the end of the stream; a Bot Message aligns to the start.

A Message that carries a File shows a file card with the File's name and size. Choosing the card downloads the File, and the browser opens no File itself. [Message](../messaging/message/product.md#files) owns what a File is and where its bytes live.

The Chat header names the Bot, the Computer kind (Local Computer or Hosted Computer), and whether the Computer is online.

The header shows the Chat's [state](../space/design.md#chat-state): working, waiting for an Approval, or failed. The Approval Card remains in the stream while the Run waits. An Approval for a server's tool names the server and the tool.

A Message MAY carry an interactive Card from a tool; [Plugin](../plugin/product.md#cards) states what the Card can do.

A Chat with no Messages shows the Bot's name and starting prompts.

The composer sends one Message. Enter sends; Shift and Enter insert a new line. The composer does not accept an empty Message.

While a Run is active, the Chat shows that the Bot is working and does not accept another Message from the same User.

A failed Run leaves a Bot Message that states the failure. The Chat keeps the Message in its place.

## Tool activity

While a Run works, the Chat shows one collapsible activity card for that Run. The card starts collapsed, and its title row names the state, the step, and the tools in use.

An action appears when it starts and updates with its result when it completes. A failed Run expands the card and shows its reason, and the card never shows an action's raw arguments.

## Needs attention

The Needs attention entry opens the [Needs attention view](../messaging/notification/product.md#needs-attention). Each item names its Bot, and selecting one opens the Chat where the item waits.

## Bots

The Bots entry lists the User's Bots. Selecting one opens that Bot's page.

The list offers **New Bot**: the User names the Bot and picks its Computer. The choice lists the Local Computers the User owns, and one that is offline cannot be picked. Without a choice the Bot starts on a new Hosted Computer.

The client opens the new Bot's Chat. [Bot](../bot/product.md#creating-a-bot) owns what a new Bot starts with, and [Computer](../computer/product.md#adding-a-local-computer) owns when a paired Computer is online.

A Bots list with no Bot shows the same action.

The page shows the Bot's name, its picture, its Computer, its Model, and its access. [Bot](../bot/product.md#the-bots-page) owns each field and who changes it.

The owner changes the Bot's name on the page. The page shows the Computer's kind and whether it is online, and the owner moves a Local Bot to another Local Computer they own. A Hosted Bot does not move.

The owner chooses the Bot's Model and thinking level on the page, and clears each back to the Space's default. Without one of its own, the Bot uses the Space's default, which the Admin sets in Settings. [Model](../model/product.md#choosing) covers the choice.

A Local Bot's page shows the folders it can write and whether it has full access. The owner grants a folder, revokes one, and turns full access on or off there. The page offers only the access the Bot's Computer supports.

Turning full access on states the risk and asks the owner to confirm, as the CLI does at a terminal. [Computer](../computer/product.md#what-a-bot-can-do) owns the access model.

The Bot's page lists its Ask and Allow rules, and the owner removes them there. The page adds rules for actions the Bot's Computer runs, and those rules belong to a Local Computer: a Hosted Computer takes none of them.

A Hosted Bot's page still lists and removes the rules it holds for calls the Space answers — a skill's script or an MCP server's tool — which apply on either Computer. [Bot](../bot/product.md#asking-before-an-action) owns what a rule covers.

The page shows who can use the Bot, and changing that is not yet available.

## Integrations

The Integrations view lists the MCP servers the Space and the signed-in User hold, and the packages the Admin installed. Each server shows whether it is connected, who connected it, and the Bots that use it.

[Plugin](../plugin/product.md) states who adds or installs what. A server added with a sign-in opens the browser's sign-in, and the client never shows a credential after it is saved.

## Skills

The Skills view lists the skills the Space holds and the skills of the User's Bots; adding and enabling follow [Plugin](../plugin/product.md#skills).

## Settings

The Settings entry opens the Space's settings in the main area. Every User reads them, and only the Admin changes them.

The settings list the Space's Providers, each with its protocol and address, and the Models each Provider offers. The Admin connects a Provider and refreshes its Models there. [Model](../model/product.md#providers) covers connecting one.

The settings show the catalog's state: the snapshot or the cache in use, its age, and whether it is stale. The Admin refreshes the catalog there. [Model](../model/product.md#catalog) covers the catalog.

The Admin sets the Space's default Model in the settings. [Model](../model/product.md#choosing) covers the default and what a Bot inherits.

A Space with no Provider shows that empty state with the action that connects one. Each Provider's Models list shows its own empty state when it offers none.

## Computers

The settings list every Computer the User owns, Local and Hosted, with its kind and whether it is online. Each Computer says what a Bot's shell runs as on it, and one that has not answered the Space yet says it waits. [Computer](../computer/product.md) owns what a Computer is.

The owner resets a Hosted Computer from that list. The reset asks for confirmation and names what it deletes. [Computer](../computer/product.md#keeping-files) covers the reset.

A Bot moves on its own page, between the Local Computers its owner owns.

## Language

The client supports English (en-US) and Simplified Chinese (zh-CN).

A first visit follows the browser language. The User can switch language from the sign-in card and from the signed-in client. The choice belongs to the browser and never changes the record.

The client shows one language switch at a time. A count of Chats uses the product term Chat.

Product terms stay in English in both locales: Space, Bot, Chat, Computer, Provider, Model, Plugin, MCP server. Dates follow the active locale; times use a 24-hour clock in both locales.

## Accessibility

- Every control has an accessible name; focus is visible.
- A waiting Approval is exposed as a status, not by color alone.
- On a narrow viewport, controls meet the touch targets in [design-system.md](design-system.md).

## Page states

Every page distinguishes loading, empty, ready, and failed. An error states what failed and what the User can do next.

A failed command keeps the record the client already shows. The client retries nothing on its own.

## Gaps

- Changing who else can use a Bot is not yet available; the Bot's page shows that access read-only.
