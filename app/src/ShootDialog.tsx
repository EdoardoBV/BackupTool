import { type FormEvent, type KeyboardEvent, useEffect, useRef, useState } from "react";
import {
  api,
  type CardInfo,
  type Client,
  type DateFormat,
  type ShootLayout,
  type ShootPreview,
} from "./api";
import { cameraName, files, leaf, list } from "./format";
import { FolderIcon } from "./ui";

type Props = {
  source: string;
  destinations: string[];
  card: CardInfo | null;
  /** The last shoot created, to continue it with another card. */
  previous: ShootLayout | null;
  onCancel: () => void;
  onCreated: (layout: ShootLayout, dir: string) => void;
};

/** Local calendar day as YYYY-MM-DD; cards record local time. */
function isoDay(seconds: number | null): string {
  const day = seconds ? new Date(seconds * 1000) : new Date();
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${day.getFullYear()}-${pad(day.getMonth() + 1)}-${pad(day.getDate())}`;
}

function savedDateFormat(): DateFormat {
  try {
    return localStorage.getItem("dateFormat") === "year_month_day"
      ? "year_month_day"
      : "day_month_year";
  } catch {
    return "day_month_year";
  }
}

/** What a remembered camera name is filed under. */
function cameraKey(card: CardInfo | null): string | null {
  const camera = card?.camera;
  if (!camera) return null;
  return camera.serial ?? `${camera.make ?? ""} ${camera.model}`.trim();
}

export default function ShootDialog({
  source,
  destinations,
  card,
  previous,
  onCancel,
  onCreated,
}: Props) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [clients, setClients] = useState<Client[]>([]);
  const [client, setClient] = useState(previous?.client ?? "");
  const [suggesting, setSuggesting] = useState(false);
  const [active, setActive] = useState(-1);
  const [shoot, setShoot] = useState(previous?.shoot ?? "");
  const [date, setDate] = useState(() => isoDay(card?.shot_at ?? null));
  const [dateFormat, setDateFormat] = useState<DateFormat>(
    () => previous?.date_format ?? savedDateFormat(),
  );
  const [camera, setCamera] = useState(card?.camera?.model ?? "");
  const cameraEdited = useRef(false);
  const [reel, setReel] = useState("");
  const [keepFolders, setKeepFolders] = useState(card?.structured ?? false);
  const [preview, setPreview] = useState<ShootPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const key = cameraKey(card);
  const layout: ShootLayout = {
    client,
    shoot,
    date,
    date_format: dateFormat,
    camera,
    reel,
    keep_card_folders: keepFolders,
  };

  useEffect(() => {
    if (dialog.current && !dialog.current.open) dialog.current.showModal();
  }, []);

  useEffect(() => {
    let alive = true;
    api.listClients(destinations[0]).then((found) => alive && setClients(found), () => {});
    api.cameraNames().then(
      (names) => {
        // A name the user gave this camera before wins over the model name.
        if (alive && key && names[key] && !cameraEdited.current) setCamera(names[key]);
      },
      () => {},
    );
    return () => {
      alive = false;
    };
  }, [destinations, key]);

  const signature = JSON.stringify(layout);
  useEffect(() => {
    if (!client.trim() || !date) {
      setPreview(null);
      return;
    }
    let alive = true;
    // Wait for a pause in typing before asking the drive.
    const timer = window.setTimeout(() => {
      api.previewShoot(source, destinations[0], JSON.parse(signature)).then(
        (found) => alive && (setPreview(found), setError(null)),
        (err) => alive && (setPreview(null), setError(String(err))),
      );
    }, 150);
    return () => {
      alive = false;
      window.clearTimeout(timer);
    };
  }, [source, destinations, signature, client, date]);

  const query = client.trim().toLowerCase();
  const matches = clients.filter((c) => c.name.toLowerCase().includes(query));
  const exact = clients.find((c) => c.name.toLowerCase() === query);
  const showList = suggesting && matches.length > 0 && !(matches.length === 1 && exact);

  const pick = (name: string) => {
    setClient(name);
    setSuggesting(false);
    setActive(-1);
  };

  const onClientKey = (event: KeyboardEvent<HTMLInputElement>) => {
    if (!showList) return;
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const step = event.key === "ArrowDown" ? 1 : -1;
      setActive((active + step + matches.length) % matches.length);
    } else if (event.key === "Enter" && active >= 0) {
      event.preventDefault();
      pick(matches[active].name);
    } else if (event.key === "Escape") {
      // Close the list, not the whole dialog.
      event.preventDefault();
      setSuggesting(false);
    }
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    try {
      const dir = await api.createShoot(source, destinations, layout);
      if (key && camera.trim()) await api.rememberCamera(key, camera.trim());
      try {
        localStorage.setItem("dateFormat", dateFormat);
      } catch {
        // Remembering the format is a convenience only.
      }
      onCreated(layout, dir);
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  };

  const detected = card?.camera
    ? cameraName(card.camera) +
      (card.camera.serial ? `, seriale …${card.camera.serial.slice(-4)}` : "")
    : "Non riconosciuta: scrivi tu il nome";

  return (
    <dialog
      ref={dialog}
      className="shoot-dialog"
      aria-labelledby="shoot-title"
      onCancel={(event) => {
        event.preventDefault();
        onCancel();
      }}
    >
      <form onSubmit={submit}>
        <h2 id="shoot-title">Nuovo shoot</h2>
        <p className="dialog-lead">Crea le cartelle su {list(destinations.map(leaf))}.</p>

        <div className="fields">
          <div className="field field-client">
            <label htmlFor="shoot-client">Cliente</label>
            <input
              id="shoot-client"
              value={client}
              autoFocus
              autoComplete="off"
              role="combobox"
              aria-expanded={showList}
              aria-controls="client-list"
              aria-autocomplete="list"
              onChange={(e) => (setClient(e.target.value), setSuggesting(true), setActive(-1))}
              onFocus={() => setSuggesting(true)}
              onBlur={() => setSuggesting(false)}
              onKeyDown={onClientKey}
            />
            {showList && (
              <ul id="client-list" className="suggestions" role="listbox">
                {matches.map((c, i) => (
                  <li
                    key={c.name}
                    role="option"
                    aria-selected={i === active}
                    // Before the input loses focus, or the list is gone.
                    onMouseDown={(e) => (e.preventDefault(), pick(c.name))}
                  >
                    <span>{c.name}</span>
                    <span className="suggestion-years">{c.years.join(", ")}</span>
                  </li>
                ))}
              </ul>
            )}
            <p className="field-note">
              {exact
                ? `Si aggiunge alla cartella ${exact.name}, che c'è già.`
                : query
                  ? "Cliente nuovo: nasce una cartella."
                  : "Scrivi per cercare tra i clienti già sul disco."}
            </p>
          </div>

          <div className="field">
            <label htmlFor="shoot-name">Nome dello shoot</label>
            <input
              id="shoot-name"
              value={shoot}
              autoComplete="off"
              onChange={(e) => setShoot(e.target.value)}
            />
            <div className="field-row">
              <label htmlFor="shoot-date">Data</label>
              <input
                id="shoot-date"
                type="date"
                required
                value={date}
                onChange={(e) => setDate(e.target.value)}
              />
              <select
                aria-label="Formato della data nel nome della cartella"
                value={dateFormat}
                onChange={(e) => setDateFormat(e.target.value as DateFormat)}
              >
                {/* Shown with the chosen day, so the folder name is what you see. */}
                <option value="day_month_year">
                  {date ? date.split("-").reverse().join(".").replace(/\d\d(\d\d)$/, "$1") : "GG.MM.AA"}
                </option>
                <option value="year_month_day">{date || "AAAA-MM-GG"}</option>
              </select>
            </div>
            <p className="field-note">La data viene dalla scheda.</p>
          </div>
        </div>

        <div className="tree" aria-label="Cartelle">
          {preview ? (
            <ul>
              {preview.folders.map((folder) => (
                <li
                  key={folder.path}
                  className={folder.exists ? undefined : "is-new"}
                  style={{ paddingLeft: `${(folder.path.split("/").length - 1) * 20}px` }}
                >
                  <FolderIcon />
                  <span className="folder">{leaf(folder.path)}</span>
                  <span className="folder-state">{folder.exists ? "c'è già" : "nuova"}</span>
                  {folder.files > 0 && (
                    <span className="folder-files">{files(folder.files)} vanno qui</span>
                  )}
                </li>
              ))}
            </ul>
          ) : (
            <p className="tree-empty">{error ?? "Scrivi il cliente per vedere le cartelle."}</p>
          )}
        </div>
        {card && card.video > 0 && card.photo === 0 && (
          <p className="field-note">Solo video, quindi niente cartella Photo.</p>
        )}
        {card && card.photo > 0 && card.video === 0 && (
          <p className="field-note">Solo foto, quindi niente cartella Footage.</p>
        )}
        {preview && preview.conflicts > 0 && (
          <p className="dialog-warning">
            Lì ci sono già {files(preview.conflicts)} con lo stesso nome ma diversi, forse di
            un'altra scheda. Dai un nome a questa scheda per tenerli separati.
          </p>
        )}

        <div className="fields">
          <div className="field">
            <label htmlFor="shoot-camera">Camera su questa scheda</label>
            <input
              id="shoot-camera"
              value={camera}
              autoComplete="off"
              onChange={(e) => ((cameraEdited.current = true), setCamera(e.target.value))}
            />
            <p className="field-note">{detected}</p>
          </div>
          <div className="field">
            <label htmlFor="shoot-reel">Nome della scheda, se serve</label>
            <input
              id="shoot-reel"
              value={reel}
              autoComplete="off"
              placeholder="A002, Scheda 2…"
              onChange={(e) => setReel(e.target.value)}
            />
            <label className="switch">
              <input
                type="checkbox"
                checked={keepFolders}
                onChange={(e) => setKeepFolders(e.target.checked)}
              />
              Mantieni le cartelle della scheda
            </label>
          </div>
        </div>

        <div className="dialog-actions">
          <button type="button" className="btn" onClick={onCancel}>
            Annulla
          </button>
          <button type="submit" className="btn primary" disabled={busy || !client.trim() || !preview}>
            Crea cartelle
          </button>
        </div>
      </form>
    </dialog>
  );
}
