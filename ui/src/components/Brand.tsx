interface BrandProps {
  compact?: boolean;
}

export default function Brand(props: BrandProps) {
  return (
    <span class={`brand ${props.compact ? "brand--compact" : ""}`} role="img" aria-label="Neha">
      <span class="brand__mark" aria-hidden="true">
        <svg viewBox="0 0 32 32" aria-hidden="true">
          <path d="M16 4.5 27 16 16 27.5 5 16Z" />
          <path d="M16 11.4 20.6 16 16 20.6 11.4 16Z" />
        </svg>
      </span>
      {!props.compact && <span class="brand__word">Neha</span>}
    </span>
  );
}
