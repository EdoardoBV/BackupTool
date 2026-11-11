import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

export type Volume = {
  path: string;
  name: string;
  file_system: string;
  removable: boolean;
  /** Holds the folders a camera writes. */
  card: boolean;
  total: number;
  available: number;
};

export type DestState = {
  path: string;
  /** False when a remembered destination is not plugged in. */
  online: boolean;
  missing: number;
  different: number;
  present: number;
  missing_bytes: number;
};

export type Inspection = { files: number; bytes: number; dests: DestState[] };

export type Camera = { make: string | null; model: string; serial: string | null };

export type CardInfo = {
  video: number;
  photo: number;
  audio: number;
  other: number;
  /** Seconds since the Unix epoch. */
  shot_at: number | null;
  camera: Camera | null;
  structured: boolean;
};

export type DateFormat = "day_month_year" | "year_month_day";

export type ShootLayout = {
  client: string;
  shoot: string;
  /** YYYY-MM-DD */
  date: string;
  date_format: DateFormat;
  camera: string;
  reel: string;
  keep_card_folders: boolean;
};

export type FolderLine = { path: string; exists: boolean; files: number };
export type ShootPreview = { folders: FolderLine[]; conflicts: number };
export type Client = { name: string; years: number[] };

export type HashAlgo = "xxh64" | "xxh3" | "xxh128" | "md5" | "sha1" | "sha256" | "c4";
export type VerifyMode = "none" | "destination" | "source_and_destination";

export type JobSpec = {
  source: string;
  destinations: string[];
  options: { algo: HashAlgo; verify: VerifyMode };
};

export type FileResult = {
  rel_path: string;
  size: number;
  hash: string | null;
  dests: { status: "copied" | "already_present" | "failed"; verified: boolean; error: string | null }[];
  error: string | null;
};

export type JobEvent =
  | { type: "file_started"; index: number; rel_path: string; size: number }
  | {
      type: "progress";
      index: number;
      phase: "copying" | "verifying";
      bytes_done: number;
      bytes_total: number;
      work_done: number;
      work_total: number;
    }
  | { type: "file_finished"; index: number; result: FileResult };

export type JobReport = {
  files: FileResult[];
  total_files: number;
  total_bytes: number;
  failed_files: number;
  cancelled: boolean;
  safe_to_format: boolean;
  duration_ms: number;
};

/** What to leave next to the footage once a job is done. */
export type Extras = { mhl: boolean; report: boolean; notify: boolean };

export type Generation = {
  manifest: string;
  number: number;
  original: number;
  verified: number;
  failed: string[];
};

/** What was written on one destination after the copy. */
export type DestExtras = {
  destination: string;
  mhl: Generation | null;
  mhl_error: string | null;
  report: string | null;
  report_error: string | null;
};

export type Finished = { report: JobReport; extras: DestExtras[] };

/** A backup with an ASC MHL history, found under a folder. */
export type HistoryRow = {
  root: string;
  generations: number;
  files: number;
  bytes: number;
  /** Why it cannot be checked, when it cannot. */
  error: string | null;
};

export type VerifyReport = {
  root: string;
  files: number;
  verified: number;
  mismatched: { path: string; format: string; expected: string; actual: string }[];
  missing: string[];
  untracked: string[];
  unsupported: string[];
  cancelled: boolean;
  recorded: Generation | null;
  duration_ms: number;
};

/** How checking one backup ended. */
export type Checked = { root: string; report: VerifyReport | null; error: string | null };

export type VerifyProgress = {
  index: number;
  total: number;
  path: string;
  bytes_done: number;
  bytes_total: number;
};

export type VerifyHandlers = {
  onProgress: (progress: VerifyProgress) => void;
  onFinished: (results: Checked[]) => void;
};

export type JobHandlers = {
  onEvent: (event: JobEvent) => void;
  onFinished: (finished: Finished) => void;
  onFailed: (message: string) => void;
};

export interface Api {
  listVolumes(): Promise<Volume[]>;
  analyzeCard(source: string): Promise<CardInfo>;
  /** With a layout, files are looked for where the shoot puts them. */
  inspect(source: string, destinations: string[], layout: ShootLayout | null): Promise<Inspection>;
  listClients(destination: string): Promise<Client[]>;
  previewShoot(source: string, destination: string, layout: ShootLayout): Promise<ShootPreview>;
  /** Creates the folders; resolves to the shoot folder relative to a destination. */
  createShoot(source: string, destinations: string[], layout: ShootLayout): Promise<string>;
  cameraNames(): Promise<Record<string, string>>;
  rememberCamera(key: string, name: string): Promise<void>;
  startJob(spec: JobSpec, layout: ShootLayout | null, extras: Extras): Promise<void>;
  /** Opens a file with the system's default application. */
  openPath(path: string): Promise<void>;
  cancelJob(): Promise<void>;
  pickFolder(title: string): Promise<string | null>;
  /** Returns a function that stops listening. */
  subscribe(handlers: JobHandlers): () => void;
  /** The listeners below also return a function that stops listening. */
  onVolumes(listener: (volumes: Volume[]) => void): () => void;
  onCardInserted(listener: (path: string) => void): () => void;
  onVolumeRemoved(listener: (path: string) => void): () => void;
  /** Tells every window (this one included) that a shared value changed. */
  share(key: string, value: unknown): void;
  onShared(listener: (key: string, value: unknown) => void): () => void;
  setTrayStatus(tone: string, title: string): Promise<void>;
  /** Makes the popover window as tall as its content, in CSS pixels. */
  fitPopover(height: number): Promise<void>;
  /** "glass" where the system blurs what is behind the popover window. */
  popoverMaterial(): Promise<"glass" | "solid">;
  openMain(): Promise<void>;
  openPopover(): Promise<void>;
  quit(): Promise<void>;
  /** The backups with a history in a folder and below. Reads no footage. */
  findBackups(folder: string): Promise<HistoryRow[]>;
  /** Checks those backups against their histories; stopped with cancelJob. */
  startVerify(roots: string[], record: boolean, notify: boolean): Promise<void>;
  onVerify(handlers: VerifyHandlers): () => void;
  /** Ejects a removable volume; rejects with the reason if it cannot. */
  eject(path: string): Promise<void>;
}

/** Registers a Tauri event listener and returns a synchronous "stop". */
function on<T>(event: string, listener: (payload: T) => void): () => void {
  const pending = listen<T>(event, (e) => listener(e.payload));
  return () => void pending.then((unlisten) => unlisten());
}

const tauri: Api = {
  listVolumes: () => invoke("list_volumes"),
  analyzeCard: (source) => invoke("analyze_card", { source }),
  inspect: (source, destinations, layout) => invoke("inspect", { source, destinations, layout }),
  listClients: (destination) => invoke("list_clients", { destination }),
  previewShoot: (source, destination, layout) =>
    invoke("preview_shoot", { source, destination, layout }),
  createShoot: (source, destinations, layout) =>
    invoke("create_shoot", { source, destinations, layout }),
  cameraNames: () => invoke("camera_names"),
  rememberCamera: (key, name) => invoke("remember_camera", { key, name }),
  startJob: (spec, layout, extras) => invoke("start_job", { spec, layout, extras }),
  openPath: (path) => invoke("open_path", { path }),
  cancelJob: () => invoke("cancel_job"),
  pickFolder: async (title) => {
    // Development builds only: an automated UI test can queue folders here,
    // because the native picker cannot be scripted. Stripped from releases.
    if (import.meta.env.DEV) {
      const scripted = (window as { __scriptedPicks?: string[] }).__scriptedPicks?.shift();
      if (scripted) return scripted;
    }
    // The system dialog takes the focus; keep the popover from closing under it.
    await invoke("pin_popover", { pinned: true });
    try {
      const picked = await open({ directory: true, multiple: false, title });
      return typeof picked === "string" ? picked : null;
    } finally {
      await invoke("pin_popover", { pinned: false });
    }
  },
  subscribe: ({ onEvent, onFinished, onFailed }) => {
    const stop = [
      on("job-event", onEvent),
      on("job-finished", onFinished),
      on("job-failed", onFailed),
    ];
    return () => stop.forEach((off) => off());
  },
  onVolumes: (listener) => on("volumes-changed", listener),
  onCardInserted: (listener) => on("card-inserted", listener),
  onVolumeRemoved: (listener) => on("volume-removed", listener),
  share: (key, value) => void emit("shared", { key, value }),
  onShared: (listener) =>
    on<{ key: string; value: unknown }>("shared", ({ key, value }) => listener(key, value)),
  setTrayStatus: (tone, title) => invoke("set_tray_status", { tone, title }),
  fitPopover: (height) => invoke("fit_popover", { height }),
  popoverMaterial: () => invoke("popover_material"),
  openMain: () => invoke("open_main"),
  openPopover: () => invoke("open_popover"),
  quit: () => invoke("quit"),
  eject: (path) => invoke("eject", { path }),
  findBackups: (folder) => invoke("find_backups", { folder }),
  startVerify: (roots, record, notify) => invoke("start_verify", { roots, record, notify }),
  onVerify: ({ onProgress, onFinished }) => {
    const stop = [on("verify-progress", onProgress), on("verify-finished", onFinished)];
    return () => stop.forEach((off) => off());
  },
};

/**
 * Stand-in used when the page runs in a plain browser (`npm run dev` without
 * Tauri), so the interface can be worked on without cards or drives.
 */
function demo(): Api {
  let handlers: JobHandlers | null = null;
  let verifying: VerifyHandlers | null = null;
  // Two browser tabs stand in for the two windows.
  const channel = new BroadcastChannel("shared");
  let timer: number | undefined;
  let backedUp = false;
  const total = 131;
  const size = 172.4e9;
  const shootDir = (layout: ShootLayout) => {
    const [year, month, day] = layout.date.split("-");
    const date =
      layout.date_format === "day_month_year"
        ? `${day}.${month}.${year.slice(2)}`
        : layout.date;
    return `${layout.client.trim()}/${`${date} ${layout.shoot.trim()}`.trim()}`;
  };
  const known = ["Kestrel Running"];
  let job: { destinations: string[]; extras: Extras } = {
    destinations: [],
    extras: { mhl: false, report: false, notify: false },
  };
  const finish = (report: JobReport) =>
    handlers?.onFinished({
      report,
      extras: report.cancelled
        ? []
        : job.destinations.map((destination) => ({
            destination,
            mhl: job.extras.mhl
              ? { manifest: "", number: 1, original: total, verified: 0, failed: [] }
              : null,
            mhl_error: null,
            report: job.extras.report ? `${destination}\\Reports\\report.html` : null,
            report_error: null,
          })),
    });
  return {
    analyzeCard: async () => ({
      video: total,
      photo: 0,
      audio: 0,
      other: 0,
      shot_at: Date.UTC(2026, 10, 8, 10) / 1000,
      camera: { make: "NIKON CORPORATION", model: "NIKON Z6_3", serial: "6020419" },
      structured: false,
    }),
    listClients: async () => [
      { name: "Kestrel Running", years: [2026, 2025] },
      { name: "Kesh Studio", years: [2024] },
      { name: "Archivio", years: [] },
    ],
    previewShoot: async (_source, _destination, layout) => {
      const dir = shootDir(layout);
      const media = [dir, "Footage", layout.camera.trim(), layout.reel.trim()].filter(Boolean);
      const folders = [
        { path: layout.client.trim(), exists: known.includes(layout.client.trim()), files: 0 },
        { path: dir, exists: false, files: 0 },
        ...media.slice(1).map((_, i) => ({
          path: media.slice(0, i + 2).join("/"),
          exists: false,
          files: i + 2 === media.length ? total : 0,
        })),
        { path: `${dir}/Project`, exists: false, files: 0 },
      ];
      return { folders, conflicts: 0 };
    },
    createShoot: async (_source, _destinations, layout) => shootDir(layout),
    cameraNames: async () => ({ "6020419": "Z6 III" }),
    rememberCamera: async () => {},
    listVolumes: async () => [
      { path: "E:\\", name: "NIKON Z6_3", file_system: "exFAT", removable: true, card: true, total: 256e9, available: 83.6e9 },
      { path: "F:\\", name: "T7 Shield", file_system: "exFAT", removable: true, card: false, total: 2e12, available: 1.31e12 },
      { path: "C:\\", name: "Windows", file_system: "NTFS", removable: false, card: false, total: 1e12, available: 52e9 },
    ],
    inspect: async (_source, destinations) => ({
      files: total,
      bytes: size,
      dests: destinations.map((path) => ({
        path,
        online: true,
        missing: backedUp ? 0 : total,
        different: 0,
        present: backedUp ? total : 0,
        missing_bytes: backedUp ? 0 : size,
      })),
    }),
    openPath: async () => {},
    onVolumes: () => () => {},
    onCardInserted: () => () => {},
    onVolumeRemoved: () => () => {},
    share: (key, value) => channel.postMessage({ key, value }),
    onShared: (listener) => {
      const hear = (e: MessageEvent) => listener(e.data.key, e.data.value);
      channel.addEventListener("message", hear);
      return () => channel.removeEventListener("message", hear);
    },
    setTrayStatus: async () => {},
    fitPopover: async () => {},
    popoverMaterial: async () => "solid",
    openMain: async () => {},
    openPopover: async () => {},
    quit: async () => {},
    findBackups: async (folder) => [
      {
        root: `${folder}\\08.11.26 Trail series`,
        generations: 2,
        files: total,
        bytes: size,
        error: null,
      },
    ],
    startVerify: async (roots, record) => {
      let done = 0;
      timer = window.setInterval(() => {
        done = Math.min(1, done + 0.02);
        const index = Math.min(total - 1, Math.floor(done * total));
        verifying?.onProgress({
          index,
          total,
          path: `Footage/Z6 III/DSC_${String(index + 1).padStart(4, "0")}.MOV`,
          bytes_done: done * size,
          bytes_total: size,
        });
        if (done >= 1) {
          window.clearInterval(timer);
          verifying?.onFinished([
            {
              root: roots[0],
              error: null,
              report: {
                root: roots[0],
                files: total,
                verified: total - 1,
                mismatched: [
                  { path: "Footage/Z6 III/DSC_0042.MOV", format: "xxh64", expected: "a", actual: "b" },
                ],
                missing: [],
                untracked: ["Footage/Z6 III/notes.txt"],
                unsupported: [],
                cancelled: false,
                recorded: record
                  ? { manifest: "", number: 3, original: 0, verified: total - 1, failed: [] }
                  : null,
                duration_ms: 6000,
              },
            },
          ]);
        }
      }, 120);
    },
    onVerify: (handlers) => {
      verifying = handlers;
      return () => {
        verifying = null;
      };
    },
    eject: (path) =>
      new Promise((resolve, reject) =>
        window.setTimeout(
          () => (path.startsWith("E:") ? resolve() : reject("Questo disco non è rimovibile.")),
          700,
        ),
      ),
    startJob: async (spec, _layout, extras) => {
      job = { destinations: spec.destinations, extras };
      let work = 0;
      timer = window.setInterval(() => {
        work += 0.012;
        const copied = Math.min(1, work);
        const index = Math.min(total - 1, Math.floor(copied * total));
        handlers?.onEvent({
          type: "file_started",
          index,
          rel_path: `DCIM/100NCZ_6/DSC_${String(index + 1).padStart(4, "0")}.MOV`,
          size: size / total,
        });
        handlers?.onEvent({
          type: "progress",
          index,
          phase: index % 2 ? "verifying" : "copying",
          bytes_done: copied * size,
          bytes_total: size,
          work_done: copied * size * 2,
          work_total: size * 2,
        });
        if (work >= 1) {
          window.clearInterval(timer);
          backedUp = true;
          finish({
            files: [],
            total_files: total,
            total_bytes: size,
            failed_files: 0,
            cancelled: false,
            safe_to_format: true,
            duration_ms: 1000,
          });
        }
      }, 120);
    },
    cancelJob: async () => {
      window.clearInterval(timer);
      finish({
        files: [],
        total_files: total,
        total_bytes: size,
        failed_files: 0,
        cancelled: true,
        safe_to_format: false,
        duration_ms: 1000,
      });
    },
    pickFolder: async () => "F:\\2026\\Kestrel Running\\08.11.26 Trail series\\Footage",
    subscribe: (h) => {
      handlers = h;
      return () => {
        handlers = null;
      };
    },
  };
}

export const api: Api = "__TAURI_INTERNALS__" in window ? tauri : demo();
