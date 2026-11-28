# Space

## Why

A user installs the desktop app and it works on the spot. Later they want their Bots to keep working while the laptop is closed, so they run the same product on a server and connect the app to it; the product is the same from the phone as well.

A Space is the Neha that holds everything: Users, Bots, their memory, every Chat and Group. It has to be usable with no setup and ownable end to end, and it has to serve one User or several with the same model.

## What

A Space is one Neha that holds the record, the Users, the Bots, and their memory.

The desktop app can start its own Local Space or connect to a Hosted Space.

A phone connects to a Space; it reaches a Hosted Space over the network and a Local Space when that Space's origin is reachable.

The Admin of a Space invites Users and configures the model providers everyone on it uses.

Each User signs in to a Space and sees their own Bots, Chats, and Groups from any client.

A Space sends notifications when a Bot finishes or waits for a User.

## Why this way

A Local Space lets a user start with nothing to install but the app; a Hosted Space is the step taken when the user wants Bots to keep working while the computer is asleep or off.

One model for both keeps a Local Space's Users and a Hosted Space's Users on the same product; connecting to a Hosted Space changes nothing but where the Space runs.

The Admin configures providers because someone has to, and a model is a choice about the whole Space; a Bot can still pick its own.

Notifications belong to the Space because they cross every Chat and Group and reach the user outside the app.

## Principles

- **Local or Hosted, the same product.** The desktop app starts its own Local Space or connects to a Hosted Space; a phone connects to either when it can reach the origin.
- **The Admin invites Users.** The Admin of a Space decides who uses it and configures the model providers it offers.
- **One model for one user or many.** A Space serves its Users with the same record, Bots, and Groups, whether there is one User or several.
- **Notifications reach the User outside the app.** A Space notifies a User when a Bot finishes or waits for them.

## What this enables

- Starting with nothing to set up, and growing to always-on by moving to a Hosted Space.
- Several users sharing one Space, each with their own Bots.
- The same Bots from a laptop, a phone, and another PC.

## User stories

- [S4 · A Bot on every machine I own](../USER-STORIES.md#s4--a-bot-on-every-machine-i-own)
