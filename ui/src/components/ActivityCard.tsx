import { createEffect, createSignal, For, Show } from "solid-js";
import { t } from "../i18n";
import type { FileSummary, RunActivity, ToolAction } from "../types";
import Avatar from "./Avatar";
import FileCard from "./FileCard";
import Icon from "./Icon";

interface ActivityCardProps {
  activity: RunActivity;
  botName: string;
  // The Chat's Files, so a produced File can name itself.
  files: Record<string, FileSummary>;
  // The Run failed; the card opens itself, because the reason is why it failed
  // and the User must not have to go looking for it.
  failed: boolean;
}

// How one action ended, in the reader's language.
function statusOf(action: ToolAction): string {
  if (action.status === "running") return t("chat.activity.running");
  if (action.status === "succeeded") return t("chat.activity.succeeded");
  if (action.status === "failed") return t("chat.activity.failed");
  return t("chat.activity.unknown");
}

// The tools the Run used, each named once, in the order it used them.
function toolsOf(actions: ToolAction[]): string {
  const names: string[] = [];
  for (const action of actions) {
    if (!names.includes(action.tool)) names.push(action.tool);
  }
  return names.join(", ");
}

// What the followed Run is doing: one quiet summary the User can open. It never
// shows a tool's arguments; it names the steps, the tools, how each action
// ended, and the Files the Run produced. It starts folded, so the stream stays
// calm while the Run works, and a failed Run opens itself to show its reason.
export default function ActivityCard(props: ActivityCardProps) {
  const [opened, setOpened] = createSignal(false);
  // A failed Run opens itself: its reason is what the User came to read.
  createEffect(
    () => props.failed,
    (failed) => {
      if (failed) setOpened(true);
    },
  );

  const expanded = () => opened();
  const title = () => {
    const label = props.failed ? t("chat.failed") : t("chat.working");
    const step = props.activity.step;
    return step > 0 ? `${label} · ${t("chat.activity.step", { count: step })}` : label;
  };
  // Why the Run failed: its own reason, else the last action that failed.
  const reason = () => {
    if (props.activity.error) return props.activity.error;
    const failed = props.activity.actions.filter((action) => action.status === "failed");
    return failed.length > 0 ? (failed[failed.length - 1]?.error ?? null) : null;
  };

  return (
    <article class={`activity activity--${props.failed ? "failed" : "working"}`} aria-live="polite">
      <button
        class="activity__head"
        type="button"
        aria-expanded={expanded() ? "true" : "false"}
        onClick={() => setOpened(!opened())}
      >
        <Avatar name={props.botName} small kind="bot" />
        <Show when={!props.failed}>
          <span class="work-bars" aria-hidden="true">
            <i />
            <i />
            <i />
          </span>
        </Show>
        <Show when={props.failed}>
          <Icon name="alert" size="sm" />
        </Show>
        <span class="activity__title">{title()}</span>
        <span class="activity__tools">{toolsOf(props.activity.actions)}</span>
        <Icon name="chevron-down" size="sm" class="activity__chev" />
      </button>
      <Show when={expanded()}>
        <ol class="activity__steps">
          <For each={props.activity.actions}>
            {(action) => (
              <li class="activity__step">
                <span
                  class={`activity__mark activity__mark--${action.status}`}
                  aria-hidden="true"
                />
                <span class="activity__step-no">
                  {t("chat.activity.step", { count: action.step })}
                </span>
                <span class="activity__tool">{action.tool}</span>
                <span class="activity__status">{statusOf(action)}</span>
              </li>
            )}
          </For>
        </ol>
        <Show when={props.activity.files.length > 0}>
          <div class="activity__files">
            <For each={props.activity.files}>
              {(id) => <FileCard id={id} file={props.files[id] ?? null} />}
            </For>
          </div>
        </Show>
        <Show when={reason()}>
          {(why) => <p class="activity__reason">{t("chat.activity.reason", { reason: why() })}</p>}
        </Show>
      </Show>
    </article>
  );
}
