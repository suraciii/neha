import { Show } from "solid-js";

import { t } from "../i18n";
import type { Account } from "../types";
import Avatar from "./Avatar";
import Brand from "./Brand";
import Icon from "./Icon";

// What the rail selects: the User's Chats, the approvals that wait, the
// management surfaces, and the libraries the Space holds.
export type RailView = "chats" | "attention" | "bots" | "settings" | "integrations" | "skills";

interface RailProps {
  account: Account;
  view: RailView;
  // How many approvals wait for the User right now.
  attentionCount: number;
  onSelect: (view: RailView) => void;
}

export default function Rail(props: RailProps) {
  // How many approvals wait, in the User's words: one, many, or none.
  const countLabel = (): string | null => {
    const count = props.attentionCount;
    if (count === 0) return null;
    return t(count === 1 ? "attention.count.one" : "attention.count.many", { count });
  };
  const attentionLabel = () => {
    const count = countLabel();
    return count ? `${t("attention.title")}: ${count}` : t("attention.title");
  };
  // An entry that names itself in the User's language and marks the one on
  // screen. The library views carry no count.
  const item = (view: RailView, label: string, icon: string) => (
    <button
      class={`rail__item${props.view === view ? " is-active" : ""}`}
      type="button"
      aria-label={label}
      aria-current={props.view === view ? "page" : undefined}
      title={label}
      onClick={() => props.onSelect(view)}
    >
      <Icon name={icon} size="lg" />
    </button>
  );
  return (
    <aside class="rail" aria-label={t("rail.primary")}>
      <Brand compact />
      <nav class="rail__nav">
        {item("chats", t("rail.chats"), "message")}
        <button
          class={`rail__item${props.view === "attention" ? " is-active" : ""}`}
          type="button"
          aria-label={attentionLabel()}
          aria-current={props.view === "attention" ? "page" : undefined}
          title={attentionLabel()}
          onClick={() => props.onSelect("attention")}
        >
          <Icon name="bell" size="lg" />
          <Show when={countLabel()}>
            <span class="rail__count" role="status">
              {props.attentionCount}
            </span>
          </Show>
        </button>
        {item("bots", t("rail.bots"), "cpu")}
        {item("settings", t("rail.settings"), "settings")}
        {item("integrations", t("rail.integrations"), "server")}
        {item("skills", t("rail.skills"), "book")}
      </nav>
      <div class="rail__spacer" />
      <div class="rail__account" title={props.account.display_name}>
        <Avatar name={props.account.display_name} kind="user" small />
        <span class="rail__admin">{props.account.is_admin ? t("rail.admin") : t("rail.user")}</span>
      </div>
    </aside>
  );
}
