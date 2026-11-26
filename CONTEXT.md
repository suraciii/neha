# Neha

This file defines the domain language shared by the specifications, the code, and the documentation.

## Language

Real operations own the unqualified vocabulary. A modeled convention carries a distinguishing qualifier and does not occupy an unqualified term of a real operation.

A term enters this file when a specification needs it. Each entry is one paragraph: the term in bold, a colon, the definition, then an `_Avoid_:` line naming the words it replaces.

**User**:
A human actor who owns Bots and Computers and grants authority. An OS account is never a User.
_Avoid_: person, principal, member

**Account**:
A User's identity on a Space: a display name, a picture, and one or more ways to sign in.
_Avoid_: login, profile, credential

**Space**:
The Neha that Users belong to: it holds the record, the Users, the Bots, and their memory. The desktop app runs a built-in Space or connects to a remote one.
_Avoid_: instance, workspace, server, community, tenant, organization

**Admin**:
The User who runs a Space: invites its Users, connects its Providers, and configures the Space.
_Avoid_: operator, owner (of a Space), manager

**Bot**:
Neha's own agent: a persistent identity with memory and a Computer that takes work and returns results. The vision calls it a teammate.
_Avoid_: teammate (as a term), agent (when meaning Neha's own), assistant, primary bot

**Computer**:
The machine a Bot runs on, managed or real; a real Computer runs Neha. Each Bot has its own Computer, chosen by its owner, and several Bots may share one.
_Avoid_: host, sandbox (as the umbrella), VM, device

**Workspace**:
The durable file tree a managed Computer keeps in the Space. A real Computer's disk holds its files instead.
_Avoid_: home, volume, sandbox disk

**Run**:
One pass of a Bot's loop, from a request to a reply or a failure. A run holds steps and the actions the Bot took.
_Avoid_: turn, job, execution

**Step**:
A checkpoint in a run: one Model call and the actions that call requests. A run resumes from a completed step.
_Avoid_: iteration, cycle, phase

**Action**:
One thing a Bot does on its Computer or with a tool. The record holds an action before it runs.
_Avoid_: call, command, tool call

**Vault**:
The Space's store of passwords and keys that a Bot uses without seeing.
_Avoid_: wallet, safe, secret store

**Provider**:
A source of Models a Space connects to: a known vendor reached with an API key, or an OpenAI-compatible server reached with its address.
_Avoid_: vendor, backend, adapter, service

**Model**:
What a Bot thinks with, offered by a Provider. A Model states whether it reads images, whether it thinks, and its context limit.
_Avoid_: LLM, engine, model name

**Chat**:
The conversation between one User and one Bot. Each User allowed to use a Bot has one Chat with it; the owner's is created and removed with the Bot.
_Avoid_: home, DM, 1:1, main chat, private chat

**Group**:
A conversation among any number of Users and Bots, created by a User. Users and Bots are members alike, and the Group is the shared record in which they coordinate: Tasks, handoffs, and approvals happen in it.
_Avoid_: channel, room, chat (unqualified), thread (as a container)

**Message**:
A unit of a Chat's or a Group's stream, written by a User or a Bot. Its content is text, and it can carry a File or a Card; any Message can carry a Thread.
_Avoid_: post, event, reply (as the unit)

**Thread**:
A single-level reply chain under a Message. Under a Task, it is the work log.
_Avoid_: nested thread, thread (as a Chat or Group)

**File**:
An image, document, or other artifact attached to a Message. A File lives in the Space, whatever Computer produced it. A File belongs to the Chat or Group it was sent in.
_Avoid_: attachment, artifact, upload, block

**Card**:
Structured content in a Message that is more than text: an approval request, a question, a page, a dashboard.
_Avoid_: block, widget, embed, canvas

**Task**:
A Card with a state, an owner, an optional assignee, and optional tags and dependencies; its Thread is the work log. A Task is made when work needs to be claimed, handed off, or waited on.
_Avoid_: ticket, issue, work item, job

**Board**:
A named view over selected Chats and Groups, with optional filters and a roster of Bots that claim its Tasks. A Board holds no Task of its own.
_Avoid_: kanban, project, workspace (as a board)

**Schedule**:
A standing instruction that tells a Bot to start work at a time, once or repeatedly.
_Avoid_: routine, trigger, automation, cron, job

**Watcher**:
A standing instruction that tells a Bot to start work when a condition is met.
_Avoid_: routine, trigger, automation, job

**Skill**:
A reusable set of instructions for how to do a task. A skill tells a Bot how and adds no authority. A skill is an instruction kept in the record, in a Space's library or a Bot's; a Plugin distributes it.
_Avoid_: recipe, playbook, macro, workflow

**Kernel**:
The fixed part of Neha that Plugins attach to: accounts and authority, the record, the composition of Plugins, the clock, and the Approval gate.
_Avoid_: core, platform, foundation, system

**Plugin**:
The unit of replaceability. Every part of Neha outside the Kernel — model, tools, Bot loop, Group — is a Plugin, and a Plugin replaces any of them or adds to it.
_Avoid_: extension, addon, integration, connector
