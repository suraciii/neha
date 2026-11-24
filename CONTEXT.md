# Neha

This file defines the domain language shared by the specifications, the code, and the documentation.

## Language

Real operations own the unqualified vocabulary. A modeled convention carries a distinguishing qualifier and does not occupy an unqualified term of a real operation.

A term enters this file when a specification needs it. Each entry is one paragraph: the term in bold, a colon, the definition, then an `_Avoid_:` line naming the words it replaces.

**User**:
A human actor who owns Bots and Computers and grants authority. An OS account is never a User.
_Avoid_: person, principal, member

**Space**:
The Neha that Users belong to: it holds the record, the Users, the Bots, and their memory. The desktop app runs a built-in Space or connects to a remote one.
_Avoid_: instance, workspace, server, community, tenant, organization

**Admin**:
The User who runs a Space: invites its Users and configures the model providers it offers.
_Avoid_: operator, owner (of a Space), manager

**Bot**:
Neha's own agent: a persistent identity with memory and a Computer that takes work and returns results. The vision calls it a teammate.
_Avoid_: teammate (as a term), agent (when meaning Neha's own), assistant, primary bot

**Computer**:
The machine a Bot runs on, managed or real; a real Computer runs Neha. Each Bot has its own Computer, chosen by its owner, and several Bots may share one.
_Avoid_: host, sandbox (as the umbrella), VM, device

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
An image, document, or other artifact attached to a Message. A File lives in the Space, whatever Computer produced it.
_Avoid_: attachment, artifact, upload, block

**Card**:
Structured content in a Message that is more than text: an approval request, a question, a page, a dashboard.
_Avoid_: block, widget, embed, canvas

**Task**:
A Card with a state, an owner, and optional tags and dependencies; its Thread is the work log. A Task is made when work needs to be claimed, handed off, or waited on.
_Avoid_: ticket, issue, work item, job

**Routine**:
A standing instruction that tells a Bot when to start working: on a schedule, when a condition is met, or after being shown once.
_Avoid_: trigger, schedule, automation, job

**Plugin**:
The unit of replaceability. Every part of Neha — model, tools, Bot loop, Group — is a Plugin, and a Plugin replaces any of them or adds to it.
_Avoid_: extension, addon, integration, connector
