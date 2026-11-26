import { Show, createSignal } from "solid-js";

import { t } from "../i18n";
import { escapeControls, isLongSummary, localScope, undoableOf } from "../lib/approvals";
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
  const [expanded, setExpanded] = createSignal(false);
  const allowed = () => props.approval.decided === true;
  const denied = () => props.approval.decided === false;
  const reason = () =>
    props.approval.reason === "rule"
      ? t("chat.approval.reason.rule")
      : t("chat.approval.reason.firstUse", { name: props.botName });
  const undo = () =>
    t(undoableOf(props.approval) === "no" ? "chat.approval.undo.none" : "chat.approval.undo.unknown");
  // What an Always allow would cover: the Space's words, else this page's guess.
  const scope = () => {
    if (props.approval.reason !== "rule") return null;
    const local = localScope(props.approval);
    const scope = props.approval.lasting_scope ?? (local ? t(local.key, local.params) : null);
    return scope ? t("chat.approval.lastingScope", { scope }) : null;
  };
  const long = () => isLongSummary(props.approval.summary);
  const folded = () => long() && !expanded();

  return (
    <article class="msg msg--bot approval" aria-live="polite">
      <Avatar name={props.botName} small />
      <div class="msg__body">
        <div class="msg__meta">
          <b>{props.botName}</b>
          <span>{t("chat.approval.asks", { tool: props.approval.tool })}</span>
        </div>
        <code class={`approval__summary ${folded() ? "approval__summary--folded" : ""}`}>
          {escapeControls(props.approval.summary)}
        </code>
        <Show when={long()}>
          <button
            class="link-btn approval__expand"
            type="button"
            aria-expanded={expanded() ? "true" : "false"}
            onClick={() => setExpanded(!expanded())}
          >
            {expanded() ? t("chat.approval.showLess") : t("chat.approval.showFull")}
          </button>
        </Show>
        <p class="approval__reason">{reason()}</p>
        <p class="approval__undo">{undo()}</p>
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
          <Show when={scope()}>
            {(line) => (
              <p class="approval__scope">{line()}</p>
            )}
          </Show>
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
