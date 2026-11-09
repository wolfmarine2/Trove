import { useRef, useState } from "react";
import { uploadFile } from "../api";
import { Dialog, Field } from "../components";
import { useI18n } from "../i18n";
import { formatBytes } from "../util";

type Status = "pending" | "uploading" | "done" | "error";

interface Item {
  file: File;
  rel: string;
  status: Status;
  loaded: number;
  message: string;
}

const folderAttrs = { webkitdirectory: "", directory: "" } as Record<string, string>;

/** Upload files (or a whole folder) into a raw hosted repository. */
export function UploadDialog({
  repo,
  dir,
  open,
  onClose,
}: {
  repo: string;
  dir: string;
  open: boolean;
  onClose: (uploaded: boolean) => void;
}) {
  const { t } = useI18n();
  const [target, setTarget] = useState(dir);
  const [items, setItems] = useState<Item[]>([]);
  const [busy, setBusy] = useState(false);
  const [finished, setFinished] = useState(false);
  const filesRef = useRef<HTMLInputElement>(null);
  const folderRef = useRef<HTMLInputElement>(null);

  const pick = (list: FileList | null) => {
    if (!list) return;
    setFinished(false);
    setItems(
      Array.from(list).map((file) => ({
        file,
        rel: file.webkitRelativePath || file.name,
        status: "pending",
        loaded: 0,
        message: "",
      })),
    );
  };

  const update = (i: number, patch: Partial<Item>) =>
    setItems((prev) => prev.map((it, idx) => (idx === i ? { ...it, ...patch } : it)));

  const start = async () => {
    setBusy(true);
    const base = target.split("/").filter(Boolean).join("/");
    for (let i = 0; i < items.length; i++) {
      const it = items[i];
      const path = base ? `${base}/${it.rel}` : it.rel;
      update(i, { status: "uploading", loaded: 0 });
      try {
        const status = await uploadFile(repo, path, it.file, (loaded) => update(i, { loaded }));
        if (status === 200 || status === 201) update(i, { status: "done", loaded: it.file.size, message: t("upload.done") });
        else if (status === 409) update(i, { status: "error", message: t("upload.exists") });
        else if (status === 401 || status === 403) update(i, { status: "error", message: t("upload.denied") });
        else update(i, { status: "error", message: t("upload.failed", { status }) });
      } catch (e) {
        update(i, { status: "error", message: e instanceof Error ? e.message : String(e) });
      }
    }
    setBusy(false);
    setFinished(true);
  };

  const close = () => {
    const uploaded = items.some((i) => i.status === "done");
    setItems([]);
    setFinished(false);
    onClose(uploaded);
  };

  const ok = items.filter((i) => i.status === "done").length;

  return (
    <Dialog open={open} title={t("upload.title", { repo })} onClose={close}>
      <div className="form upload">
        <Field label={t("upload.dir")} help={t("upload.dirHelp")} htmlFor="up-dir">
          <input id="up-dir" className="mono" value={target} onChange={(e) => setTarget(e.target.value)} placeholder="ca/2026" disabled={busy} />
        </Field>
        <div className="actions">
          <button type="button" className="btn btn-quiet" onClick={() => filesRef.current?.click()} disabled={busy}>
            {t("upload.files")}
          </button>
          <button type="button" className="btn btn-quiet" onClick={() => folderRef.current?.click()} disabled={busy}>
            {t("upload.folder")}
          </button>
          <input ref={filesRef} type="file" multiple hidden onChange={(e) => pick(e.target.files)} />
          <input ref={folderRef} type="file" multiple hidden {...folderAttrs} onChange={(e) => pick(e.target.files)} />
        </div>
        {items.length === 0 ? (
          <p className="muted">{t("upload.none")}</p>
        ) : (
          <ul className="upload-list">
            {items.map((it, i) => (
              <li key={i} className={`upload-${it.status}`}>
                <div className="upload-row">
                  <code>{it.rel}</code>
                  <span className="muted">{formatBytes(it.file.size)}</span>
                </div>
                {it.status === "uploading" && (
                  <div className="meter" role="progressbar" aria-valuemin={0} aria-valuemax={it.file.size} aria-valuenow={it.loaded}>
                    <span style={{ width: `${it.file.size ? (it.loaded / it.file.size) * 100 : 100}%` }} />
                  </div>
                )}
                {it.message && <span className="upload-msg">{it.message}</span>}
              </li>
            ))}
          </ul>
        )}
        {finished && <p className="lede">{t("upload.summary", { ok, n: items.length })}</p>}
        <div className="actions">
          <button type="button" className="btn btn-quiet" onClick={close} disabled={busy}>
            {t("common.close")}
          </button>
          <button type="button" className="btn btn-primary" onClick={start} disabled={busy || items.length === 0 || finished}>
            {t("upload.start", { n: items.length })}
          </button>
        </div>
      </div>
    </Dialog>
  );
}
