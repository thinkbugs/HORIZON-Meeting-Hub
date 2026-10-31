/** Whether the event came from a control that owns the keyboard.
 *
 *  Global shortcuts (Escape, arrow keys) must not fire while one of these has
 *  focus: the media stage's Escape closes the whole dialog, which would tear
 *  the stage down under someone who only wanted to clear a URL field.
 */
export function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return Boolean(target.closest("input, textarea, select, [contenteditable='true']"));
}
