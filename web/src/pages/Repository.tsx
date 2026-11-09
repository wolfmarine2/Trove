import { useState } from "react";
import { Link, NavLink, useNavigate, useParams } from "react-router-dom";
import { api, type Asset, type Repository } from "../api";
import { Code, CopyButton, ErrorNote, FormatTag, KindGlyph, KindLabel, Loading, Notice, useAsync, useConfirm } from "../components";
import { useI18n } from "../i18n";
import { useSession } from "../session";
import { clientSnippets, displayUrl, formatBytes, formatTime, relativeTime } from "../util";
import { RepoForm } from "./RepoForm";

export function RepositoryPage() {
  const params = useParams();
  const name = params.name!;
  const rest = params["*"] ?? "";
  const { t } = useI18n();
  const { user } = useSession();
  const { data: repo, error, loading, reload } = useAsync(() => api.repo(name), [name]);
  const isAdmin = user?.role === "admin";

  if (loading && !repo) return <Loading />;
  if (error || !repo) return <section className="page"><ErrorNote error={error ?? t("common.notFound")} /></section>;

  const section = rest.startsWith("browse") ? "browse" : rest.startsWith("settings") ? "settings" : "setup";
  const browsePath = rest.startsWith("browse") ? rest.slice("browse".length).replace(/^\/+/, "") : "";

  return (
    <section className="page">
      <header className="repo-head">
        <p className="crumbs">
          <Link to="/">{t("nav.repositories")}</Link>
        </p>
        <div className="repo-title">
          <h1>{repo.name}</h1>
          <FormatTag format={repo.format} />
          <KindLabel kind={repo.kind} />
          {!repo.online && <span className="badge badge-off">{t("repos.offline")}</span>}
        </div>
        {repo.description && <p className="lede">{repo.description}</p>}
        <div className={`repo-url rule-${repo.format}`}>
          <code>{displayUrl(repo)}</code>
          <CopyButton text={displayUrl(repo)} />
        </div>
        <RepoFacts repo={repo} />
      </header>

      <nav className="tabs" aria-label={repo.name}>
        <NavLink to={`/repositories/${repo.name}`} end>{t("repo.tab.setup")}</NavLink>
        <NavLink to={`/repositories/${repo.name}/browse`}>{t("repo.tab.browse")}</NavLink>
        {isAdmin && <NavLink to={`/repositories/${repo.name}/settings`}>{t("repo.tab.settings")}</NavLink>}
      </nav>

      {section === "setup" && <Setup repo={repo} />}
      {section === "browse" && <Browse repo={repo} path={browsePath} />}
      {section === "settings" && isAdmin && <Settings repo={repo} onSaved={reload} />}
    </section>
  );
}

function RepoFacts({ repo }: { repo: Repository }) {
  const { t } = useI18n();
  if (repo.kind === "proxy" && repo.config.proxy) {
    const ob = repo.config.proxy.outbound;
    return (
      <div className="facts">
        <span>
          {t("repo.remote")}: <code>{repo.config.proxy.remote_url}</code>
        </span>
        {ob && ob.mode !== "inherit" && (
          <span>
            {t("outbound.via")}: <code>{ob.mode === "direct" ? t("outbound.viaDirect") : ob.url}</code>
          </span>
        )}
      </div>
    );
  }
  if (repo.kind === "group")
    return (
      <div className="facts">
        <span>{t("repo.members")}:</span>
        <ol className="members-inline">
          {(repo.config.group?.members ?? []).map((m, i) => (
            <li key={m}>
              {i > 0 && <span className="order" aria-hidden="true">→</span>}
              <Link to={`/repositories/${m}`}>{m}</Link>
            </li>
          ))}
        </ol>
      </div>
    );
  return (
    <p className="facts">
      {t("repo.hostedFacts", { files: repo.asset_count.toLocaleString(), size: formatBytes(repo.total_size) })}
    </p>
  );
}

function Setup({ repo }: { repo: Repository }) {
  const { t } = useI18n();
  const { anonymousRead } = useSession();
  const snippets = clientSnippets(repo, anonymousRead);
  const needsToken = snippets.some((s) => s.code.includes("<TOKEN>"));
  return (
    <div className="setup">
      {snippets.map((s, i) => (
        <Code key={i} file={s.file} code={s.code} />
      ))}
      {repo.format === "docker" && (
        <p className="help">
          {t("repo.dockerNote", { registry: repo.url.replace(/^https?:\/\//, "").replace(/\/repository\/.*$/, ""), name: repo.name })}
        </p>
      )}
      {(repo.format === "apt" || repo.format === "yum") && <p className="help">{t("repo.osNote")}</p>}
      {needsToken && <p className="help">{t(anonymousRead ? "repo.tokenNotePublish" : "repo.tokenNote")}</p>}
      {anonymousRead && !needsToken && <p className="help">{t("repo.anonymousNote")}</p>}
    </div>
  );
}

function Browse({ repo, path }: { repo: Repository; path: string }) {
  const { t, lang } = useI18n();
  const { user } = useSession();
  const { data, error, loading, reload } = useAsync(() => api.browse(repo.name, path), [repo.name, path]);
  const [selected, setSelected] = useState<Asset | null>(null);
  const { confirm, element } = useConfirm();
  const segments = path ? path.split("/") : [];
  const base = `/repositories/${repo.name}/browse`;

  const remove = async (a: Asset) => {
    if (!(await confirm(t("asset.deleteConfirm", { path: a.path })))) return;
    await api.deleteAsset(a.id);
    setSelected(null);
    reload();
  };

  return (
    <div className="browse">
      <nav className="path" aria-label="Path">
        <Link to={base}>{t("browse.root")}</Link>
        {segments.map((seg, i) => (
          <span key={i}>
            <span className="sep">/</span>
            <Link to={`${base}/${segments.slice(0, i + 1).join("/")}`}>{seg}</Link>
          </span>
        ))}
      </nav>
      <ErrorNote error={error} />
      <div className="browse-grid">
        <div className="table-wrap">
          {loading && !data && <Loading />}
          {data && data.entries.length === 0 && (
            <p className="empty">{path ? t("browse.empty") : t("browse.emptyRepo")}</p>
          )}
          {data && data.entries.length > 0 && (
            <table className="table browse-table">
              <thead>
                <tr>
                  <th>{t("browse.col.name")}</th>
                  <th className="num">{t("browse.col.size")}</th>
                  <th>{t("browse.col.updated")}</th>
                </tr>
              </thead>
              <tbody>
                {path && (
                  <tr>
                    <td colSpan={3}>
                      <Link to={segments.length > 1 ? `${base}/${segments.slice(0, -1).join("/")}` : base} className="entry dir">
                        ..
                      </Link>
                    </td>
                  </tr>
                )}
                {data.entries.map((e) => (
                  <tr key={e.path} className={selected?.path === e.path ? "selected" : undefined}>
                    <td>
                      {e.is_dir ? (
                        <Link to={`${base}/${e.path}`} className="entry dir">
                          {e.name}/
                        </Link>
                      ) : (
                        <button className="entry file" onClick={() => setSelected(e.asset ?? null)} aria-pressed={selected?.path === e.path}>
                          {e.name}
                        </button>
                      )}
                    </td>
                    <td className="num">{formatBytes(e.size)}</td>
                    <td title={formatTime(e.updated_at, lang)}>{relativeTime(e.updated_at, lang)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
        <aside className="asset-panel" aria-live="polite">
          {selected ? (
            <AssetDetails
              asset={selected}
              repo={repo}
              canDelete={user?.role === "admin" && repo.kind !== "group"}
              onDelete={() => remove(selected)}
              onClose={() => setSelected(null)}
            />
          ) : (
            <p className="muted">{t("browse.select")}</p>
          )}
        </aside>
      </div>
      {element}
    </div>
  );
}

function AssetDetails({
  asset,
  repo,
  canDelete,
  onDelete,
  onClose,
}: {
  asset: Asset;
  repo: Repository;
  canDelete: boolean;
  onDelete: () => void;
  onClose: () => void;
}) {
  const { t, lang } = useI18n();
  const url = `${repo.url}/${asset.path.split("/").map(encodeURIComponent).join("/")}`;
  const upstream = typeof asset.attributes?.upstream_url === "string" ? asset.attributes.upstream_url : null;
  const sums: [string, string][] = [
    ["SHA-256", asset.blob.sha256],
    ["SHA-1", asset.blob.sha1],
    ["MD5", asset.blob.md5],
    ["SHA-512", asset.blob.sha512],
  ];
  return (
    <div className="asset">
      <div className="asset-head">
        <h2>{asset.path.split("/").pop()}</h2>
        <button className="btn btn-quiet btn-small" onClick={onClose} aria-label={t("common.close")}>
          ×
        </button>
      </div>
      <p className="asset-path"><code>{asset.path}</code></p>
      <div className="actions">
        <a className="btn btn-primary btn-small" href={url} download>
          {t("asset.download")}
        </a>
        {canDelete && (
          <button className="btn btn-danger btn-small" onClick={onDelete}>
            {t("asset.delete")}
          </button>
        )}
      </div>
      <dl className="facts-list">
        <dt>{t("asset.size")}</dt>
        <dd>{formatBytes(asset.blob.size)} ({asset.blob.size.toLocaleString()} B)</dd>
        <dt>{t("asset.contentType")}</dt>
        <dd>{asset.content_type}</dd>
        <dt>{t("asset.uploader")}</dt>
        <dd>{asset.uploader || "—"}</dd>
        <dt>{t("asset.created")}</dt>
        <dd>{formatTime(asset.created_at, lang)}</dd>
        <dt>{t("asset.updated")}</dt>
        <dd>{formatTime(asset.updated_at, lang)}</dd>
        <dt>{t("asset.lastDownloaded")}</dt>
        <dd>{asset.last_downloaded_at ? formatTime(asset.last_downloaded_at, lang) : t("asset.never")}</dd>
        {upstream && (
          <>
            <dt>{t("asset.upstream")}</dt>
            <dd className="break"><code>{upstream}</code></dd>
          </>
        )}
      </dl>
      <dl className="hashes">
        {sums.map(([k, v]) => (
          <div key={k}>
            <dt>{k}</dt>
            <dd>
              <code>{v}</code>
              <CopyButton text={v} />
            </dd>
          </div>
        ))}
      </dl>
    </div>
  );
}

function Settings({ repo, onSaved }: { repo: Repository; onSaved: () => void }) {
  const { t } = useI18n();
  const navigate = useNavigate();
  const { confirm, element } = useConfirm();
  const [message, setMessage] = useState("");
  const [error, setError] = useState<unknown>();

  const invalidate = async () => {
    try {
      await api.invalidateCache(repo.name);
      setMessage(t("repo.invalidated"));
    } catch (e) {
      setError(e);
    }
  };
  const remove = async () => {
    if (!(await confirm(t("repo.deleteConfirm", { name: repo.name, count: repo.asset_count })))) return;
    try {
      await api.deleteRepo(repo.name);
      navigate("/");
    } catch (e) {
      setError(e);
    }
  };

  return (
    <div className="settings">
      <RepoForm existing={repo} onSaved={onSaved} />
      <section className="danger-zone">
        {message && <Notice>{message}</Notice>}
        <ErrorNote error={error} />
        <div className="actions">
          {repo.kind === "proxy" && (
            <button className="btn btn-quiet" onClick={invalidate}>
              <KindGlyph kind="proxy" /> {t("repo.invalidate")}
            </button>
          )}
          <button className="btn btn-danger" onClick={remove}>
            {t("repo.delete")}
          </button>
        </div>
      </section>
      {element}
    </div>
  );
}
