import { Show } from "solid-js";

import { t } from "../i18n";

// The longest kind the placeholder names. A Card of a kind this client does not
// know still shows the kind when it is a short word the reader can use.
const MAX_KIND_LENGTH = 32;

// Control, format, and bidi characters: a kind that carries one could reorder
// or hide the words around it, so the placeholder does not show it.
const UNPRINTABLE = /[\u0000-\u001F\u007F-\u009F\u200B-\u200F\u202A-\u202E\u2060-\u206F\uFEFF]/;

// The kind a placeholder may name, trimmed; null for one it must not show.
function readableKind(kind: string): string | null {
  const trimmed = kind.trim();
  if (trimmed === "" || trimmed.length > MAX_KIND_LENGTH) return null;
  if (UNPRINTABLE.test(trimmed)) return null;
  return trimmed;
}

// A Card of a kind the client does not know. It says the Card cannot be shown
// and names the kind as text, never as markup.
export default function CardPlaceholder(props: { kind: string }) {
  const kind = () => readableKind(props.kind);
  return (
    <p class="card-placeholder">
      <span>{t("chat.card.placeholder")}</span>
      <Show when={kind()}>{(shown) => <span class="card-placeholder__kind">{shown()}</span>}</Show>
    </p>
  );
}
