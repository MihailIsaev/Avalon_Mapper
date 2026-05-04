const COMMON_WORDS = [
  "avalonian portal",
  "roads of avalon",
  "portal",
  "enter",
  "exit",
];

export function normalizeLocationName(rawText: string): string {
  let cleaned = rawText
    .replace(/[^a-zA-Z0-9\s'-]/g, " ")
    .replace(/\s+/g, " ")
    .trim();

  for (const word of COMMON_WORDS) {
    cleaned = cleaned.replace(new RegExp(escapeRegExp(word), "gi"), " ");
  }

  return cleaned.replace(/\s+/g, " ").trim().toLowerCase();
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
