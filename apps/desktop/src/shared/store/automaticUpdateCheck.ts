export const AUTO_UPDATE_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1_000;

/** Failed automatic checks consume the interval too; manual checks bypass this wrapper. */
export function createAutomaticUpdateCheck(
  check: () => Promise<string | null>,
  now: () => number = Date.now,
): () => Promise<string | null> {
  let lastAttempt: number | null = null;
  return async () => {
    const time = now();
    if (lastAttempt !== null && time - lastAttempt < AUTO_UPDATE_CHECK_INTERVAL_MS) return null;
    lastAttempt = time;
    return check();
  };
}
