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
    expect(t("chat.aria", { name: "Quill" })).toBe("与Quill的 Chat");
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
