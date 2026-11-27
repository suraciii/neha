import { Show } from "solid-js";

import { t } from "../i18n";
import type { Account } from "../types";
import Avatar from "./Avatar";
import Brand from "./Brand";
import Icon from "./Icon";

// What the rail selects: the User's Chats, or the approvals that wait.
export type RailView = "chats" | "attention";

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
  return (
    <aside class="rail" aria-label={t("rail.primary")}>
      <Brand compact />
      <nav class="rail__nav">
        <button
          class={`rail__item${props.view === "chats" ? " is-active" : ""}`}
          type="button"
          aria-label={t("rail.chats")}
          aria-current={props.view === "chats" ? "page" : undefined}
          title={t("rail.chats")}
          onClick={() => props.onSelect("chats")}
        >
          <Icon name="message" size="lg" />
        </button>
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
      </nav>
      <div class="rail__spacer" />
      <div class="rail__account" title={props.account.display_name}>
        <Avatar name={props.account.display_name} kind="user" small />
        <span class="rail__admin">{props.account.is_admin ? t("rail.admin") : t("rail.user")}</span>
      </div>
    </aside>
  );
}
