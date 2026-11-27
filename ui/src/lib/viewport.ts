// Keeps the app as tall as the part of the window a phone actually shows.
//
// A phone keyboard shrinks `visualViewport` without changing the layout
// viewport. The CSS uses `--viewport-height` when it is set, so the composer
// stays above the keyboard instead of behind it.
export function watchViewportHeight(target: Window = window): () => void {
  const root = target.document.documentElement;
  const viewport = target.visualViewport;
  const apply = () => {
    const height = viewport?.height ?? target.innerHeight;
    root.style.setProperty("--viewport-height", `${Math.round(height)}px`);
  };

  apply();
  target.addEventListener("resize", apply);
  viewport?.addEventListener("resize", apply);
  viewport?.addEventListener("scroll", apply);
  return () => {
    target.removeEventListener("resize", apply);
    viewport?.removeEventListener("resize", apply);
    viewport?.removeEventListener("scroll", apply);
    root.style.removeProperty("--viewport-height");
  };
}
