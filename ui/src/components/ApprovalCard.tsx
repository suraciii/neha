import { Show } from "solid-js";

import { t } from "../i18n";
import type { Approval } from "../types";
import Avatar from "./Avatar";
import Icon from "./Icon";

interface ApprovalCardProps {
  approval: Approval;
  botName: string;
  // The answer for this approval is on its way to the Space.
  pending: boolean;
  onDecide: (approval: Approval, allow: boolean, lasting: boolean) => void;
}

// One action a Bot waits to run until the User answers.
export default function ApprovalCard(props: ApprovalCardProps) {
  const allowed = () => props.approval.decided === true;
  const denied = () => props.approval.decided === false;
  const reason = () =>
    props.approval.reason === "rule"
      ? t("chat.approval.reason.rule")
      : t("chat.approval.reason.firstUse", { name: props.botName });

  return (
    <article class="msg msg--bot approval" aria-live="polite">
      <Avatar name={props.botName} small />
      <div class="msg__body">
        <div class="msg__meta">
          <b>{props.botName}</b>
          <span>{t("chat.approval.asks", { tool: props.approval.tool })}</span>
        </div>
        <code class="approval__summary">{props.approval.summary}</code>
        <p class="approval__reason">{reason()}</p>
        <Show
          when={!allowed() && !denied()}
          fallback={
            <p
              class={`approval__verdict ${denied() ? "approval__verdict--denied" : ""}`}
              role="status"
            >
              <Icon name={allowed() ? "check" : "x"} size="sm" />
              {allowed() ? t("chat.approval.allowed") : t("chat.approval.denied")}
            </p>
          }
        >
          <div class="approval__actions">
            <button
              class="btn btn--primary"
              type="button"
              disabled={props.pending}
              onClick={() => props.onDecide(props.approval, true, false)}
            >
              {t("chat.approval.allow")}
            </button>
            <Show when={props.approval.reason === "rule"}>
              <button
                class="btn btn--quiet"
                type="button"
                disabled={props.pending}
                onClick={() => props.onDecide(props.approval, true, true)}
              >
                {t("chat.approval.alwaysAllow")}
              </button>
            </Show>
            <button
              class="btn btn--danger"
              type="button"
              disabled={props.pending}
              onClick={() => props.onDecide(props.approval, false, false)}
            >
              {t("chat.approval.deny")}
            </button>
          </div>
        </Show>
      </div>
    </article>
  );
}
