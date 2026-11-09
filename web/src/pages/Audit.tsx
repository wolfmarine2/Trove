import { useState } from "react";
import { api } from "../api";
import { ErrorNote, Loading, useAsync } from "../components";
import { useI18n } from "../i18n";
import { formatTime } from "../util";

const PAGE = 100;

export function AuditPage() {
  const { t, lang } = useI18n();
  const [actor, setActor] = useState("");
  const [action, setAction] = useState("");
  const [offset, setOffset] = useState(0);
  const { data, error, loading } = useAsync(() => api.audit({ actor, action, limit: PAGE, offset }), [actor, action, offset]);

  return (
    <section className="page">
      <header className="page-head">
        <h1>{t("audit.title")}</h1>
      </header>
      <div className="filters">
        <input placeholder={t("audit.actor")} aria-label={t("audit.actor")} value={actor} onChange={(e) => { setOffset(0); setActor(e.target.value); }} />
        <input placeholder={t("audit.action")} aria-label={t("audit.action")} value={action} onChange={(e) => { setOffset(0); setAction(e.target.value); }} list="audit-actions" />
        <datalist id="audit-actions">
          {["auth.", "repository.", "asset.", "user.", "token.", "npm.", "pypi.", "cargo.", "settings.", "system."].map((a) => (
            <option key={a} value={a} />
          ))}
        </datalist>
      </div>
      {loading && !data && <Loading />}
      <ErrorNote error={error} />
      {data && data.items.length === 0 && <p className="empty">{t("audit.empty")}</p>}
      {data && data.items.length > 0 && (
        <>
          <div className="table-wrap">
            <table className="table audit-table">
              <thead>
                <tr>
                  <th>{t("audit.col.time")}</th>
                  <th>{t("audit.col.user")}</th>
                  <th>{t("audit.col.action")}</th>
                  <th>{t("audit.col.target")}</th>
                  <th>{t("audit.col.detail")}</th>
                  <th>{t("audit.col.ip")}</th>
                </tr>
              </thead>
              <tbody>
                {data.items.map((e) => (
                  <tr key={e.id} className={e.action.endsWith("_failed") ? "warn" : undefined}>
                    <td className="nowrap">{formatTime(e.ts, lang)}</td>
                    <td>{e.actor}</td>
                    <td><code>{e.action}</code></td>
                    <td className="break">{e.target}</td>
                    <td>{e.detail}</td>
                    <td>{e.client_ip}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {data.total > PAGE && (
            <div className="pager">
              <button className="btn btn-quiet" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - PAGE))}>{t("search.prev")}</button>
              <span>{offset + 1}–{Math.min(offset + PAGE, data.total)} / {data.total}</span>
              <button className="btn btn-quiet" disabled={offset + PAGE >= data.total} onClick={() => setOffset(offset + PAGE)}>{t("search.next")}</button>
            </div>
          )}
        </>
      )}
    </section>
  );
}
