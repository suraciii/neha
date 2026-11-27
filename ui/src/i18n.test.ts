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
