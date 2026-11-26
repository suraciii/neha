# CLI

## Authority

Owned by cli. The CLI depends on every topic for its behavior. No topic depends on the CLI for its behavior; every topic is reached through it.

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

`neha serve` starts a Space on the machine and prints its address and an invitation link. The first User opens the link in an app or a browser to become the Admin.

## Computers

A real Computer runs the CLI. `neha computer pair` joins the machine to a Space and prints a pairing code, which a signed-in User enters with `neha computer add`. `neha computer run` starts the machine again after a stop.

`neha computer list` shows the Computers the User owns, managed and real. `neha bot create --computer` starts a new Bot on one of the User's real Computers, and `neha bot move` moves a Bot to another real Computer.

## Gaps

- The command surface is not yet specified; it follows the topics as their product contracts settle.
