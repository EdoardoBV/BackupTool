import { useEffect, useRef } from "react";
import { api } from "./api";
import { destinationState, type Intent } from "./App";
import { bytes, files, leaf, remaining } from "./format";
import { useSession } from "./session";
import { CardIcon, DriveIcon, EjectIcon, FolderIcon, Lens } from "./ui";

/**
 * The small window that opens from the tray icon: the same session as the
 * main window, reduced to the one question (is this card safe?) and the one
 * next step.
 */
export default function Popover() {
  const {
    volumes,
    source,
    destinations,
    shoot,
    inspection,
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
    addDestination,
    start,
    stop,
    ejectable,
    ejecting,
    ejectError,
    eject,
  } = useSession();
  const panel = useRef<HTMLDivElement>(null);

  // The tray icon wears the colour of the lens.
  useEffect(() => {
    api.setTrayStatus(status.tone, status.title).catch(() => {});
  }, [status.tone, status.title]);

  // Where the system can blur what is behind the window, let it show through.
  useEffect(() => {
    api.popoverMaterial().then(
      (material) => document.documentElement.classList.toggle("glass", material === "glass"),
      () => {},
    );
  }, []);

  // The window is exactly as tall as what it shows.
  useEffect(() => {
    const element = panel.current;
    if (!element) return;
    const fit = () => api.fitPopover(element.scrollHeight).catch(() => {});
    const observer = new ResizeObserver(fit);
    observer.observe(element);
    fit();
    return () => observer.disconnect();
  }, []);

  const cardName = source
    ? volumes.find((volume) => volume.path === source)?.name || leaf(source)
    : null;
  const report = written.find((w) => w.report)?.report;

  const chooseDestination = async () => {
    await addDestination();
    // The folder dialog took the focus away; come back to the front.
    api.openPopover();
  };

  const newShoot = () => {
    const intent: Intent = { kind: "new-shoot", at: Date.now() };
    api.share("intent", intent);
    api.openMain();
  };

  const ejectButton = (primary: boolean) =>
    ejectable && (
      <button className={primary ? "btn primary" : "btn"} disabled={ejecting} onClick={eject}>
        <EjectIcon />
        {ejecting ? "Espulsione…" : "Espelli"}
      </button>
    );

  return (
    <div className="pop" ref={panel}>
      <div className="pop-head" role="status" aria-live="polite">
        <Lens tone={status.tone} busy={running} />
        <div>
          <p className="pop-name">
            {cardName ? (
              <>
                <CardIcon />
                {cardName}
              </>
            ) : (
              "BackupTool"
            )}
          </p>
          <p className="pop-status">
            {/* What sits below already says the rest: don't say it twice. */}
            {running ? (
              <strong>{status.title}</strong>
            ) : done || !status.detail ? (
              (status.detail ?? status.title)
            ) : (
              <>
                {status.detail} <strong>{status.title}</strong>
              </>
            )}
          </p>
        </div>
      </div>

      {running ? (
        <div className="pop-progress">
          <p className="progress-line">
            {progress ? (
              <span>
                {progress.phase === "copying" ? "Copia" : "Verifica"} del file {progress.index + 1}{" "}
                di {inspection?.files ?? "…"}, {bytes(progress.bytesDone)} di{" "}
                {bytes(progress.bytesTotal)}
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
          <p className="pop-note">
            <span>
              {Number.isFinite(timeLeft) ? `${remaining(timeLeft)}. ` : ""}
              Puoi chiudere questa finestra: la copia continua.
            </span>
            <button className="btn" onClick={stop}>
              Ferma
            </button>
          </p>
        </div>
      ) : !source ? (
        <p className="pop-note">
          Inserisci una scheda: comparirà qui. Per copiare una cartella apri la finestra.
        </p>
      ) : destinations.length === 0 ? (
        <div className="pop-actions">
          {ejectButton(false)}
          <button className="btn primary" onClick={chooseDestination}>
            Scegli dove copiare…
          </button>
        </div>
      ) : done ? (
        <div className="pop-safe">
          <span className="ok">Puoi formattare</span>
          {report && (
            <button className="link" onClick={() => api.openPath(report)}>
              Apri il report
            </button>
          )}
          {ejectButton(true)}
        </div>
      ) : (
        <div className="pop-actions">
          {ejectButton(false)}
          {!shoot && (
            <button className="btn" onClick={newShoot}>
              Nuovo shoot…
            </button>
          )}
          <button className="btn primary" disabled={!canStart} onClick={start}>
            {startLabel}
          </button>
        </div>
      )}
      {ejectError && <p className="warn">{ejectError}</p>}
      {failures.length > 0 && !running && (
        <p className="warn">
          {files(failures.length)} non verificati.{" "}
          <button className="link" onClick={() => api.openMain()}>
            Vedi quali
          </button>
        </p>
      )}

      {(destinations.length > 0 || shoot) && (
        <section className="pop-sec" aria-labelledby="pop-drives-heading">
          <h2 className="label" id="pop-drives-heading">
            Destinazioni
          </h2>
          <ul>
            {destinations.map((dest) => {
              const state = inspection?.dests.find((d) => d.path === dest);
              const shown = state && !running ? destinationState(state, done) : null;
              return (
                <li className="drive" key={dest}>
                  <DriveIcon />
                  <span className="drive-name">{leaf(dest)}</span>
                  {shown && <span className={`chip tone-${shown.tone}`}>{shown.text}</span>}
                </li>
              );
            })}
            {shoot && (
              <li className="drive">
                <FolderIcon />
                <span className="drive-name">{shoot.dir.split("/").join(" › ")}</span>
              </li>
            )}
          </ul>
        </section>
      )}

      <footer className="pop-foot">
        <button className="link" onClick={() => api.openMain()}>
          Apri BackupTool
        </button>
        <button
          className="link quiet"
          disabled={running}
          title={running ? "Ferma la copia prima di uscire" : undefined}
          onClick={() => api.quit()}
        >
          Esci
        </button>
      </footer>
    </div>
  );
}
