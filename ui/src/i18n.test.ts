import { afterEach, describe, expect, it } from "vitest";
import { locale, setLocale, t, toggleLocale } from "./i18n";

afterEach(() => {
  setLocale("en-US");
  localStorage.clear();
});

describe("i18n", () => {
  it("translates product copy", () => {
    expect(t("auth.headline")).toBe("Put work in the hands of a team you can see.");
    setLocale("zh-CN");
    expect(t("auth.headline")).toBe("把工作交给一支你看得见的团队。");
  });

  it("interpolates names", () => {
    expect(t("chat.aria", { name: "Quill" })).toBe("Chat with Quill");
    setLocale("zh-CN");
    expect(t("chat.aria", { name: "Quill" })).toBe("与Quill的 Chat");
  });

  it("remembers the chosen locale", () => {
    setLocale("zh-CN");
    expect(localStorage.getItem("neha.locale")).toBe("zh-CN");
    setLocale("en-US");
    expect(localStorage.getItem("neha.locale")).toBe("en-US");
  });

  it("toggles the document language", () => {
    setLocale("en-US");
    expect(document.documentElement.lang).toBe("en-US");
    toggleLocale();
    expect(locale()).toBe("zh-CN");
    expect(document.documentElement.lang).toBe("zh-CN");
  });
});
