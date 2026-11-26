export function timeOfDay(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", hour12: false });
}

export function dayLabel(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  const today = new Date();
  const sameDay =
    date.getFullYear() === today.getFullYear() &&
    date.getMonth() === today.getMonth() &&
    date.getDate() === today.getDate();
  const dateText = date.toLocaleDateString("en-US", { month: "short", day: "numeric" });
  if (sameDay) return `Today · ${dateText}`;
  return date.toLocaleDateString("en-US", { weekday: "short", month: "short", day: "numeric" });
}

export function firstName(name: string): string {
  return name.trim().split(/\s+/)[0] ?? name;
}
