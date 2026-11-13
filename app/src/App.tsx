import { useEffect, useState } from "react";
import { api, type DestState, type HashAlgo, type VerifyMode } from "./api";
import { bytes, cameraName, files, leaf, remaining } from "./format";
import { useSession, useShared } from "./session";
import ShootDialog from "./ShootDialog";
import VerifyDialog from "./VerifyDialog";
import {
  CameraIcon,
  CardIcon,
  CloseIcon,
  DriveIcon,
  EjectIcon,
  FolderIcon,
  Lens,
  NetworkIcon,
  PlusIcon,
} from "./ui";

const ALGORITHMS: [HashAlgo, string][] = [
  ["xxh64", "xxHash64"],
  ["xxh3", "XXH3"],
  ["xxh128", "XXH128"],
  ["md5", "MD5"],
  ["sha1", "SHA-1"],
  ["sha256", "SHA-256"],
  ["c4", "C4"],
];

const VERIFY_MODES: [VerifyMode, string][] = [
  ["destination", "Rileggi ogni copia"],
  ["source_and_destination", "Rileggi le copie e la scheda"],
  ["none", "Nessuna verifica"],
];

/**
 * Paths are cut at the start, not the end, by laying them out right to
 * left. This invisible mark keeps a trailing "\\" or ":" where it belongs.
 */
const LTR = "\u200E";

/** What the popover asks this window to do when it brings it forward. */
export type Intent = { kind: "new-shoot"; at: number } | null;

/** How a destination stands, in a few words and a colour. */
export function destinationState(state: DestState, done: boolean): { tone: string; text: string } {
  const absent = state.missing + state.different;
  if (!state.online) return { tone: "red", text: "non collegato" };
  if (absent > 0) return { tone: "red", text: `mancano ${files(absent)}` };
  if (done) return { tone: "green", text: "tutto verificato" };
  return { tone: "yellow", text: "da verificare" };
}

export default function App() {
  const {
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
    stop,
    ejectable,
    ejecting,
    ejectError,
    eject,
  } = useSession();
  const [choosingShoot, setChoosingShoot] = useState(false);
  // The backup being checked against its history, while that dialog is open.
  const [verifying, setVerifying] = useState<string | null>(null);

  const chooseBackup = async () => {
    const picked = await api.pickFolder("Scegli il backup da verificare");
    if (picked) setVerifying(picked);
  };
  const [intent] = useShared<Intent>("intent", null, false);

  useEffect(() => {
    if (intent?.kind === "new-shoot") setChoosingShoot(true);
  }, [intent]);

  const sources = volumes.filter((volume) => volume.removable || volume.card || volume.network);
  const sourceIsFolder = source !== null && !sources.some((volume) => volume.path === source);
  const camera = card?.camera;

  return (
    <div className="app">
      <header className="win-top" role="status" aria-live="polite">
        <Lens tone={status.tone} large busy={running} />
        <div>
          <h1 className="win-title">{status.title}</h1>
          {status.detail && <p className="win-sub">{status.detail}</p>}
        </div>
        <nav className="win-links">
          <button className="link" disabled={running} onClick={chooseBackup}>
            Verifica un backup…
          </button>
          <button className="link" onClick={() => api.openPopover()}>
            Vista compatta
          </button>
        </nav>
      </header>

      <main className="win-body">
        <section className="card" aria-labelledby="source-heading">
          <h2 id="source-heading">Scheda</h2>
          <ul>
            {sources.map((volume) => (
              <li key={volume.path}>
                <button
                  className="item item-pick"
                  aria-pressed={source === volume.path}
                  disabled={running}
                  onClick={() => chooseSource(volume.path)}
                >
                  {volume.card ? <CardIcon /> : volume.network ? <NetworkIcon /> : <DriveIcon />}
                  <b>{volume.name || volume.path}</b>
                  <small>
                    {volume.network
                      ? `${volume.path} Unità di rete`
                      : `${volume.path} ${bytes(volume.total - volume.available)} usati di ${bytes(volume.total)}`}
                  </small>
                </button>
              </li>
            ))}
            {sourceIsFolder && (
              <li>
                <div className="item item-pick" aria-pressed="true">
                  <FolderIcon />
                  <b>{leaf(source)}</b>
                  <small className="path">{source}{LTR}</small>
                </div>
              </li>
            )}
            {source && inspection && (
              <li>
                <div className="item">
                  <CameraIcon />
                  <b>
                    {camera ? cameraName(camera) : "Camera non riconosciuta"}
                  </b>
                  <small>
                    {files(inspection.files)}, {bytes(inspection.bytes)}
                    {camera?.serial && `, seriale …${camera.serial.slice(-4)}`}
                  </small>
                </div>
              </li>
            )}
          </ul>
          <div className="card-foot">
            <button className="btn" disabled={running} onClick={browseSource}>
              <FolderIcon />
              Scegli una cartella…
            </button>
            {ejectable && (
              <button className="btn" disabled={running || ejecting} onClick={eject}>
                <EjectIcon />
                {ejecting ? "Espulsione…" : "Espelli la scheda"}
              </button>
            )}
          </div>
          {ejectError && <p className="warn hint">{ejectError}</p>}
        </section>

        <section className="card" aria-labelledby="dest-heading">
          <h2 id="dest-heading">Destinazioni</h2>
          <ul>
            {destinations.map((dest) => {
              const state = inspection?.dests.find((d) => d.path === dest);
              const shown = state && !running ? destinationState(state, done) : null;
              const extras = running ? undefined : written.find((w) => w.destination === dest);
              return (
                <li key={dest}>
                  <div className="item">
                    <DriveIcon />
                    <b>{leaf(dest)}</b>
                    <span className="item-aside">
                      {shown && (
                        <span className={`chip tone-${shown.tone}`}>
                          {shown.text}
                          {state && state.missing > 0 && `, ${bytes(state.missing_bytes)}`}
                        </span>
                      )}
                      <button
                        className="remove"
                        disabled={running}
                        aria-label={`Togli ${leaf(dest)}`}
                        title="Togli dalle destinazioni"
                        onClick={() => removeDestination(dest)}
                      >
                        <CloseIcon />
                      </button>
                    </span>
                    <small className="path">{dest}{LTR}</small>
                  </div>
                  {extras && (
                    <div className="extras">
                      {extras.mhl && (
                        <span>
                          MHL: generazione {extras.mhl.number}, {files(extras.mhl.original)} nuovi
                          {extras.mhl.verified > 0 && ` e ${extras.mhl.verified} riverificati`}
                        </span>
                      )}
                      {extras.mhl && extras.mhl.failed.length > 0 && (
                        <span className="warn">
                          {files(extras.mhl.failed.length)} non corrispondono più a quanto registrato
                          in passato nella storia MHL.
                        </span>
                      )}
                      {extras.mhl_error && (
                        <span className="warn">MHL non scritto: {extras.mhl_error}</span>
                      )}
                      {extras.report_error && (
                        <span className="warn">Report non scritto: {extras.report_error}</span>
                      )}
                      {extras.report && (
                        <button className="link" onClick={() => api.openPath(extras.report!)}>
                          Apri il report
                        </button>
                      )}
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
          {shoot && (
            <div className="shoot">
              <FolderIcon />
              <span className="shoot-path">{shoot.dir.split("/").join(" › ")}</span>
              <span className="shoot-actions">
                <button className="link" disabled={running} onClick={() => setChoosingShoot(true)}>
                  Modifica
                </button>
                <button className="link quiet" disabled={running} onClick={() => setShoot(null)}>
                  Copia senza shoot
                </button>
              </span>
            </div>
          )}
          <div className="card-foot">
            <button className="btn" disabled={running} onClick={addDestination}>
              <PlusIcon />
              {destinations.length === 0 ? "Aggiungi una destinazione…" : "Aggiungi un'altra copia…"}
            </button>
            {!shoot && source && destinations.length > 0 && (
              <button
                className="btn"
                disabled={running || card === undefined}
                onClick={() => setChoosingShoot(true)}
              >
                <FolderIcon />
                Nuovo shoot…
              </button>
            )}
          </div>
          {destinations.length === 1 && (
            <p className="hint">
              Una copia sola non è un backup: aggiungine una seconda su un altro disco.
            </p>
          )}
        </section>
      </main>

      {failures.length > 0 && (
        <section className="failures" aria-labelledby="failures-heading">
          <h2 id="failures-heading">File non verificati</h2>
          <ul>
            {failures.map((file) => (
              <li key={file.rel_path}>
                <b>{file.rel_path}</b>
                <small>
                  {file.error ??
                    file.dests
                      .map((dest, i) => dest.error && `${leaf(destinations[i] ?? "")}: ${dest.error}`)
                      .filter(Boolean)
                      .join(" ")}
                </small>
              </li>
            ))}
          </ul>
        </section>
      )}

      <footer className="win-bar">
        {running ? (
          <div className="progress">
            <p className="progress-line">
              {progress ? (
                <>
                  <span>
                    {progress.phase === "copying" ? "Copia" : "Verifica"} del file{" "}
                    {progress.index + 1} di {inspection?.files ?? "…"}, {bytes(progress.bytesDone)} di{" "}
                    {bytes(progress.bytesTotal)}
                  </span>
                  {Number.isFinite(timeLeft) && <span className="muted">{remaining(timeLeft)}</span>}
                </>
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
            <p className="progress-file">{progress?.file}</p>
          </div>
        ) : (
          <div className="options">
            <select
              aria-label="Checksum"
              title="Checksum"
              value={algo}
              onChange={(e) => setAlgo(e.target.value as HashAlgo)}
            >
              {ALGORITHMS.map(([value, label]) => (
                <option key={value} value={value}>
                  {label}
                </option>
              ))}
            </select>
            <select
              aria-label="Verifica"
              title="Verifica"
              value={verify}
              onChange={(e) => setVerify(e.target.value as VerifyMode)}
            >
              {VERIFY_MODES.map(([value, label]) => (
                <option key={value} value={value}>
                  {label}
                </option>
              ))}
            </select>
            <label className="switch" title="Storia ASC MHL nella cartella ascmhl">
              <input
                type="checkbox"
                checked={wantMhl && mhlPossible}
                disabled={!mhlPossible}
                onChange={(e) => setWantMhl(e.target.checked)}
              />
              MHL
            </label>
            <label className="switch" title="Report HTML e CSV nella cartella Reports">
              <input
                type="checkbox"
                checked={wantReport}
                onChange={(e) => setWantReport(e.target.checked)}
              />
              Report
            </label>
            <label
              className="switch"
              title="Avviso di sistema a fine copia, se non stai guardando l'app"
            >
              <input
                type="checkbox"
                checked={wantNotify}
                onChange={(e) => setWantNotify(e.target.checked)}
              />
              Notifiche
            </label>
            {!mhlPossible && wantMhl && (
              <p className="option-note">
                {algo === "sha256"
                  ? "Lo standard MHL non prevede SHA-256."
                  : "Senza verifica non c'è nulla da registrare in MHL."}
              </p>
            )}
          </div>
        )}
        {running ? (
          <button className="btn large" onClick={stop}>
            Ferma
          </button>
        ) : (
          <button className="btn primary large" disabled={!canStart} onClick={start}>
            {startLabel}
          </button>
        )}
      </footer>

      {verifying && (
        <VerifyDialog folder={verifying} notify={wantNotify} onClose={() => setVerifying(null)} />
      )}

      {choosingShoot && source && (
        <ShootDialog
          source={source}
          destinations={destinations}
          card={card ?? null}
          previous={shoot?.layout ?? lastLayout}
          onCancel={() => setChoosingShoot(false)}
          onCreated={(created, dir) => {
            setShoot({ layout: created, dir });
            setLastLayout(created);
            setChoosingShoot(false);
          }}
        />
      )}
    </div>
  );
}
