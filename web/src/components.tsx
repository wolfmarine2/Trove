import { useEffect, useRef, useState, type ReactNode } from "react";
import type { Format, RepoKind } from "./api";
import { useI18n } from "./i18n";

export function FormatTag({ format }: { format: Format }) {
  return <span className={`format format-${format}`}>{format}</span>;
}

/** Small glyph encoding the repository type: solid box, arrow, stacked boxes. */
export function KindGlyph({ kind }: { kind: RepoKind }) {
  const common = { width: 14, height: 14, viewBox: "0 0 14 14", "aria-hidden": true } as const;
  if (kind === "hosted")
    return (
      <svg {...common}>
        <rect x="2" y="2" width="10" height="10" fill="currentColor" />
      </svg>
    );
  if (kind === "proxy")
    return (
      <svg {...common}>
        <rect x="1.5" y="3.5" width="7" height="7" fill="none" stroke="currentColor" strokeWidth="1.5" />
        <path d="M8 7h5M10.5 4.5 13 7l-2.5 2.5" fill="none" stroke="currentColor" strokeWidth="1.5" />
      </svg>
    );
  return (
    <svg {...common}>
      <rect x="1" y="5" width="8" height="8" fill="none" stroke="currentColor" strokeWidth="1.5" />
      <rect x="5" y="1" width="8" height="8" fill="currentColor" />
    </svg>
  );
}

export function KindLabel({ kind }: { kind: RepoKind }) {
  const { t } = useI18n();
  return (
    <span className="kind" title={t(`kind.${kind}.help`)}>
      <KindGlyph kind={kind} />
      {t(`kind.${kind}`)}
    </span>
  );
}

export function CopyButton({ text, className = "" }: { text: string; className?: string }) {
  const { t } = useI18n();
  const [done, setDone] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const ta = document.createElement("textarea");
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      ta.remove();
    }
    setDone(true);
    setTimeout(() => setDone(false), 1500);
  };
  return (
    <button type="button" className={`btn btn-quiet btn-small ${className}`} onClick={copy} aria-live="polite">
      {done ? t("repo.copied") : t("repo.copy")}
    </button>
  );
}

export function Code({ file, code }: { file: string; code: string }) {
  return (
    <figure className="snippet">
      <figcaption>
        <span>{file}</span>
        <CopyButton text={code} />
      </figcaption>
      <pre>
        <code>{code}</code>
      </pre>
    </figure>
  );
}

export function Loading() {
  const { t } = useI18n();
  return <p className="muted pad">{t("common.loading")}</p>;
}

export function ErrorNote({ error }: { error: unknown }) {
  const { t } = useI18n();
  if (!error) return null;
  const msg = error instanceof Error ? error.message : String(error);
  return (
    <p className="note note-error" role="alert">
      {t("common.error", { msg })}
    </p>
  );
}

export function Notice({ children }: { children: ReactNode }) {
  return (
    <p className="note note-ok" role="status">
      {children}
    </p>
  );
}

export function Field({
  label,
  help,
  children,
  htmlFor,
}: {
  label: string;
  help?: string;
  children: ReactNode;
  htmlFor?: string;
}) {
  return (
    <div className="field">
      <label htmlFor={htmlFor}>{label}</label>
      {children}
      {help && <p className="help">{help}</p>}
    </div>
  );
}

/** Accessible modal dialog built on <dialog>. */
export function Dialog({
  open,
  title,
  onClose,
  children,
}: {
  open: boolean;
  title: string;
  onClose: () => void;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) d.showModal();
    if (!open && d.open) d.close();
  }, [open]);
  return (
    <dialog ref={ref} className="dialog" onClose={onClose} onCancel={onClose}>
      <h2>{title}</h2>
      {children}
    </dialog>
  );
}

export function useConfirm() {
  const { t } = useI18n();
  const [state, setState] = useState<{ message: string; resolve: (v: boolean) => void } | null>(null);
  const confirm = (message: string) => new Promise<boolean>((resolve) => setState({ message, resolve }));
  const close = (v: boolean) => {
    state?.resolve(v);
    setState(null);
  };
  const element = (
    <Dialog open={!!state} title={t("common.confirm")} onClose={() => close(false)}>
      <p>{state?.message}</p>
      <div className="actions">
        <button className="btn btn-quiet" onClick={() => close(false)}>
          {t("common.cancel")}
        </button>
        <button className="btn btn-danger" onClick={() => close(true)} autoFocus>
          {t("common.confirm")}
        </button>
      </div>
    </Dialog>
  );
  return { confirm, element };
}

/** Minimal async data hook. */
export function useAsync<T>(fn: () => Promise<T>, deps: unknown[]) {
  const [data, setData] = useState<T | undefined>();
  const [error, setError] = useState<unknown>();
  const [loading, setLoading] = useState(true);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError(undefined);
    fn()
      .then((d) => alive && setData(d))
      .catch((e) => alive && setError(e))
      .finally(() => alive && setLoading(false));
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, tick]);
  return { data, error, loading, reload: () => setTick((x) => x + 1), setData };
}
