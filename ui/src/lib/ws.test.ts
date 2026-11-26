import { describe, expect, it } from "vitest";
import { commandFrame } from "./ws";

const id = "01890a5d-ac96-774b-bcce-b302099a8057";

describe("commandFrame", () => {
  it("puts a unit command under its name", () => {
    expect(commandFrame(id, "chat_list")).toEqual({
      type: "command",
      id,
      command: { name: "chat_list" },
    });
  });

  it("puts a command's arguments under args", () => {
    expect(commandFrame(id, "chat_read", { chat: id })).toEqual({
      type: "command",
      id,
      command: { name: "chat_read", args: { chat: id } },
    });
  });
});
