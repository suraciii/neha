import { Show } from "solid-js";

import { t } from "../i18n";
import { formatBytes } from "../lib/files";
import type { FileSummary } from "../types";
import Icon from "./Icon";

interface FileCardProps {
  id: string;
  file: FileSummary | null;
}

// A File a Message carries, linking to its download.
export default function FileCard(props: FileCardProps) {
  const name = () => props.file?.name ?? props.id;
  return (
    <a
      class="file-card"
      href={`/api/files/${props.id}`}
      download={name()}
      aria-label={t("chat.file.download", { name: name() })}
    >
      <span class="file-card__icon">
        <Icon name="file" size="sm" />
      </span>
      <span class="file-card__text">
        <span class="file-card__name">{name()}</span>
        <Show when={props.file}>
          {(file) => <span class="file-card__size">{formatBytes(file().size)}</span>}
        </Show>
      </span>
      <Icon name="download" size="sm" class="file-card__action" />
    </a>
  );
}
