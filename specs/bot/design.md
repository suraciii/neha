# Bot

[product.md](product.md) states what a Bot does for a User. This page states where a Bot's work runs, what a Bot can do, and what holds its state.

## Authority

Owned by bot. A Bot's work depends on [Computer](../computer/product.md) for where its actions execute, on [CLI](../cli/product.md) for how its loop reaches the Space, and on [Identity](../identity/product.md) for the credential its loop uses.

## Laws

1. A Bot's loop acts as the Bot and holds no record.
2. Every action a Bot takes runs on its Computer.
3. What a Bot has seen is reconstructible from the record.
4. A Bot does only what Neha can enforce on its Computer.
5. A run stops between two of its actions and never inside one.

## Where work runs

The built-in loop runs with the Space. It reaches the Space as the Bot does, through the command surface, with the credential the Space gave the Bot.

The built-in loop sends a Bot's actions to the Computer: commands, file reads and writes, and screen actions. The Model, the Bot's memory, and its conversation live in the Space.

An outside harness is a loop that runs on the Bot's Computer. Its work lands in the record as any Bot's work does.

## State

The record holds a run: its requests, its steps, the actions the Bot took, and what the Bot saw. A demonstration holds what the User did and saw while in control, and never holds a credential.

A summary the Bot writes is a record event. The history a Model sees is a projection of the record, and the record keeps every original Message.

A Bot's memory is a set of entries in the record that belong to the Bot. Each entry records its author, where it came from, and the scope it applies to. The system prompt is rebuilt for every request from the entries that apply to the acting User and the conversation.

## Steps and actions

A step holds one Model call and the actions that call requests. A completed step never runs again; the record gives its result back to the context. A step interrupted before it completes runs again from its start.

The resumed step replays the results of the actions it already recorded, then continues from the first action it did not record.

An action is recorded before it runs and after it returns. A started action never runs twice.

An action whose result never returns is unknown. The loop checks its effect on the Computer, or asks the User, and does not run that started action again.

## Waiting and resuming

A loop stops a run when the Bot's Computer goes offline and resumes it when the Computer is back. A Message sent to an offline Computer waits in the record.

A run stops between two actions when it waits for an Approval or an answer, when its Computer goes offline, or when a User takes control of its screen. It resumes after the last recorded action, and a recorded action never runs again.

A managed Computer's recovery, reset, or replacement stops its runs between two actions before its saved workspace is replaced.

A failed run does not resume; the Bot says so as a Message. A run never resumes a Model stream; [Model](../model/design.md) covers that.

## Instructions and data

A lasting rule is an instruction to the Bot within the authority of the User who gave it.

Everything else a Bot reads is data: tool results, files, web pages, messages, and what the Bot noticed itself. Data enters the context with a label that names where it came from.

A message from another surface, a bridge, or another agent carries no authority, whoever it claims to come from.

A skill that the Bot's owner enabled is an instruction. A skill adds no authority, and a script it carries runs only as one of the Bot's actions. A skill the Bot wrote itself becomes an instruction only when its owner approves it.

## Authority for an action

On a Computer, a Bot does what that Computer's owner granted. The User a Bot acts for approves each action that needs an approval.

An approval covers one action of one run, and Neha checks it again when the action runs. It ends when that User leaves the conversation, when the Computer is revoked, or when the run is cancelled.

A lasting rule from a User does not cover an action the Computer's owner requires asking for.

## Runs

A Bot runs one run at a time for one User in one conversation.

A Message from that User that answers the run's request steers the run. Another request from the same User waits for a run of its own.

Runs for different Users, and runs in different conversations, run in parallel.

A handoff from one Bot to another carries the authority of the User the first Bot acted for, and never more.
