# CLI

## Authority

Owned by cli. Every topic's behavior is reachable through the CLI, so the CLI depends on all of them and none of them depends on it.

## Laws

1. Whatever a user can do in the GUI, a user or an agent can do from the CLI.
2. A Bot is a CLI user: it reads, posts, replies, schedules, and watches through the CLI, as a user does.
3. The GUI, the CLI, and a Bot all speak to one Space and see the same record.

## Users of the CLI

| User | How it reaches the CLI |
| --- | --- |
| A user at a terminal | Types commands |
| A user in an app | The app issues commands on the user's behalf |
| A Bot | Its loop issues commands as the Bot |

A Bot posting a File into a Group is a Bot posting a Message that carries the File, through the CLI.

## Running a Space

`neha serve` starts a Space on the machine and prints its address and a setup code. The first User opens the address in an app or a browser and enters the code to become the Admin.

## Gaps

- The command surface is not yet specified; it follows the topics as their product contracts settle.
