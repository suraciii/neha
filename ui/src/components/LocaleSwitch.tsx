import { locale, t, toggleLocale } from "../i18n";
import Icon from "./Icon";

export default function LocaleSwitch(props: { class?: string }) {
  const target = () => (locale() === "en-US" ? "zh-CN" : "en-US");
  const visible = () => (locale() === "en-US" ? t("locale.name.zh") : t("locale.name.en"));
  const action = () =>
    locale() === "en-US" ? t("locale.switch.to.zh") : t("locale.switch.to.en");
  const label = () => `${visible()} — ${action()}`;
  return (
    <button
      class={`locale-switch ${props.class ?? ""}`}
      type="button"
      onClick={toggleLocale}
      aria-label={label()}
      title={label()}
    >
      <Icon name="globe" size="sm" />
      <span lang={locale() === "en-US" ? "zh-CN" : "en-US"}>{visible()}</span>
    </button>
  );
}
