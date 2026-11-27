# Client design system

[product.md](product.md) states what the browser client shows. [design.md](design.md) states its technical boundaries. This page states the UI/UX design that makes those surfaces one product.

## Authority

Owned by client. The design system applies to the browser client and the shells that reuse it. Exact token values live in [styles.css](../../ui/src/styles.css); visual checks live in [testing.md](../../eng/testing.md).

## Design language

### Direction

Warm paper, forest green, and an editorial grotesk. The client reads as a calm instrument: one hierarchy, one accent, one attention state, and no decorative chrome.

### Principles

| Principle | What it requires | What it forbids |
| --- | --- | --- |
| Calm surface | One ground, one accent, one attention color. | Decorative color and competing highlights. |
| Editorial hierarchy | Display type carries the page; body stays quiet. | Kickers above headings and stacked labels. |
| The User's language | Copy names what the User does. | Storage, transport, or encoding details. |
| One authored moment | The attention pill and Approval Card own the accent. | Competing calls to action. |
| Border before shadow | Elevation uses a border or a shadow, not both. | Default card shadows, glass, and decorative gradients. |
| One pattern | Reuse an existing component and state. | One-off variants. |

### Voice

- Calm, direct, and specific; no exclamation marks.
- Product terms stay English in both locales: Space, Bot, Chat, Computer, Provider, Model.
- Chinese copy separates Latin product terms from surrounding text with a space.
- An error states what happened and what the User can do next.
- A control names the action it takes.
- A description is optional; do not restate a title, label, value, or action.

## Design system

### Foundations

#### Color

The client uses a warm paper ground, a forest accent, and an amber attention color. Components MUST use semantic tokens, not raw colors. Body text and placeholders MUST keep at least 4.5:1 contrast.

#### Typography

Schibsted Grotesk carries the interface; Noto Sans SC carries Chinese. Geist Mono is limited to code, commands, URLs, and machine data.

Functional text has a readable floor; body text is the default size. The scale is role-named in [styles.css](../../ui/src/styles.css).

#### Space

Spacing follows one compact rhythm. Component groups stay tight; sections separate with generous space.

#### Elevation

Elevation uses a border or a shadow, not both. The client has no default card shadow.

#### Shapes

Cards use soft corners; controls use a smaller radius. Pills are limited to small controls, avatars, dots, and counts.

#### Motion

Motion explains a change; it never decorates. The client honors reduced motion and keeps one working indicator.

### Layout

One rail, one sidebar, and one main area frame every surface. The product owns the narrow breakpoint; the visual breakpoints and shell sizes live in [styles.css](../../ui/src/styles.css). The setup steps fill the main area on a narrow viewport; the Chat list and Chat share that viewport after setup.

### Components

| Component | Anatomy | States | Source |
| --- | --- | --- | --- |
| Auth card | Card, title, fields, error, locale switch | Join, sign in, working, error | [AuthView.tsx](../../ui/src/components/AuthView.tsx) |
| Setup | Two-step progress, provider and Bot panels | Step 1/2, working, error | [SetupView.tsx](../../ui/src/components/SetupView.tsx) |
| Rail | Brand, Chats, Admin | Selected, idle | [Rail.tsx](../../ui/src/components/Rail.tsx) |
| Sidebar | Compact brand, Chat list, count, footer | Empty, selected | [Sidebar.tsx](../../ui/src/components/Sidebar.tsx) |
| Chat header | Bot, Computer kind, online state, attention | Online, offline, waiting | [ChatView.tsx](../../ui/src/components/ChatView.tsx) |
| Attention pill | Waiting label, compact icon | Waiting | [ChatView.tsx](../../ui/src/components/ChatView.tsx) |
| Chat empty | Avatar, heading, body, prompt chips | No Messages | [ChatView.tsx](../../ui/src/components/ChatView.tsx) |
| Message | User and Bot alignment, dayline, File card | Text, File, failed | [ChatView.tsx](../../ui/src/components/ChatView.tsx) |
| Approval | Heading, tool, summary, reason, scope, actions | Waiting, allowed, denied | [ApprovalCard.tsx](../../ui/src/components/ApprovalCard.tsx) |
| Composer | Input, attach, send, attachment row | Empty, attached, disabled | [ChatView.tsx](../../ui/src/components/ChatView.tsx) |
| Attachment | File icon, name, size, remove | Attached | [ChatView.tsx](../../ui/src/components/ChatView.tsx) |
| File card | Download link, name, size, download icon | Ready, metadata pending | [FileCard.tsx](../../ui/src/components/FileCard.tsx) |

The Approval Card's fields and answers are owned by [the message specification](../messaging/message/product.md). This page owns their visual presentation.

### Patterns

These patterns state composition; behavior is in [product.md](product.md).

- First run: a quiet shell and one raised setup surface.
- Empty: text-only empty states; no empty-state cards or icons.
- Waiting: one attention pill in the header and one raised Approval Card in the stream.
- Failure: one inline recovery line; no modal.
- Narrow: one surface at a time, with Back at the top and the composer at the foot.
- Mobile: 40px primary controls, a 44px attachment row, and safe-area padding.
- Localization: Latin product terms keep the display voice.

### Accessibility

- Focus uses a visible ring with at least 3:1 contrast.
- Primary touch controls meet the 40px target; the compact language switch uses its 36px target.
- Reduced motion is honored.

### Do's and Don'ts

- Do use the semantic tokens and radius scale in [styles.css](../../ui/src/styles.css).
- Do reuse an existing component and state.
- Do keep one accent and one attention color.
- Don't place a kicker or eyebrow above a heading.
- Don't show implementation copy: no cookie names, byte counts, or storage details.
- Don't use monospace outside code, commands, URLs, and machine data.
- Don't combine a border and a shadow on one surface.
- Don't add a second language switch to a surface.
- Don't nest a card inside a card.
- Don't label the one-action Approval `Allow` in the browser. The browser uses `Allow once`; the canonical answer remains Allow.

## Token contract

| Group | Contract | Source |
| --- | --- | --- |
| Color | Warm paper, forest accent, amber attention; semantic roles; 4.5:1 body and placeholder contrast. | `--ground`, `--accent`, `--attention` in [styles.css](../../ui/src/styles.css) |
| Type | 12px functional floor; 15px body; role-named scale. | `--font-*`, `--text-*` in [styles.css](../../ui/src/styles.css) |
| Space | 4px rhythm. | `--sp-*` in [styles.css](../../ui/src/styles.css) |
| Radius | Cards 12–16px; controls 8–10px; pills for small controls. | [styles.css](../../ui/src/styles.css) |
| Elevation | Border or shadow, not both; no default card shadow. | `--line` in [styles.css](../../ui/src/styles.css) |
| Touch | 40px primary controls; 36px compact language switch. | [styles.css](../../ui/src/styles.css) |
| Breakpoints | 900px product boundary; 1080, 560, and 360px visual breakpoints. | [product.md](product.md), [styles.css](../../ui/src/styles.css) |

These rules are the contract. The exact values live in [styles.css](../../ui/src/styles.css). A change to a value that keeps these rules does not change this page.
