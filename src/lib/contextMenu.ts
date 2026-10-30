/**
 * WebView2 shows its own back/forward/reload/"save as" menu on right click,
 * which reads as a browser leaking into a desktop launcher. Text fields keep
 * the native menu — that is where paste lives; anything else gets nothing.
 */
export function suppressNativeContextMenu(): () => void {
  const handler = (event: MouseEvent) => {
    const target = event.target as HTMLElement | null;
    if (target?.closest("input, textarea, [contenteditable='true'], [contenteditable='']")) {
      return;
    }
    event.preventDefault();
  };
  window.addEventListener("contextmenu", handler);
  return () => window.removeEventListener("contextmenu", handler);
}
