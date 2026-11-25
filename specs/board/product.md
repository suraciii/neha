# Board

## Authority

Owned by board. A Board depends on [Group](../messaging/group/product.md) and [Chat](../messaging/chat/product.md) for where its Tasks live, and on [Bot](../bot/product.md) for the Bots that claim them.

## Laws

1. A Task lives in one Chat or Group, and a Board holds no Task of its own.
2. A Board grants no access; a User sees only the Tasks they can read.
3. A Task has one state at a time.
4. A claim is a lease; a claim that has expired never changes a Task.

## Boards

A Space has any number of Boards. A Board has a name, the Chats and Groups it includes, the Bots that can claim from it, and optional filters.

A User adds a Board to a Group, which adds the Group to the Board's scope. A Group can show more than one Board.

A Board shows a Task when its conversation is in the Board's scope, the Task matches the Board's filters, and the User can read it.

A Bot on a Board's roster can claim its ready unassigned Tasks. The roster gives no authority beyond what the Bot's owner and its Computer grant.

The User who creates a Board manages it. The owner of a Group manages the Boards shown there.

## Tasks

A Task is a Card in a Message. Its Thread is the work log, and its state is a field the Space owns.

| Field | What it holds |
| --- | --- |
| State | One of the states below |
| Owner | The User who owns the work |
| Assignee | The Bot the work is assigned to, when there is one |
| Tags | Optional labels a Board can filter by |
| Needs | Other Tasks that MUST complete first |

A User or a Bot turns a Message into a Task. The Message becomes the Task's Card, and its Thread becomes the work log.

An assignee is optional. A Task's assignee is a Bot its owner owns.

Only the assignee can claim an assigned Task; a roster never overrides that.

Assigning a ready Task to a Bot posts a Message in the Task's conversation that addresses the Bot.

## States

| State | Meaning |
| --- | --- |
| waiting | A need has not completed. |
| ready | A Bot can claim the Task. |
| claimed | A Bot holds a live claim. |
| blocked | The Task waits for a person: an answer or an Approval. |
| completed | The work is done. |
| failed | The work ended in failure. |
| canceled | No one will do the Task. |

A need that completes moves its dependents from waiting to ready. A need that fails or is canceled leaves its dependents waiting.

A claim moves a ready Task to claimed. Releasing a claim, or letting its lease expire, moves the Task back to ready. The owner can cancel a Task that is waiting, ready, or claimed.

A claimed Task becomes completed, failed, or blocked. A failed Task can become ready again.

The owner of a Task can reopen a completed Task by moving it to ready. A canceled Task stays canceled.

## Claims

A claim is a lease with an epoch and a token. Only the newest claim can change a Task's state.

A claim ends when the work ends, when the Task becomes blocked or canceled, or when its lease expires. A lease expires by the clock alone, and a stale token changes nothing.

A Bot claims a Task only when it can write in the Task's conversation. Writing a Message is not claiming.

## Blocked work

A Task becomes blocked when its work needs a person: a Question or an Approval.

The answer is in the Task's Thread, and the usual approval rules apply. A Question's answer or an Approval's Allow moves the Task back to ready; a Deny moves it to failed, and the action never runs.

A Bot cannot unblock a Task by itself.
