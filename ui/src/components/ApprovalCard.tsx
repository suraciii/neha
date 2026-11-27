import { createSignal, Show } from "solid-js";

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
  // The first use of the User's machine comes first, as the CLI tells it; an
  // ambiguous action has no rule behind it, so it cannot claim one.
  const reason = () =>
    props.approval.reason === "first_use_local"
      ? t("chat.approval.reason.firstUse", { name: props.botName })
      : props.approval.reason === "grant"
        ? t("chat.approval.reason.grant")
        : props.approval.ambiguous
          ? t("chat.approval.reason.ambiguous")
          : t("chat.approval.reason.rule");
  const undo = () =>
    t(
      undoableOf(props.approval) === "no"
        ? "chat.approval.undo.none"
        : "chat.approval.undo.unknown",
    );
  // What an Always allow would cover. null, or an empty string, is the Space
  // saying that no lasting rule can come from this action; only a Space that
  // says nothing at all leaves this page to name what it can see.
  const scope = () => {
    const approval = props.approval;
    if (
      (approval.reason !== "rule" && approval.reason !== "grant") ||
      approval.lasting_scope === null
    )
      return null;
    if (approval.lasting_scope !== undefined) {
      // The Space's own words, spelled as the record spells them: a folder it
      // names can end in a space, and tidying the sentence here would name
      // another folder than the answer covers. An empty scope names none.
      const said = approval.lasting_scope;
      return said ? t("chat.approval.lastingScope", { scope: said }) : null;
    }
    const local = localScope(approval);
    return local ? t("chat.approval.lastingScope", { scope: t(local.key, local.params) }) : null;
  };
  const long = () => isLongSummary(props.approval.summary);
  const folded = () => long() && !expanded();
  // The directory the run works in, when it named one: the action lands there,
  // so the card names it next to the action it belongs to. The Space records
  // the directory it normalized, and a last name that ends in a space is its
  // own directory, so the card shows the recorded spelling exactly.
  const cwd = () => props.approval.cwd || null;

  return (
    <article
      class="msg msg--bot approval"
      aria-live="polite"
      data-approval={props.approval.approval}
    >
      <Avatar name={props.botName} small />
      <div class="msg__body">
        <div class="approval__head">
          <b>{props.botName}</b>
          <span>{t("chat.approval.heading")}</span>
        </div>
        <span class="approval__tool mono">{props.approval.tool}</span>
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
        <Show when={cwd()}>
          {(path) => <p class="approval__cwd">{t("chat.approval.cwd", { path: path() })}</p>}
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
          <Show when={scope()}>{(line) => <p class="approval__scope">{line()}</p>}</Show>
          <div class="approval__actions">
            <button
              class="btn btn--primary"
              type="button"
              disabled={props.pending}
              onClick={() => props.onDecide(props.approval, true, false)}
            >
              {t("chat.approval.allowOnce")}
            </button>
            <Show when={scope()}>
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
