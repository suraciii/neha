# Search

## Authority

Owned by messaging. Search depends on [Message](../message/product.md), [File](../message/product.md), [Bot](../../bot/product.md), and [Group](../group/product.md) for what it covers.

## Laws

1. Search covers only what the User can read.
2. A result opens at its place in the record.

## What is searchable

| Kind | Matched by | Opens |
| --- | --- | --- |
| Message | Its text | The Chat or Group at that Message |
| File | Its name | The Message carrying it |
| Bot | Its name, among the Bots the User can use | Its Chat with the User |
| Group | Its name | The Group |

A User's own Chats and the Groups they belong to are searched. Another User's Chats, deleted content, the Vault, and a Bot's memory are never searched.

## Asking

A User searches from the search box in any app and from the CLI.

Recent results come first. A User can widen a search to older Messages, or narrow it to one Chat or Group, one Bot, Messages with a File, or a time range.

## Results

Results are grouped by kind, each showing where it is, who wrote it, when, and a snippet. Choosing a result opens it at its place.
