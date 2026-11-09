import { useState } from "react";
import { api, type Settings } from "../api";
import { ErrorNote, Field, Loading, Notice, useAsync } from "../components";
import { useI18n } from "../i18n";
import { useSession } from "../session";
import { formatBytes, relativeTime } from "../util";

const COUNTERS = ["downloads", "uploads", "proxy_hits", "proxy_fetches", "upstream_errors"] as const;

export function SystemPage() {
  const { t, lang } = useI18n();
  const { refresh } = useSession();
  const settings = useAsync(() => api.settings(), []);
  const stats = useAsync(() => api.stats(), []);
  const [gcMessage, setGcMessage] = useState("");
  const [error, setError] = useState<unknown>();
  const [busy, setBusy] = useState(false);

  const toggleAnonymous = async (v: boolean) => {
    try {
      settings.setData(await api.updateSettings({ anonymous_read: v }));
      await refresh();
    } catch (e) {
      setError(e);
    }
  };

  const runGc = async () => {
    setBusy(true);
    try {
      const r = await api.gc();
      setGcMessage(t("system.gcDone", { n: r.deleted_blobs, size: formatBytes(r.freed_bytes) }));
      stats.reload();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  const s = stats.data;
  return (
    <section className="page narrow">
      <header className="page-head">
        <h1>{t("system.title")}</h1>
      </header>
      <ErrorNote error={error ?? settings.error ?? stats.error} />
      {(settings.loading || stats.loading) && !s && <Loading />}

      {settings.data && (
        <section className="subsection">
          <DockerDefault
            value={settings.data.docker_default_repository}
            onSaved={(s) => settings.setData(s)}
            onError={setError}
          />
        </section>
      )}

      {settings.data && (
        <section className="subsection">
          <label className="check switch">
            <input type="checkbox" checked={settings.data.anonymous_read} onChange={(e) => toggleAnonymous(e.target.checked)} />
            <span>
              {t("system.anonymous")}
              <span className="help">{t("system.anonymousHelp")}</span>
            </span>
          </label>
        </section>
      )}

      {s && (
        <section className="subsection">
          <h2>{t("system.storage")}</h2>
          <p>{t("system.dedupe", { logical: formatBytes(s.logical_bytes), stored: formatBytes(s.stored_bytes) })}</p>
          <div className="meter" role="img" aria-label={`${formatBytes(s.stored_bytes)} / ${formatBytes(s.logical_bytes)}`}>
            <span style={{ width: `${s.logical_bytes ? Math.min(100, (s.stored_bytes / s.logical_bytes) * 100) : 0}%` }} />
          </div>
          <dl className="facts-list">
            <dt>{t("system.storage")}</dt>
            <dd><code>{s.storage}</code></dd>
            <dt>{t("system.database")}</dt>
            <dd>{s.database}</dd>
          </dl>
          <div className="actions">
            <button className="btn btn-quiet" onClick={runGc} disabled={busy}>{t("system.gc")}</button>
          </div>
          <p className="help">{t("system.gcHelp")}</p>
          {gcMessage && <Notice>{gcMessage}</Notice>}
        </section>
      )}

      {s && (
        <section className="subsection">
          <h2>{t("system.counters")}</h2>
          <dl className="counters">
            {COUNTERS.map((c) => (
              <div key={c}>
                <dt>{t(`system.counter.${c}`)}</dt>
                <dd>{(s.counters[c] ?? 0).toLocaleString()}</dd>
              </div>
            ))}
          </dl>
        </section>
      )}

      {s && s.recent.length > 0 && (
        <section className="subsection">
          <h2>{t("system.recent")}</h2>
          <ul className="recent">
            {s.recent.map((r) => (
              <li key={r.id}>
                <a href={r.download_url}><code>{r.repository}/{r.path}</code></a>
                <span className="muted">{r.uploader}, {relativeTime(r.updated_at, lang)}, {formatBytes(r.size)}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </section>
  );
}

function DockerDefault({ value, onSaved, onError }: { value: string; onSaved: (s: Settings) => void; onError: (e: unknown) => void }) {
  const { t } = useI18n();
  const { data: repos } = useAsync(() => api.repos(), []);
  const [saved, setSaved] = useState(false);
  const docker = (repos ?? []).filter((r) => r.format === "docker");
  return (
    <>
      <Field label={t("system.dockerDefault")} help={t("system.dockerDefaultHelp")} htmlFor="docker-default">
        <select
          id="docker-default"
          value={value}
          onChange={async (e) => {
            setSaved(false);
            try {
              onSaved(await api.updateSettings({ docker_default_repository: e.target.value }));
              setSaved(true);
            } catch (err) {
              onError(err);
            }
          }}
        >
          <option value="">{t("system.none")}</option>
          {docker.map((r) => (
            <option key={r.id} value={r.name}>{r.name}</option>
          ))}
        </select>
      </Field>
      {saved && <Notice>{t("system.saved")}</Notice>}
    </>
  );
}
