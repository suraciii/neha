import { createEffect, createSignal, For, Show } from "solid-js";
import { t } from "../i18n";
import type { FileSummary, PlanState, RunActivity, ToolAction } from "../types";
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

// How one plan entry stands, in the reader's language.
function planStateOf(state: PlanState): string {
  if (state === "done") return t("chat.activity.plan.done");
  if (state === "doing") return t("chat.activity.plan.doing");
  return t("chat.activity.plan.pending");
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
// shows a tool's arguments; it names the state, the plan and its progress or
// the steps and tools, how each action ended, and the Files the Run produced. It
// starts folded, so the stream stays calm while the Run works, and a failed Run
// opens itself to show its reason.
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
  // The plan the Run holds now. Its entries are the whole list, oldest first,
  // so the count and the bar follow the record's newest write.
  const plan = () => props.activity.plan;
  const done = () => plan().filter((entry) => entry.state === "done").length;
  const total = () => plan().length;
  // The share of the bar to fill. A plan with no entry has nothing to show, so
  // the bar only renders for one with entries.
  const percent = () => (total() === 0 ? 0 : Math.round((done() / total()) * 100));
  // A Run with a plan states its progress instead of its step: the plan is the
  // better answer to how far the work has come.
  const title = () => {
    const label = props.failed ? t("chat.failed") : t("chat.working");
    if (total() > 0) return label;
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
        <Show
          when={total() > 0}
          fallback={<span class="activity__tools">{toolsOf(props.activity.actions)}</span>}
        >
          <span
            class="activity__progress"
            role="progressbar"
            aria-label={t("chat.activity.plan")}
            aria-valuemin={0}
            aria-valuemax={total()}
            aria-valuenow={done()}
            aria-valuetext={t("chat.activity.progress", { done: done(), total: total() })}
          >
            <span class="activity__progress-fill" style={{ width: `${percent()}%` }} />
          </span>
          <span class="activity__count">
            {t("chat.activity.plan.count", { done: done(), total: total() })}
          </span>
        </Show>
        <Icon name="chevron-down" size="sm" class="activity__chev" />
      </button>
      <Show when={expanded()}>
        <Show when={total() > 0}>
          <ol class="activity__plan" aria-label={t("chat.activity.plan")}>
            <For each={plan()}>
              {(entry) => (
                <li class={`activity__plan-entry activity__plan-entry--${entry.state}`}>
                  <span class="activity__plan-mark" aria-hidden="true">
                    <Show when={entry.state === "done"} fallback={<i class="activity__plan-dot" />}>
                      <Icon name="check" size="sm" />
                    </Show>
                  </span>
                  <span class="activity__plan-title">{entry.title}</span>
                  <span class="activity__plan-state">{planStateOf(entry.state)}</span>
                </li>
              )}
            </For>
          </ol>
          <Show when={props.activity.actions.length > 0}>
            <p class="activity__label">{t("chat.activity.actions")}</p>
          </Show>
        </Show>
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
                <Show when={action.status !== "failed" && action.error}>
                  {(note) => <span class="activity__note">{note()}</span>}
                </Show>
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
