import { t } from "../i18n";
import type { Account } from "../types";
import Avatar from "./Avatar";
import Brand from "./Brand";
import Icon from "./Icon";

interface RailProps {
  account: Account;
}

export default function Rail(props: RailProps) {
  return (
    <aside class="rail" aria-label={t("rail.primary")}>
      <Brand compact />
      <nav class="rail__nav">
        <button class="rail__item is-active" type="button" aria-label={t("rail.chats")} title={t("rail.chats")}>
          <Icon name="message" size="lg" />
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
