import { useEffect, useRef, useState } from "react";
import { api, type ProxyCodec, type ProxySize, type VideoFile } from "./api";
import { bytes, leaf } from "./format";
import type { ProxySettings } from "./session";

const CODECS: [ProxyCodec, string, string][] = [
  ["prores_proxy", "ProRes 422 Proxy", ".mov, 10 bit: il proxy classico per montare"],
  ["prores_lt", "ProRes 422 LT", ".mov, 10 bit: più pesante, più vicino all'originale"],
  ["h264", "H.264", ".mp4, 8 bit: i file più piccoli, si aprono ovunque"],
  ["h265", "H.265", ".mp4, 10 bit: piccoli e con il colore a 10 bit"],
  ["dnxhr_lb", "DNxHR LB", ".mov: per Avid"],
];

const SIZES: [ProxySize, string][] = [
  ["hd", "1080p"],
  ["half", "Metà dell'originale"],
  ["quarter", "Un quarto dell'originale"],
  ["original", "Come l'originale"],
];

type Props = {
  source: string | null;
  /** Name of the first destination, where proxies are made. */
  workingDrive: string | null;
  settings: ProxySettings;
  onSettings: (next: ProxySettings) => void;
  /** Source paths of the clips left out. */
  excluded: string[];
  onExcluded: (next: string[]) => void;
  onClose: () => void;
};

/** Format of the proxies, and which clips of this card get one. */
export default function ProxyDialog({
  source,
  workingDrive,
  settings,
  onSettings,
  excluded,
  onExcluded,
  onClose,
}: Props) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [videos, setVideos] = useState<VideoFile[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (dialog.current && !dialog.current.open) dialog.current.showModal();
  }, []);

  useEffect(() => {
    if (!source) {
      setVideos([]);
      return;
    }
    let alive = true;
    api.listVideos(source).then(
      (found) => alive && setVideos(found),
      (err) => alive && (setVideos([]), setError(String(err))),
    );
    return () => {
      alive = false;
    };
  }, [source]);

  const usable = (videos ?? []).filter((video) => video.convertible);
  const chosen = usable.filter((video) => !excluded.includes(video.rel_path));
  const raw = (videos ?? []).length - usable.length;
  const codec = CODECS.find(([value]) => value === settings.codec);

  const toggle = (path: string) =>
    onExcluded(excluded.includes(path) ? excluded.filter((p) => p !== path) : [...excluded, path]);

  return (
    <dialog
      ref={dialog}
      className="shoot-dialog"
      aria-labelledby="proxy-title"
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
    >
      <h2 id="proxy-title">Proxy</h2>
      <p className="dialog-lead">
        Dopo la copia, di ogni clip viene fatta una versione leggera in una cartella Proxy accanto
        agli originali{workingDrive ? `, su ${workingDrive}` : ""}. DaVinci Resolve li aggancia da
        solo.
      </p>

      <div className="fields">
        <div className="field">
          <label htmlFor="proxy-codec">Formato</label>
          <select
            id="proxy-codec"
            value={settings.codec}
            onChange={(e) => onSettings({ ...settings, codec: e.target.value as ProxyCodec })}
          >
            {CODECS.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
          <p className="field-note">{codec?.[2]}</p>
        </div>
        <div className="field">
          <label htmlFor="proxy-size">Dimensione</label>
          <select
            id="proxy-size"
            value={settings.size}
            onChange={(e) => onSettings({ ...settings, size: e.target.value as ProxySize })}
          >
            {SIZES.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
          <p className="field-note">Timecode, nome e audio restano quelli dell'originale.</p>
        </div>
      </div>

      <div className="clip-head">
        <span>
          {videos === null
            ? "Lettura dei clip…"
            : usable.length === 0
              ? "Nessun clip da convertire su questa scheda."
              : `${chosen.length} di ${usable.length} clip`}
        </span>
        {usable.length > 0 && (
          <span className="shoot-actions">
            <button type="button" className="link" onClick={() => onExcluded([])}>
              Tutti
            </button>
            <button
              type="button"
              className="link"
              onClick={() => onExcluded(usable.map((video) => video.rel_path))}
            >
              Nessuno
            </button>
          </span>
        )}
      </div>
      <ul className="tree clips">
        {(videos ?? []).map((video) => (
          <li key={video.rel_path}>
            <label className={video.convertible ? "clip" : "clip clip-off"}>
              <input
                type="checkbox"
                disabled={!video.convertible}
                checked={video.convertible && !excluded.includes(video.rel_path)}
                onChange={() => toggle(video.rel_path)}
              />
              <span className="clip-name" title={video.rel_path}>
                {leaf(video.rel_path)}
              </span>
              <span className="folder-state">
                {video.convertible ? bytes(video.size) : "raw, non convertibile"}
              </span>
            </label>
          </li>
        ))}
      </ul>
      {raw > 0 && (
        <p className="field-note">
          I clip in formato raw della camera (BRAW, R3D, ARRIRAW, Cinema RAW Light, N-RAW) non si
          possono convertire qui: i loro proxy vanno fatti con il programma del produttore.
        </p>
      )}
      {error && <p className="dialog-warning">{error}</p>}

      <div className="dialog-actions">
        <button type="button" className="btn primary" onClick={onClose}>
          Fatto
        </button>
      </div>
    </dialog>
  );
}
