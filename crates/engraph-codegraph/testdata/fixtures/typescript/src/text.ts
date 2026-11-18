// String helpers used from models and ops.

export function titleCase(s: string): string {
  return s.split(/\s+/).filter(Boolean).map(capitalize).join(" ");
}

export function capitalize(word: string): string {
  if (word.length === 0) {
    return "";
  }
  return word[0].toUpperCase() + word.slice(1);
}

export function initials(name: string): string {
  return name
    .split(/\s+/)
    .filter(Boolean)
    .map((word) => word[0])
    .join("");
}

export function pluralize(word: string, n: number): string {
  if (n === 1) {
    return `1 ${word}`;
  }
  return `${n} ${word}s`;
}

export function truncate(s: string, max: number): string {
  return s.slice(0, max);
}

export function slug(s: string): string {
  return s.toLowerCase().replace(/ /g, "-");
}
