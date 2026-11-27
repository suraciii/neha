interface IconProps {
  name: string;
  size?: "sm" | "md" | "lg";
  class?: string;
}

export default function Icon(props: IconProps) {
  return (
    <svg
      class={`icon ${props.size ? `icon--${props.size}` : ""} ${props.class ?? ""}`}
      aria-hidden="true"
    >
      <use href={`/sprite.svg#i-${props.name}`} />
    </svg>
  );
}
