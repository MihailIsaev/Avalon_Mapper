import type { OcrLine } from "../types";

export interface ParsedCurrentLocation {
  locationName: string | null;
  confidence: number;
  ignoredLines: string[];
  candidates: string[];
  reason: string;
}

export interface ParsedPortalTooltip {
  destinationName: string | null;
  slotsUsed: number | null;
  slotsTotal: number | null;
  expiresInSeconds: number | null;
  confidence: number;
  ignoredLines: string[];
  candidates: string[];
  reason: string;
}

export function parseCurrentLocationOcr(rawText: string, ocrLines: OcrLine[] = []): ParsedCurrentLocation {
  const ignoredLines: string[] = [];
  const candidates = lines(rawText, ocrLines)
    .map((line) => {
      const cleaned = line
        .split(/\s+/)
        .map(cleanNameToken)
        .filter((token) => token && !isRomanTier(token) && !/^\d+$/.test(token) && !isTimer(token))
        .join(" ")
        .trim();
      if (!/[A-Za-z]{3,}/.test(cleaned)) ignoredLines.push(line);
      return cleaned;
    })
    .filter((line) => /[A-Za-z]{3,}/.test(line));

  const unique = [...new Set(candidates)].sort((a, b) => b.length - a.length);
  return {
    locationName: unique[0] ?? null,
    confidence: unique[0] ? 0.7 : 0,
    ignoredLines,
    candidates: unique,
    reason: unique[0] ? "Removed plaque tier, numbers, timers, and icon fragments." : "No name-like line found.",
  };
}

export function parsePortalTooltipOcr(rawText: string, ocrLines: OcrLine[] = []): ParsedPortalTooltip {
  const ignoredLines: string[] = [];
  const candidates: string[] = [];
  let slotsUsed: number | null = null;
  let slotsTotal: number | null = null;
  let expiresInSeconds: number | null = null;
  let fallbackTimer: number | null = null;

  for (const line of lines(rawText, ocrLines)) {
    const slots = line.match(/(\d+)\s*\/\s*(\d+)/);
    if (slots && slotsUsed === null) {
      slotsUsed = Number(slots[1]);
      slotsTotal = Number(slots[2]);
    }
    expiresInSeconds ??= parseDuration(line);
    fallbackTimer ??= parseColonTimer(line);

    if (isPortalTitle(line) || slots || parseDuration(line) !== null || isTimer(line) || cleanNameToken(line).length <= 1) {
      ignoredLines.push(line);
      continue;
    }
    const cleaned = line
      .split(/\s+/)
      .map(cleanNameToken)
      .filter(Boolean)
      .join(" ");
    if (/[A-Za-zА-Яа-я]{3,}/.test(cleaned)) candidates.push(cleaned);
  }

  const unique = [...new Set(candidates)].sort((a, b) => Number(b.includes("-")) - Number(a.includes("-")) || b.length - a.length);
  return {
    destinationName: unique[0] ?? null,
    slotsUsed,
    slotsTotal,
    expiresInSeconds: expiresInSeconds ?? fallbackTimer,
    confidence: unique[0] ? (unique[0].includes("-") ? 0.82 : 0.65) : 0,
    ignoredLines,
    candidates: unique,
    reason: unique[0] ? "Ignored title, slot, timer, duration, and icon-like lines." : "No destination-like line found.",
  };
}

function lines(rawText: string, ocrLines: OcrLine[]): string[] {
  const structured = ocrLines.map((line) => line.text);
  return [...structured, ...rawText.split(/\r?\n/)].map((line) => line.trim()).filter(Boolean);
}

function cleanNameToken(token: string): string {
  return token.replace(/[^A-Za-zА-Яа-я'-]/g, "").replace(/^-+|-+$/g, "");
}

function isRomanTier(token: string): boolean {
  return /^(I|II|III|IV|V|VI|VII|VIII)$/i.test(token);
}

function isTimer(value: string): boolean {
  return /\b\d{1,2}:\d{2}\b/.test(value);
}

function isPortalTitle(value: string): boolean {
  return /(Путь Авалона|Авалона|Avalon|Roads?|Portal|Портал)/i.test(value);
}

function parseDuration(value: string): number | null {
  const lower = value.toLowerCase();
  let total = 0;
  let found = false;
  const re = /(\d+)\s*(h|hr|hrs|hour|hours|ч|m|min|mins|minute|minutes|м|s|sec|secs|second|seconds|с)/gi;
  for (const match of lower.matchAll(re)) {
    const amount = Number(match[1]);
    const unit = match[2];
    if (/^(h|hr|hrs|hour|hours|ч)$/.test(unit)) total += amount * 3600;
    else if (/^(m|min|mins|minute|minutes|м)$/.test(unit)) total += amount * 60;
    else total += amount;
    found = true;
  }
  return found ? total : null;
}

function parseColonTimer(value: string): number | null {
  const match = value.match(/\b(\d{1,2}):(\d{2})\b/);
  return match ? Number(match[1]) * 60 + Number(match[2]) : null;
}
