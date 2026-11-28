interface AvatarProps {
  name: string;
  kind?: "user" | "bot";
  small?: boolean;
}

export default function Avatar(props: AvatarProps) {
  const initials = () =>
    props.name
      .trim()
      .split(/\s+/)
      .slice(0, 2)
      .map((part) => part[0]?.toUpperCase() ?? "")
      .join("");

  return (
    <span
      class={`avatar avatar--${props.kind ?? "bot"} ${props.small ? "avatar--sm" : ""}`}
      aria-hidden="true"
    >
      {initials()}
    </span>
  );
}
