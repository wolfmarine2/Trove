import { useEffect, useState, type FormEvent } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { api, type Asset } from "../api";
import { ErrorNote, Loading, useAsync } from "../components";
import { useI18n } from "../i18n";
import { formatBytes, relativeTime } from "../util";

const PAGE = 50;

export function SearchPage() {
  const { t, lang } = useI18n();
  const [params, setParams] = useSearchParams();
  const q = params.get("q") ?? "";
  const repo = params.get("repo") ?? "";
  const offset = Number(params.get("offset") ?? 0);
  const [draft, setDraft] = useState(q);
  useEffect(() => setDraft(q), [q]);
  const { data: repos } = useAsync(() => api.repos(), []);
  const [result, setResult] = useState<{ items: Asset[]; total: number }>();
  const [error, setError] = useState<unknown>();
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!q) {
      setResult(undefined);
      return;
    }
    let alive = true;
    setLoading(true);
    api
      .search({ q, repo, limit: PAGE, offset })
      .then((r) => alive && setResult(r))
      .catch((e) => alive && setError(e))
      .finally(() => alive && setLoading(false));
    return () => {
      alive = false;
    };
  }, [q, repo, offset]);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    setParams({ q: draft.trim(), ...(repo ? { repo } : {}) });
  };

  return (
    <section className="page">
      <header className="page-head">
        <h1>{t("search.title")}</h1>
      </header>
      <form className="search-bar" onSubmit={submit} role="search">
        <input
          type="search"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder={t("search.placeholder")}
          aria-label={t("search.title")}
          autoFocus
        />
        <select
          value={repo}
          onChange={(e) => setParams({ ...(q ? { q } : {}), ...(e.target.value ? { repo: e.target.value } : {}) })}
          aria-label={t("search.col.repo")}
        >
          <option value="">{t("search.allRepos")}</option>
          {(repos ?? []).map((r) => (
            <option key={r.id} value={r.name}>{r.name}</option>
          ))}
        </select>
        <button className="btn btn-primary">{t("search.title")}</button>
      </form>
      <ErrorNote error={error} />
      {loading && <Loading />}
      {result && q && (
        <>
          <p className="lede">{result.total ? t("search.results", { n: result.total }) : t("search.none", { q })}</p>
          {result.items.length > 0 && (
            <div className="table-wrap">
              <table className="table">
                <thead>
                  <tr>
                    <th>{t("search.col.path")}</th>
                    <th>{t("search.col.repo")}</th>
                    <th className="num">{t("browse.col.size")}</th>
                    <th>{t("browse.col.updated")}</th>
                  </tr>
                </thead>
                <tbody>
                  {result.items.map((a) => {
                    const dir = a.path.split("/").slice(0, -1).join("/");
                    return (
                      <tr key={a.id} className={`rule-${a.format}`}>
                        <td className="path-cell">
                          <a href={a.download_url}><code>{a.path}</code></a>
                        </td>
                        <td>
                          <Link to={`/repositories/${a.repository}/browse${dir ? `/${dir}` : ""}`}>{a.repository}</Link>
                        </td>
                        <td className="num">{formatBytes(a.blob.size)}</td>
                        <td>{relativeTime(a.updated_at, lang)}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
          {result.total > PAGE && (
            <div className="pager">
              <button className="btn btn-quiet" disabled={offset === 0} onClick={() => setParams({ q, ...(repo ? { repo } : {}), offset: String(Math.max(0, offset - PAGE)) })}>
                {t("search.prev")}
              </button>
              <span>{offset + 1}–{Math.min(offset + PAGE, result.total)} / {result.total}</span>
              <button className="btn btn-quiet" disabled={offset + PAGE >= result.total} onClick={() => setParams({ q, ...(repo ? { repo } : {}), offset: String(offset + PAGE) })}>
                {t("search.next")}
              </button>
            </div>
          )}
        </>
      )}
    </section>
  );
}
