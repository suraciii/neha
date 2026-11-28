import { describe, expect, it } from "vitest";
import { commandFrame, UNIT_COMMANDS } from "./ws";

const id = "01890a5d-ac96-774b-bcce-b302099a8057";

describe("commandFrame", () => {
  it("leaves args off a unit command, as the Space's decoder reads it", () => {
    expect(commandFrame(id, "provider_list")).toStrictEqual({
      type: "command",
      id,
      command: { name: "provider_list" },
    });
    expect(commandFrame(id, "chat_list")).toStrictEqual({
      type: "command",
      id,
      command: { name: "chat_list" },
    });
  });

  it("states the args object of an argument command", () => {
    expect(commandFrame(id, "chat_read", { chat: id })).toStrictEqual({
      type: "command",
      id,
      command: { name: "chat_read", args: { chat: id } },
    });
  });

  it("states an empty args object for a command whose fields are all optional", () => {
    expect(commandFrame(id, "skill_list", {})).toStrictEqual({
      type: "command",
      id,
      command: { name: "skill_list", args: {} },
    });
  });

  it("refuses args on a unit command", () => {
    expect(() => commandFrame(id, "mcp_list", {})).toThrow(/carries no args/);
  });

  it("refuses an argument command that names no args object", () => {
    expect(() => commandFrame(id, "skill_list")).toThrow(/requires an args object/);
    expect(() => commandFrame(id, "skill_list", null)).toThrow(/requires an args object/);
  });

  it("refuses a name the Space does not know instead of guessing its shape", () => {
    expect(() => commandFrame(id, "run_list")).toThrow(/knows no command/);
  });

  it("classifies every unit command exactly as a frame without args", () => {
    for (const name of UNIT_COMMANDS) {
      expect(commandFrame(id, name)).toStrictEqual({
        type: "command",
        id,
        command: { name },
      });
    }
  });
});
