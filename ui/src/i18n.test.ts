import { flush } from "solid-js";
import { afterEach, describe, expect, it } from "vitest";
import { locale, setLocale, t, toggleLocale } from "./i18n";

afterEach(() => {
  flush(() => setLocale("en-US"));
  localStorage.clear();
});

describe("i18n", () => {
  it("translates product copy", () => {
    expect(t("auth.headline")).toBe("Put work in the hands of a team you can see.");
    expect(t("bots.new")).toBe("New Bot");
    // A new Bot works on a Computer of its own unless one is chosen.
    expect(t("bots.new.computer")).toBe("Computer");
    expect(t("bots.new.computer.offline")).toContain("offline Computer");
    expect(t("bots.new.noProvider")).toContain("Provider");
    flush(() => setLocale("zh-CN"));
    expect(t("auth.headline")).toBe("把工作交给一支你看得见的团队。");
  });

  it("interpolates names", () => {
    expect(t("chat.aria", { name: "Quill" })).toBe("Chat with Quill");
    flush(() => setLocale("zh-CN"));
    expect(t("chat.aria", { name: "Quill" })).toBe("与 Quill 的 Chat");
  });

  it("keeps product terms and Chinese spacing", () => {
    flush(() => setLocale("zh-CN"));
    expect(t("chat.localComputer")).toBe("Local Computer");
    expect(t("chat.hostedComputer")).toBe("Hosted Computer");
    expect(t("sidebar.chats.one")).toBe("1 个 Chat");
    expect(t("sidebar.chats.many", { count: 3 })).toBe("3 个 Chat");
    expect(t("chat.new.title", { name: "Nova" })).toBe("和 Nova 打个招呼。");
    expect(t("chat.aria", { name: "Nova" })).toBe("与 Nova 的 Chat");
    // The action that makes a Bot, in the User's words.
    expect(t("bots.new")).toBe("新建 Bot");
    expect(t("bots.new.computer")).toBe("Computer");
    expect(t("bots.new.computer.offline")).toContain("离线");
    expect(t("bots.new.noProvider")).toContain("Provider");
  });

  it("names what a Chat waits for, in the reader's language", () => {
    expect(t("attention.title")).toBe("Needs attention");
    expect(t("attention.count.one")).toBe("1 approval waiting");
    expect(t("attention.count.many", { count: 2 })).toBe("2 approvals waiting");
    expect(t("attention.empty.title")).toBe("Nothing waits");
    expect(t("chat.newActivity")).toBe("New activity");
    expect(t("chat.failed")).toBe("Failed");
    expect(t("chat.activity.step", { count: 2 })).toBe("Step 2");
    expect(t("chat.activity.succeeded")).toBe("Succeeded");
    expect(t("chat.activity.unknown")).toBe("Unknown");
    flush(() => setLocale("zh-CN"));
    expect(t("attention.title")).toBe("在等你");
    expect(t("attention.count.one")).toBe("1 个审批在等你");
    expect(t("attention.count.many", { count: 2 })).toBe("2 个审批在等你");
    expect(t("attention.empty.title")).toBe("没有在等的事");
    expect(t("chat.newActivity")).toBe("有新活动");
    expect(t("chat.failed")).toBe("已失败");
    expect(t("chat.activity.step", { count: 2 })).toBe("第 2 步");
    expect(t("chat.activity.succeeded")).toBe("成功");
    expect(t("chat.activity.failed")).toBe("失败");
    expect(t("chat.activity.unknown")).toBe("未知");
  });

  it("explains a closed composer with the Bot's name", () => {
    expect(t("chat.composer.busy", { name: "Scout" })).toContain("Scout");
    expect(t("chat.composer.busy", { name: "Scout" })).toContain("working");
    expect(t("chat.composer.waiting", { name: "Scout" })).toContain("approval");
    flush(() => setLocale("zh-CN"));
    expect(t("chat.composer.busy", { name: "Scout" })).toContain("Scout");
    expect(t("chat.composer.waiting", { name: "Scout" })).toContain("审批");
  });

  it("keeps implementation copy out of the client", () => {
    expect(t("auth.fine")).not.toContain("HttpOnly");
    expect(t("auth.password.hint")).not.toContain("byte");
    flush(() => setLocale("zh-CN"));
    expect(t("auth.fine")).not.toContain("HttpOnly");
    expect(t("setup.bot.nameHint")).not.toContain("任务板");
  });

  it("remembers the chosen locale", () => {
    flush(() => setLocale("zh-CN"));
    expect(localStorage.getItem("neha.locale")).toBe("zh-CN");
    flush(() => setLocale("en-US"));
    expect(localStorage.getItem("neha.locale")).toBe("en-US");
  });

  it("toggles the document language", () => {
    flush(() => setLocale("en-US"));
    expect(document.documentElement.lang).toBe("en-US");
    flush(() => toggleLocale());
    expect(locale()).toBe("zh-CN");
    expect(document.documentElement.lang).toBe("zh-CN");
  });
});
