// The platform the app runs on, for the few places where macOS differs (Cmd key, title bar).
export const isMac = typeof navigator !== 'undefined' && /Mac/.test(navigator.userAgent);

/** The modifier of the app's shortcuts: Cmd on macOS, Ctrl elsewhere (Ctrl also works on macOS). */
export function modKey(e: KeyboardEvent): boolean {
  return e.ctrlKey || (isMac && e.metaKey);
}
