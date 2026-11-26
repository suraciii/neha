import type { FileSummary } from "../types";

// The largest File a Space accepts; the Space refuses anything larger.
export const MAX_FILE_LABEL = "32 MiB";

export const MAX_FILE_BYTES = 32 * 1024 * 1024;

// The media type to send for a File; the browser leaves rare types empty.
export function mediaTypeFor(file: File): string {
  return file.type || "application/octet-stream";
}

// A size for a reader: 20 B, 1.5 KB, 32 MB.
export function formatBytes(size: number): string {
  if (size < 1024) return `${size} B`;
  const units = ["KB", "MB", "GB"];
  let value = size / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

// The Chat's Files, by identifier.
export function indexFiles(files: FileSummary[]): Record<string, FileSummary> {
  return Object.fromEntries(files.map((file) => [file.id, file]));
}
