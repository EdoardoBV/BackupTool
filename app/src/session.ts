import { useCallback, useEffect, useRef, useState } from "react";
import {
  api,
  type CardInfo,
  type DestExtras,
  type FileResult,
  type HashAlgo,
  type Inspection,
  type JobReport,
  type ShootLayout,
  type VerifyMode,
  type Volume,
} from "./api";
import { bytes, files, leaf, list, theFiles } from "./format";

/**
 * The state of the app as both windows see it: the main window and the tray
 * popover each run this hook, and the parts that must agree (which card,
 * which destinations, which shoot, the options) are shared between them.
 */

export type Progress = {
  index: number;
  file: string;
  phase: "copying" | "verifying";
  bytesDone: number;
  bytesTotal: number;
  workDone: number;
  workTotal: number;
};

/** An active shoot: its layout and its folder relative to each destination. */
export type Shoot = { layout: ShootLayout; dir: string };

export type Tone = "blank" | "red" | "yellow" | "green";
export type Status = { tone: Tone; title: string; detail?: string };

/**
 * While a new destination is being looked at, the last inspection still
 * describes the previous list; it must not be read as covering the new one.
 */
function covers(inspection: Inspection, destinations: string[]): boolean {
  return (
    inspection.dests.length === destinations.length &&
    inspection.dests.every((dest, i) => dest.path === destinations[i])
  );
}

function describe(
  source: string | null,
  destinations: string[],
  inspection: Inspection | null,
  inspectError: string | null,
  running: boolean,
  report: JobReport | null,
  jobError: string | null,
  ejected: string | null,
): Status {
  const drives = list(destinations.map(leaf));
  if (!source) {
    if (ejected) {
      return { tone: "green", title: "Scheda espulsa.", detail: `Puoi togliere ${ejected}.` };
    }
    return { tone: "blank", title: "Scegli la scheda da mettere al sicuro." };
  }
  if (inspectError) {
    return { tone: "red", title: "Non riesco a leggere la scheda.", detail: inspectError };
  }
  if (running) {
    return {
      tone: "yellow",
      title: "Copia in corso. Non formattare.",
      detail: "Ogni file viene riletto e confrontato con la scheda.",
    };
  }
  if (jobError) {
    return { tone: "red", title: "La copia non è partita. Non formattare.", detail: jobError };
  }
  if (report) {
    if (report.safe_to_format) {
      return {
        tone: "green",
        title: "Puoi formattare.",
        detail: `Tutti ${theFiles(report.total_files)} sono verificati su ${drives}.`,
      };
    }
    if (report.cancelled) {
      return {
        tone: "red",
        title: "Copia fermata. Non formattare.",
        detail: `${report.files.length} di ${files(report.total_files)} completati. Riprendi quando vuoi: riparte da dove era arrivata.`,
      };
    }
    if (report.failed_files > 0) {
      return {
        tone: "red",
        title: "Non formattare.",
        detail: `${files(report.failed_files)} non verificati. Li trovi elencati nella finestra.`,
      };
    }
    return {
      tone: "yellow",
      title: "Copiato, ma non verificato. Non formattare.",
      detail: "Senza verifica non c'è prova che le copie siano integre.",
    };
  }
  if (!inspection) {
    return { tone: "blank", title: "Lettura della scheda…" };
  }
  if (inspection.files === 0) {
    return { tone: "blank", title: "Qui non ci sono file da copiare." };
  }
  const summary = `${files(inspection.files)}, ${bytes(inspection.bytes)}`;
  if (destinations.length === 0) {
    return { tone: "red", title: "Non formattare.", detail: `${summary}. Aggiungi dove copiarli.` };
  }
  if (!covers(inspection, destinations)) {
    return {
      tone: "red",
      title: "Non formattare.",
      detail: `${summary}. Controllo cosa c'è già su ${drives}…`,
    };
  }
  const offline = inspection.dests.filter((dest) => !dest.online).map((dest) => leaf(dest.path));
  if (offline.length > 0) {
    return {
      tone: "red",
      title: "Non formattare.",
      detail:
        offline.length === 1
          ? `${offline[0]} non è collegato. Collegalo, oppure toglilo dalle destinazioni.`
          : `${list(offline)} non sono collegati. Collegali, oppure toglili dalle destinazioni.`,
    };
  }
  const worst = inspection.dests.reduce((a, b) =>
    b.missing + b.different > a.missing + a.different ? b : a,
  );
  const absent = worst.missing + worst.different;
  if (absent > 0) {
    return {
      tone: "red",
      title: "Non formattare.",
      detail: `${absent} di ${files(inspection.files)} non sono ancora su ${leaf(worst.path)}.`,
    };
  }
  return {
    tone: "yellow",
    title: "Non ancora verificato. Non formattare.",
    detail: `Tutti ${theFiles(inspection.files)} sembrano già su ${drives}, ma contano solo nome e dimensione. Verifica per esserne certo.`,
  };
}

function stored<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(`shared:${key}`);
    return raw === null ? fallback : (JSON.parse(raw) as T);
  } catch {
    return fallback;
  }
}

/**
 * A value both windows hold. Setting it in one updates the other; with
 * `persist` it is also still there the next time the app starts.
 */
export function useShared<T>(key: string, initial: T, persist: boolean): [T, (next: T) => void] {
  const [value, setValue] = useState<T>(() => (persist ? stored(key, initial) : initial));

  useEffect(
    () =>
      api.onShared((changed, next) => {
        if (changed !== key) return;
        // The sender hears its own message too: keep the same object when
        // nothing changed, so effects that depend on it do not run again.
        setValue((current) =>
          JSON.stringify(current) === JSON.stringify(next) ? current : (next as T),
        );
      }),
    [key],
  );

  const set = useCallback(
    (next: T) => {
      setValue(next);
      if (persist) {
        try {
          localStorage.setItem(`shared:${key}`, JSON.stringify(next));
        } catch {
          // Remembering it for next time is a convenience only.
        }
      }
      api.share(key, next);
    },
    [key, persist],
  );

  return [value, set];
}

export function useSession() {
  const [volumes, setVolumes] = useState<Volume[]>([]);
  const [source, setSource] = useShared<string | null>("source", null, false);
  const [destinations, setDestinations] = useShared<string[]>("destinations", [], true);
  const [shoot, setShoot] = useShared<Shoot | null>("shoot", null, false);
  const [lastLayout, setLastLayout] = useShared<ShootLayout | null>("lastLayout", null, true);
  const [algo, setAlgo] = useShared<HashAlgo>("algo", "xxh64", true);
  const [verify, setVerify] = useShared<VerifyMode>("verify", "destination", true);
  const [wantMhl, setWantMhl] = useShared("mhl", true, true);
  const [wantReport, setWantReport] = useShared("report", true, true);
  const [wantNotify, setWantNotify] = useShared("notify", true, true);

  const [inspection, setInspection] = useState<Inspection | null>(null);
  const [inspectError, setInspectError] = useState<string | null>(null);
  // undefined while the card is being read, null when nothing could be told.
  const [card, setCard] = useState<CardInfo | null | undefined>(undefined);
  const [running, setRunning] = useState(false);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [report, setReport] = useState<JobReport | null>(null);
  const [failures, setFailures] = useState<FileResult[]>([]);
  const [written, setWritten] = useState<DestExtras[]>([]);
  const [jobError, setJobError] = useState<string | null>(null);
  // Name of the card just ejected, until another one is chosen.
  const [ejected, setEjected] = useShared<string | null>("ejected", null, false);
  const [ejecting, setEjecting] = useState(false);
  const [ejectError, setEjectError] = useState<string | null>(null);

  const startedAt = useRef(0);
  const currentFile = useRef("");
  const runningNow = useRef(false);
  // What event handlers registered once need to read as it is now.
  const latest = useRef({ source, destinations });
  latest.current = { source, destinations };

  const refreshVolumes = useCallback(
    () => api.listVolumes().then(setVolumes, () => {}),
    [],
  );

  const layout = shoot?.layout ?? null;

  const reset = useCallback(() => {
    setReport(null);
    setFailures([]);
    setWritten([]);
    setJobError(null);
  }, []);

  const chooseSource = useCallback(
    (path: string) => {
      // The folders depend on what the card holds: confirm them per card.
      setShoot(null);
      setEjected(null);
      setSource(path);
      if (latest.current.destinations.includes(path)) {
        setDestinations(latest.current.destinations.filter((dest) => dest !== path));
      }
    },
    [setShoot, setEjected, setSource, setDestinations],
  );

  // Drives come and go. A card that is already in when the app starts is
  // taken as the source, like one plugged in later.
  useEffect(() => {
    let first = true;
    api.listVolumes().then(
      (found) => {
        setVolumes(found);
        const present = found.find((volume) => volume.card);
        if (first && present && !latest.current.source) chooseSource(present.path);
        first = false;
      },
      () => {},
    );
    const stop = [
      api.onVolumes(setVolumes),
      api.onCardInserted((path) => {
        if (!runningNow.current) chooseSource(path);
      }),
      api.onVolumeRemoved((path) => {
        if (!runningNow.current && latest.current.source === path) setSource(null);
      }),
    ];
    return () => stop.forEach((off) => off());
  }, [chooseSource, setSource]);

  // What a finished job said no longer applies once any of these change,
  // whichever window changed them.
  useEffect(() => {
    if (!runningNow.current) reset();
  }, [source, destinations, layout, reset]);

  const inspect = useCallback(() => {
    if (!source) {
      setInspection(null);
      return () => {};
    }
    let alive = true;
    setInspectError(null);
    api.inspect(source, destinations, layout).then(
      (found) => alive && setInspection(found),
      (err) => alive && (setInspection(null), setInspectError(String(err))),
    );
    return () => {
      alive = false;
    };
  }, [source, destinations, layout]);

  useEffect(() => {
    setInspection(null);
    setEjectError(null);
  }, [source]);

  useEffect(() => inspect(), [inspect]);

  useEffect(() => {
    setCard(undefined);
    if (!source) return;
    let alive = true;
    api.analyzeCard(source).then(
      (info) => alive && setCard(info),
      () => alive && setCard(null),
    );
    return () => {
      alive = false;
    };
  }, [source]);

  /** Marks a job as under way; the other window learns it from the first event. */
  const begin = useCallback(() => {
    if (runningNow.current) return;
    runningNow.current = true;
    reset();
    setProgress(null);
    startedAt.current = Date.now();
    setRunning(true);
  }, [reset]);

  const end = useCallback(() => {
    runningNow.current = false;
    setRunning(false);
    setProgress(null);
  }, []);

  useEffect(
    () =>
      api.subscribe({
        onEvent: (event) => {
          begin();
          if (event.type === "file_started") {
            currentFile.current = event.rel_path;
          } else if (event.type === "progress") {
            setProgress({
              index: event.index,
              file: currentFile.current,
              phase: event.phase,
              bytesDone: event.bytes_done,
              bytesTotal: event.bytes_total,
              workDone: event.work_done,
              workTotal: event.work_total,
            });
          } else if (
            event.result.error ||
            event.result.dests.some((dest) => dest.status === "failed")
          ) {
            setFailures((known) => [...known, event.result]);
          }
        },
        onFinished: (finished) => {
          end();
          setReport(finished.report);
          setWritten(finished.extras);
          refreshVolumes();
        },
        onFailed: (message) => {
          end();
          setJobError(message);
        },
      }),
    [begin, end, refreshVolumes],
  );

  // A finished job changes what is on the destinations.
  useEffect(() => {
    if (report) return inspect();
  }, [report, inspect]);

  const browseSource = async () => {
    const picked = await api.pickFolder("Scegli la scheda o la cartella da copiare");
    if (picked) chooseSource(picked);
  };

  const addDestination = async () => {
    const picked = await api.pickFolder("Scegli dove copiare");
    if (picked && picked !== source && !destinations.includes(picked)) {
      const next = [...destinations, picked];
      // An active shoot gets its folders on the new drive too.
      if (source && shoot) await api.createShoot(source, next, shoot.layout).catch(() => {});
      setDestinations(next);
    }
  };

  const removeDestination = (path: string) =>
    setDestinations(destinations.filter((dest) => dest !== path));

  // The standard has no SHA-256, and an unverified copy proves nothing to record.
  const mhlPossible = verify !== "none" && algo !== "sha256";

  const start = async () => {
    if (!source) return;
    begin();
    try {
      await api.startJob({ source, destinations, options: { algo, verify } }, layout, {
        mhl: wantMhl && mhlPossible,
        report: wantReport,
        notify: wantNotify,
      });
    } catch (err) {
      end();
      setJobError(String(err));
    }
  };

  // Only a removable volume can be ejected: not a folder, not a fixed disk.
  const sourceVolume = volumes.find((volume) => volume.path === source);
  const ejectable = sourceVolume?.removable === true;

  const eject = async () => {
    if (!source || !ejectable) return;
    const name = sourceVolume.name || leaf(source);
    setEjecting(true);
    setEjectError(null);
    try {
      await api.eject(source);
      setEjected(name);
      setSource(null);
    } catch (err) {
      setEjectError(String(err));
    } finally {
      setEjecting(false);
    }
  };

  const status = describe(
    source,
    destinations,
    inspection,
    inspectError,
    running,
    report,
    jobError,
    ejected,
  );
  const upToDate = inspection !== null && covers(inspection, destinations);
  const allPresent =
    upToDate &&
    inspection.dests.length > 0 &&
    inspection.dests.every((dest) => dest.missing + dest.different === 0);
  const canStart =
    !running &&
    upToDate &&
    inspection.files > 0 &&
    destinations.length > 0 &&
    inspection.dests.every((dest) => dest.online);
  const done = report?.safe_to_format === true;
  const startLabel = allPresent ? "Verifica" : verify === "none" ? "Copia" : "Copia e verifica";

  const fraction = progress && progress.workTotal > 0 ? progress.workDone / progress.workTotal : 0;
  const elapsed = (Date.now() - startedAt.current) / 1000;
  const timeLeft =
    progress && fraction > 0.01 && elapsed > 3 ? (elapsed / fraction) * (1 - fraction) : NaN;

  return {
    volumes,
    source,
    destinations,
    shoot,
    setShoot,
    lastLayout,
    setLastLayout,
    algo,
    setAlgo,
    verify,
    setVerify,
    wantMhl,
    setWantMhl,
    wantReport,
    setWantReport,
    wantNotify,
    setWantNotify,
    mhlPossible,
    inspection,
    card,
    running,
    progress,
    report,
    failures,
    written,
    status,
    canStart,
    done,
    startLabel,
    fraction,
    timeLeft,
    chooseSource,
    browseSource,
    addDestination,
    removeDestination,
    start,
    stop: () => api.cancelJob(),
    ejectable,
    ejecting,
    ejectError,
    eject,
  };
}
