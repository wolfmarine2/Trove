import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { api } from "../api";
import { CopyButton, ErrorNote, FormatTag, KindLabel, Loading, useAsync } from "../components";
import { useI18n } from "../i18n";
import { useSession } from "../session";
import { displayUrl, formatBytes } from "../util";

export function RepositoriesPage() {
  const { t } = useI18n();
  const { user } = useSession();
  const { data, error, loading } = useAsync(() => api.repos(), []);
  const [filter, setFilter] = useState("");
  const repos = useMemo(
    () => (data ?? []).filter((r) => r.name.toLowerCase().includes(filter.trim().toLowerCase())),
    [data, filter],
  );
  const totals = useMemo(
    () => (data ?? []).reduce((acc, r) => ({ files: acc.files + r.asset_count, size: acc.size + r.total_size }), { files: 0, size: 0 }),
    [data],
  );

  return (
    <section className="page">
      <header className="page-head">
        <div>
          <h1>{t("repos.title")}</h1>
          {data && data.length > 0 && (
            <p className="lede">{t("repos.summary", { repos: data.length, assets: totals.files, size: formatBytes(totals.size) })}</p>
          )}
        </div>
        {user?.role === "admin" && (
          <Link to="/repositories/new" className="btn btn-primary">{t("repos.create")}</Link>
        )}
      </header>
      {loading && <Loading />}
      <ErrorNote error={error} />
      {data && data.length === 0 && <p className="empty">{t("repos.empty")}</p>}
      {data && data.length > 0 && (
        <>
          <input
            className="filter"
            type="search"
            placeholder={t("repos.filter")}
            aria-label={t("repos.filter")}
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
          <div className="table-wrap">
            <table className="table repo-table">
              <thead>
                <tr>
                  <th>{t("repos.col.name")}</th>
                  <th>{t("repos.col.format")}</th>
                  <th>{t("repos.col.kind")}</th>
                  <th className="num">{t("repos.col.files")}</th>
                  <th className="num">{t("repos.col.size")}</th>
                  <th>{t("repos.col.url")}</th>
                </tr>
              </thead>
              <tbody>
                {repos.map((r) => (
                  <tr key={r.id} className={`rule-${r.format}`}>
                    <td>
                      <Link to={`/repositories/${r.name}`} className="repo-name">{r.name}</Link>
                      {!r.online && <span className="badge badge-off">{t("repos.offline")}</span>}
                      {r.config.private && <span className="badge badge-private">{t("repos.private")}</span>}
                      {r.description && <span className="desc">{r.description}</span>}
                    </td>
                    <td><FormatTag format={r.format} /></td>
                    <td><KindLabel kind={r.kind} /></td>
                    <td className="num">{r.asset_count.toLocaleString()}</td>
                    <td className="num">{formatBytes(r.total_size)}</td>
                    <td className="url-cell">
                      <code>{displayUrl(r)}</code>
                      <CopyButton text={displayUrl(r)} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </section>
  );
}
