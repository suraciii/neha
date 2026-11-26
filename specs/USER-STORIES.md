# User stories

The concrete moments Neha exists for: the start of concrete → abstract → concrete. Each story names what it exercises and opens with a prompt the user can paste. These are analysis context, not work items: they carry no status or priority.

A topic's `VISION.md` links to the stories it serves.

## One Bot, every day

### S1 · Tell the Bot, and it gets done

As a user at my PC, I want to tell my Bot what I need in natural language, so that a question, a task, a document, or a repair gets done here and I refine it by talking.

This is the baseline every agent product has, and Neha has it too.

- Exercises:
  - Chat
  - Bot
  - Computer
  - File
  - Thread
  - Plugin (tools, MCP)
  - approval
- Starter:
  - "Will it rain this afternoon in Taipei?"
  - "Install Docker Desktop and add me to the docker group."
  - "Make a ten-slide deck from this outline for Friday's review."
  - "My laptop fan runs at full speed since the update. Find out why and fix it."
  - "Book me a flight to Shanghai next Tuesday morning, and show me before paying."

### S2 · Runs on its own schedule

As a user with a task that repeats or a condition to watch for, I want to tell my Bot once when to act, so that it runs on its own and tells me when it matters.

- Exercises:
  - Schedule
  - Watcher
  - notification
  - memory
- Starter:
  - "Write the weekly status report from these docs; keep it under a page."
  - "Watch staging, and tell me if the error rate stays above 1% for five minutes."
  - "Watch me file these receipts, then do it every Friday."

## A team of Bots

### S3 · A team in the open

As a user with a multi-stage job, I want to assign one Bot to each stage in a Group, so that handoffs happen where I can see them.

- Exercises:
  - Group
  - Task
  - Thread
  - handoff
- Starter:
  - "@Researcher gather the sources; @Writer draft it; @Reviewer check it against the sources."

## Machines I own, and other agents

### S4 · A Bot on every machine I own

As a user with several machines, I want a Bot that lives on each one and learns it, so that I can get things done on any of them from anywhere.

This is what sets Neha apart. A Bot runs on a Hosted Computer or on the Local Computer where Neha runs; on the Local one, its files, logins, and software are the machine's own. The user reaches it from any client, its screen is a panel in the Chat, and it keeps working when the client is closed.

- Exercises:
  - Bot
  - Computer
  - remote Space
  - phone client
  - memory
  - File
  - panel
  - approval
- Starter:
  - "You manage `web-1`. Learn its layout, and keep away from production data."
  - "Start the GTA6 pre-download on my home PC."
  - "Send me the Q4 deck from my home PC."
  - "Pull the last hour of logs from the API service and find what broke."

### S5 · Hand a repo to another harness

As a user with a favorite coding harness, I want a Bot that runs on it, so that its work lands in my Chat like any other Bot's.

- Exercises:
  - Plugin
  - Bot loop
- Starter:
  - "Have codex refactor the auth module and open a pull request."

### S6 · A Bot that starts working on this PC

As a user at my PC, I want to create a Bot on the Local Computer, so that I can ask for a report or a script without setting up a Workspace first.

The Bot starts in its own Workspace on the machine.

- Exercises:
  - Bot
  - Computer
  - Workspace
  - File
- Starter:
  - "Create a Bot on this PC, then write this up as a one-page report and send it to me as a File."
  - "Make a script that sorts the files in your Workspace."
  - "Show me where you keep your files."
