import { useEffect, useRef, useState } from "react";
import { api, type Checked, type HistoryRow, type VerifyProgress } from "./api";
import { bytes, files, leaf, theFiles } from "./format";
import type { Tone } from "./session";
import { FolderIcon, Lens } from "./ui";

type Props = {
  /** The folder the user picked: a backup, or a folder of backups. */
  folder: string;
  /** Whether to send a system notification when the check ends. */
  notify: boolean;
  onClose: () => void;
};

/** At most this many paths per list: beyond that the count says enough. */
const SHOWN = 200;

function Paths({ title, tone, paths }: { title: string; tone: Tone; paths: string[] }) {
  if (paths.length === 0) return null;
  return (
    <section className={`found tone-${tone}`}>
      <h3>
        {title}, {paths.length}
      </h3>
      <ul>
        {paths.slice(0, SHOWN).map((path) => (
          <li key={path}>{path}</li>
        ))}
        {paths.length > SHOWN && <li className="muted">e altri {paths.length - SHOWN}</li>}
      </ul>
    </section>
  );
}

/**
 * Checks an existing backup against its ASC MHL history: what is there, a
 * progress bar while every file is read again, then what was found.
 */
export default function VerifyDialog({ folder, notify, onClose }: Props) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [found, setFound] = useState<HistoryRow[] | null>(null);
  const [record, setRecord] = useState(true);
  const [phase, setPhase] = useState<"ready" | "running" | "done">("ready");
  const [progress, setProgress] = useState<VerifyProgress | null>(null);
  const [results, setResults] = useState<Checked[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (dialog.current && !dialog.current.open) dialog.current.showModal();
  }, []);

  useEffect(() => {
    let alive = true;
    api.findBackups(folder).then(
      (rows) => alive && setFound(rows),
      (err) => alive && (setFound([]), setError(String(err))),
    );
    return () => {
      alive = false;
    };
  }, [folder]);

  useEffect(
    () =>
      api.onVerify({
        onProgress: setProgress,
        onFinished: (checked) => {
          setResults(checked);
          setPhase("done");
        },
      }),
    [],
  );

  const usable = (found ?? []).filter((row) => !row.error);
  const sealed = (found ?? []).filter((row) => row.error);
  const totalFiles = usable.reduce((sum, row) => sum + row.files, 0);
  const totalBytes = usable.reduce((sum, row) => sum + row.bytes, 0);

  const start = async () => {
    setError(null);
    setProgress(null);
    setPhase("running");
    try {
      await api.startVerify(
        usable.map((row) => row.root),
        record,
        notify,
      );
    } catch (err) {
      setError(String(err));
      setPhase("ready");
    }
  };

  // With several backups, say which one each path belongs to.
  const several = results.length > 1;
  const within = (root: string, path: string) => (several ? `${leaf(root)} / ${path}` : path);
  const reports = results.flatMap((r) => (r.report ? [r.report] : []));
  const changed = reports.flatMap((r) => r.mismatched.map((m) => within(r.root, m.path)));
  const missing = reports.flatMap((r) => r.missing.map((p) => within(r.root, p)));
  const unsupported = reports.flatMap((r) => r.unsupported.map((p) => within(r.root, p)));
  const untracked = reports.flatMap((r) => r.untracked.map((p) => within(r.root, p)));
  const failed = results.filter((r) => r.error);
  const cancelled = reports.some((r) => r.cancelled);
  const checkedFiles = reports.reduce((sum, r) => sum + r.files, 0);
  const verifiedFiles = reports.reduce((sum, r) => sum + r.verified, 0);
  const damaged = changed.length + missing.length + unsupported.length + failed.length > 0;
  const recorded = reports.flatMap((r) => (r.recorded ? [r.recorded.number] : []));

  let tone: Tone = "blank";
  let title = "Verifica un backup";
  let detail = `Ogni file viene riletto e confrontato con il checksum registrato quando è stato copiato.`;
  if (phase === "running") {
    tone = "yellow";
    title = "Verifica in corso";
    detail = "Puoi lasciarla lavorare: la finestra resta qui.";
  } else if (phase === "done") {
    if (damaged) {
      tone = "red";
      title = "Il backup non è integro.";
      detail = `${verifiedFiles} file su ${checkedFiles} corrispondono alla storia.`;
    } else if (cancelled) {
      tone = "yellow";
      title = "Verifica fermata.";
      detail = `${verifiedFiles} file controllati su ${checkedFiles}: fin qui nessun problema.`;
    } else {
      tone = "green";
      title = "Il backup è integro.";
      detail = `Tutti ${theFiles(checkedFiles)} corrispondono alla storia MHL.`;
    }
  }
  const fraction =
    progress && progress.bytes_total > 0 ? progress.bytes_done / progress.bytes_total : 0;

  return (
    <dialog
      ref={dialog}
      className="shoot-dialog"
      aria-labelledby="verify-title"
      onCancel={(event) => {
        event.preventDefault();
        if (phase !== "running") onClose();
      }}
    >
      <div className="dialog-head" role="status" aria-live="polite">
        <Lens tone={tone} busy={phase === "running"} />
        <div>
          <h2 id="verify-title">{title}</h2>
          <p className="dialog-lead">{detail}</p>
        </div>
      </div>

      {phase === "ready" &&
        (found === null ? (
          <p className="tree tree-empty">Cerco le storie MHL in {leaf(folder)}…</p>
        ) : found.length === 0 ? (
          <p className="tree tree-empty">
            In {leaf(folder)} non c'è una storia MHL. Si può verificare solo un backup fatto con
            l'opzione MHL attiva, da BackupTool o da un altro programma che scrive ASC MHL.
          </p>
        ) : (
          <>
            <ul className="tree histories">
              {found.map((row) => (
                <li key={row.root} className={row.error ? undefined : "is-new"}>
                  <FolderIcon />
                  <span className="folder">{leaf(row.root)}</span>
                  {row.error ? (
                    <span className="warn">storia alterata, non verificabile</span>
                  ) : (
                    <span className="folder-state">
                      {files(row.files)}, {bytes(row.bytes)}, {row.generations}{" "}
                      {row.generations === 1 ? "generazione" : "generazioni"}
                    </span>
                  )}
                </li>
              ))}
            </ul>
            {sealed.length > 0 && (
              <p className="dialog-warning">
                {sealed.length === 1 ? "Una storia è stata" : `${sealed.length} storie sono state`}{" "}
                modificate dopo essere state scritte: non c'è modo di fidarsi di quello che dicono.
              </p>
            )}
            <label className="switch">
              <input
                type="checkbox"
                checked={record}
                onChange={(e) => setRecord(e.target.checked)}
              />
              Registra la verifica nella storia
            </label>
          </>
        ))}

      {phase === "running" && (
        <div className="progress">
          <p className="progress-line">
            {progress ? (
              <span>
                File {progress.index + 1} di {progress.total}, {bytes(progress.bytes_done)} di{" "}
                {bytes(progress.bytes_total)}
              </span>
            ) : (
              "Preparazione…"
            )}
          </p>
          <div
            className="bar"
            role="progressbar"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.round(fraction * 100)}
          >
            <i style={{ width: `${fraction * 100}%` }} />
          </div>
          <p className="progress-file">{progress?.path}</p>
        </div>
      )}

      {phase === "done" && (
        <div className="findings">
          {failed.map((r) => (
            <p className="dialog-warning" key={r.root}>
              {leaf(r.root)}: {r.error}
            </p>
          ))}
          <Paths title="Modificati" tone="red" paths={changed} />
          <Paths title="Mancanti" tone="red" paths={missing} />
          <Paths title="Checksum di un tipo non supportato" tone="red" paths={unsupported} />
          <Paths title="Non registrati nella storia" tone="yellow" paths={untracked} />
          {untracked.length > 0 && (
            <p className="field-note">
              Dei file non registrati non si può dire se siano integri: la storia non li conosce.
            </p>
          )}
          {recorded.length > 0 && (
            <p className="field-note">
              Verifica registrata nella storia come generazione {recorded.join(", ")}.
            </p>
          )}
        </div>
      )}

      {error && <p className="dialog-warning">{error}</p>}

      <div className="dialog-actions">
        {phase === "ready" && (
          <>
            <button type="button" className="btn" onClick={onClose}>
              Annulla
            </button>
            <button
              type="button"
              className="btn primary"
              disabled={usable.length === 0 || totalFiles === 0}
              onClick={start}
            >
              {totalFiles > 0 ? `Verifica ${files(totalFiles)}, ${bytes(totalBytes)}` : "Verifica"}
            </button>
          </>
        )}
        {phase === "running" && (
          <button type="button" className="btn" onClick={() => api.cancelJob()}>
            Ferma
          </button>
        )}
        {phase === "done" && (
          <button type="button" className="btn primary" onClick={onClose}>
            Chiudi
          </button>
        )}
      </div>
    </dialog>
  );
}
