// What a Computer is called where the User chooses between several of them.
import { t } from "./i18n";
import type { Computer } from "./types";

// A Computer's workspace is a path on a Computer the User paired, and a paired
// record does not always name one: the identifier the Space gave it is what
// tells one Computer from another. A workspace it does name, and whether it is
// online, come after the identifier.
export function computerLabel(known: Computer): string {
  const kind = known.kind === "local" ? t("chat.localComputer") : t("chat.hostedComputer");
  const named = known.workspace.trim();
  const parts = [`${kind} ${known.id.slice(0, 8)}`];
  if (named !== "") parts.push(named);
  if (!known.online) parts.push(t("chat.offline"));
  return parts.join(" · ");
}
