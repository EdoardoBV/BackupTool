const oneDecimal = new Intl.NumberFormat("it-IT", {
  minimumFractionDigits: 1,
  maximumFractionDigits: 1,
});

/** Decimal units, like the capacity printed on cards and drives. */
export function bytes(n: number): string {
  if (n >= 1e9) return `${oneDecimal.format(n / 1e9)} GB`;
  if (n >= 1e6) return `${oneDecimal.format(n / 1e6)} MB`;
  if (n >= 1e3) return `${Math.round(n / 1e3)} kB`;
  return `${n} B`;
}

export function remaining(seconds: number): string {
  if (!Number.isFinite(seconds)) return "";
  if (seconds < 60) return "meno di un minuto";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `circa ${minutes} min`;
  return `circa ${Math.floor(minutes / 60)} h ${String(minutes % 60).padStart(2, "0")} min`;
}

/** Last path segment, or the drive itself for a volume root like `E:\`. */
export function leaf(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

export function files(n: number): string {
  return n === 1 ? "1 file" : `${n} file`;
}

/**
 * "i 131 file", "gli 8 file": the article follows how the number sounds
 * (otto, undici, ottanta… start with a vowel). One file reads "i file (1)"
 * so the plural sentences it goes into stay correct.
 */
export function theFiles(n: number): string {
  if (n === 1) return "i file (1)";
  const digits = String(n);
  const leading = Number(digits.slice(0, ((digits.length - 1) % 3) + 1));
  const vowel = leading === 8 || leading === 11 || (leading >= 80 && leading <= 89) || (leading >= 800 && leading <= 899);
  return `${vowel ? "gli" : "i"} ${n} file`;
}

/** "T7", "T7 e Backup", "T7, Backup e Shuttle". */
export function list(names: string[]): string {
  if (names.length <= 1) return names.join("");
  return `${names.slice(0, -1).join(", ")} e ${names[names.length - 1]}`;
}

/**
 * "NIKON Z6_3", not "NIKON CORPORATION NIKON Z6_3": cameras often repeat
 * the maker's name in the model.
 */
export function cameraName(camera: { make: string | null; model: string }): string {
  const brand = camera.make?.trim().split(/\s+/)[0] ?? "";
  const repeats = brand !== "" && camera.model.toLowerCase().startsWith(brand.toLowerCase());
  return repeats || !camera.make ? camera.model : `${camera.make} ${camera.model}`;
}
