import { useMemo, useState, type FormEvent } from "react";
import { useNavigate } from "react-router-dom";
import {
  api,
  FORMATS,
  GROUP_DEPLOYMENT_FORMATS,
  supportedKinds,
  type ContentDisposition,
  type Format,
  type OutboundMode,
  type RemoteTestResult,
  type RepoConfig,
  type RepoKind,
  type Repository,
  type VersionPolicy,
  type WritePolicy,
} from "../api";
import { ErrorNote, Field, FormatTag, KindGlyph, Notice, useAsync } from "../components";
import { useI18n } from "../i18n";

const DEFAULT_REMOTES: Record<Format, string> = {
  maven: "https://repo1.maven.org/maven2/",
  npm: "https://registry.npmjs.org/",
  pypi: "https://pypi.org/simple/",
  cargo: "https://index.crates.io/",
  docker: "https://registry-1.docker.io",
  apt: "http://archive.ubuntu.com/ubuntu/",
  yum: "https://repo.openeuler.org/openEuler-24.03-LTS/OS/x86_64/",
  git: "https://github.com/",
  skills: "https://lightmake.site/",
  raw: "",
};

export function RepoFormPage() {
  const { t } = useI18n();
  return (
    <section className="page">
      <header className="page-head">
        <h1>{t("repos.create")}</h1>
      </header>
      <RepoForm />
    </section>
  );
}

export function RepoForm({ existing, onSaved }: { existing?: Repository; onSaved?: () => void }) {
  const { t } = useI18n();
  const navigate = useNavigate();
  const editing = !!existing;
  const [name, setName] = useState(existing?.name ?? "");
  const [format, setFormat] = useState<Format>(existing?.format ?? "maven");
  const [kind, setKind] = useState<RepoKind>(existing?.kind ?? "hosted");
  const [description, setDescription] = useState(existing?.description ?? "");
  const [online, setOnline] = useState(existing?.online ?? true);
  const [isPrivate, setIsPrivate] = useState(existing?.config.private ?? false);
  const [disposition, setDisposition] = useState<ContentDisposition>(existing?.config.content_disposition ?? "attachment");
  const [writePolicy, setWritePolicy] = useState<WritePolicy>(existing?.config.write_policy ?? "allow_once");
  const [versionPolicy, setVersionPolicy] = useState<VersionPolicy>(existing?.config.version_policy ?? "release");
  const [remoteUrl, setRemoteUrl] = useState(existing?.config.proxy?.remote_url ?? DEFAULT_REMOTES.maven);
  const [maxAgeMin, setMaxAgeMin] = useState(Math.round((existing?.config.proxy?.metadata_max_age_secs ?? 1800) / 60));
  const [remoteUser, setRemoteUser] = useState(existing?.config.proxy?.username ?? "");
  const [remotePassword, setRemotePassword] = useState(existing?.config.proxy?.password ?? "");
  const [offline, setOffline] = useState(existing?.config.proxy?.offline ?? false);
  const [contentMaxAgeMin, setContentMaxAgeMin] = useState(Math.round((existing?.config.proxy?.content_max_age_secs ?? 0) / 60));
  const ob = existing?.config.proxy?.outbound;
  const [obMode, setObMode] = useState<OutboundMode>(ob?.mode ?? "inherit");
  const [obUrl, setObUrl] = useState(ob?.url ?? "");
  const [obUser, setObUser] = useState(ob?.username ?? "");
  const [obPassword, setObPassword] = useState(ob?.password ?? "");
  const [obNoProxy, setObNoProxy] = useState(ob?.no_proxy ?? "");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<RemoteTestResult | null>(null);
  const [members, setMembers] = useState<string[]>(existing?.config.group?.members ?? []);
  const [writable, setWritable] = useState(existing?.config.group?.writable_member ?? "");
  const [pick, setPick] = useState("");
  const [error, setError] = useState<unknown>();
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);

  const { data: allRepos } = useAsync(() => api.repos(), []);
  const candidates = useMemo(
    () => (allRepos ?? []).filter((r) => r.format === format && r.name !== name && !members.includes(r.name)),
    [allRepos, format, name, members],
  );

  const changeFormat = (f: Format) => {
    if (!editing && (remoteUrl === "" || Object.values(DEFAULT_REMOTES).includes(remoteUrl))) setRemoteUrl(DEFAULT_REMOTES[f]);
    setFormat(f);
    setMembers([]);
    if (!supportedKinds(f).includes(kind)) setKind(supportedKinds(f)[0]);
  };

  const move = (i: number, d: -1 | 1) => {
    const next = [...members];
    [next[i], next[i + d]] = [next[i + d], next[i]];
    setMembers(next);
  };

  const proxyConfig = () => ({
    remote_url: remoteUrl.trim(),
    metadata_max_age_secs: Math.max(1, maxAgeMin) * 60,
    username: remoteUser,
    password: remotePassword,
    offline,
    content_max_age_secs: format === "raw" ? Math.max(0, contentMaxAgeMin) * 60 : 0,
    outbound: { mode: obMode, url: obUrl.trim(), username: obUser, password: obPassword, no_proxy: obNoProxy },
  });

  const testConnection = async () => {
    setTesting(true);
    setTestResult(null);
    try {
      setTestResult(await api.testRemote({ format, proxy: proxyConfig(), name: existing?.name }));
    } catch (err) {
      setTestResult({ ok: false, url: remoteUrl, status: null, elapsed_ms: 0, message: err instanceof Error ? err.message : String(err) });
    } finally {
      setTesting(false);
    }
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    setSaved(false);
    const config: RepoConfig = { write_policy: writePolicy, private: isPrivate };
    if (format === "raw") config.content_disposition = disposition;
    if (format === "maven") config.version_policy = versionPolicy;
    if (kind === "proxy") config.proxy = proxyConfig();
    if (kind === "group")
      config.group = { members, writable_member: writable && members.includes(writable) ? writable : undefined };
    try {
      if (editing) {
        await api.updateRepo(existing!.name, { description, online, config });
        setSaved(true);
        onSaved?.();
      } else {
        const repo = await api.createRepo({ name: name.trim(), format, kind, description, online, config });
        navigate(`/repositories/${repo.name}`);
      }
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <form className="form" onSubmit={submit}>
      {!editing && (
        <>
          <fieldset className="choice">
            <legend>{t("form.format")}</legend>
            <div className="choice-row">
              {FORMATS.map((f) => (
                <label key={f} className={`choice-item ${format === f ? "on" : ""}`}>
                  <input type="radio" name="format" value={f} checked={format === f} onChange={() => changeFormat(f)} />
                  <FormatTag format={f} />
                </label>
              ))}
            </div>
          </fieldset>
          <fieldset className="choice">
            <legend>{t("form.kind")}</legend>
            <div className="choice-row">
              {supportedKinds(format).map((k) => (
                <label key={k} className={`choice-item choice-kind ${kind === k ? "on" : ""}`}>
                  <input type="radio" name="kind" value={k} checked={kind === k} onChange={() => setKind(k)} />
                  <span className="choice-title">
                    <KindGlyph kind={k} /> {t(`kind.${k}`)}
                  </span>
                  <span className="help">{t(`kind.${k}.help`)}</span>
                </label>
              ))}
            </div>
          </fieldset>
          <Field label={t("form.name")} help={t("form.nameHelp")} htmlFor="repo-name">
            <input
              id="repo-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              required
              pattern="[A-Za-z0-9][A-Za-z0-9._\-]{0,63}"
              placeholder={`${format}-${kind === "hosted" ? "releases" : kind === "proxy" ? "remote" : "public"}`}
            />
          </Field>
        </>
      )}

      <Field label={t("form.description")} htmlFor="repo-desc">
        <input id="repo-desc" value={description} onChange={(e) => setDescription(e.target.value)} />
      </Field>

      <label className="check">
        <input type="checkbox" checked={online} onChange={(e) => setOnline(e.target.checked)} />
        <span>
          {t("form.online")}
          <span className="help">{t("form.onlineHelp")}</span>
        </span>
      </label>

      <label className="check">
        <input type="checkbox" checked={isPrivate} onChange={(e) => setIsPrivate(e.target.checked)} />
        <span>
          {t("form.private")}
          <span className="help">{t("form.privateHelp")}</span>
        </span>
      </label>

      {format === "raw" && (
        <Field label={t("form.disposition")} htmlFor="cd">
          <select id="cd" value={disposition} onChange={(e) => setDisposition(e.target.value as ContentDisposition)}>
            <option value="attachment">{t("form.disposition.attachment")}</option>
            <option value="inline">{t("form.disposition.inline")}</option>
          </select>
        </Field>
      )}

      {kind === "hosted" && (
        <Field label={t("form.writePolicy")} htmlFor="wp">
          <select id="wp" value={writePolicy} onChange={(e) => setWritePolicy(e.target.value as WritePolicy)}>
            <option value="allow_once">{t("form.wp.allow_once")}</option>
            <option value="allow_redeploy">{t("form.wp.allow_redeploy")}</option>
            <option value="read_only">{t("form.wp.read_only")}</option>
          </select>
        </Field>
      )}

      {format === "maven" && kind !== "group" && (
        <Field label={t("form.versionPolicy")} htmlFor="vp">
          <select
            id="vp"
            value={versionPolicy}
            onChange={(e) => {
              const v = e.target.value as VersionPolicy;
              setVersionPolicy(v);
              if (v === "snapshot" && !editing) setWritePolicy("allow_redeploy");
            }}
          >
            <option value="release">{t("form.vp.release")}</option>
            <option value="snapshot">{t("form.vp.snapshot")}</option>
            <option value="mixed">{t("form.vp.mixed")}</option>
          </select>
        </Field>
      )}

      {kind === "proxy" && (
        <div className="form-group">
          <Field label={t("form.remoteUrl")} htmlFor="remote">
            <input
              id="remote"
              type="url"
              value={remoteUrl}
              onChange={(e) => setRemoteUrl(e.target.value)}
              required
              placeholder={DEFAULT_REMOTES[format] || "https://"}
              className="mono"
            />
          </Field>
          <Field
            label={t(format === "git" ? "form.maxAgeGit" : "form.maxAge")}
            help={t(format === "git" ? "form.maxAgeGitHelp" : "form.maxAgeHelp")}
            htmlFor="maxage"
          >
            <input id="maxage" type="number" min={1} value={maxAgeMin} onChange={(e) => setMaxAgeMin(Number(e.target.value))} />
          </Field>
          {format === "raw" && (
            <Field label={t("form.contentMaxAge")} help={t("form.contentMaxAgeHelp")} htmlFor="cmaxage">
              <input
                id="cmaxage"
                type="number"
                min={0}
                value={contentMaxAgeMin}
                onChange={(e) => setContentMaxAgeMin(Number(e.target.value))}
              />
            </Field>
          )}
          {(format === "git" || format === "skills") && <p className="help">{t(`form.remoteHint.${format}`)}</p>}
          <div className="two">
            <Field label={t("form.remoteUser")} htmlFor="ruser">
              <input id="ruser" value={remoteUser} onChange={(e) => setRemoteUser(e.target.value)} autoComplete="off" />
            </Field>
            <Field label={t("form.remotePassword")} htmlFor="rpass">
              <input id="rpass" type="password" value={remotePassword} onChange={(e) => setRemotePassword(e.target.value)} autoComplete="new-password" />
            </Field>
          </div>
          <label className="check">
            <input type="checkbox" checked={offline} onChange={(e) => setOffline(e.target.checked)} />
            <span>
              {t("form.offlineMode")}
              <span className="help">{t("form.offlineModeHelp")}</span>
            </span>
          </label>

          <fieldset className="choice outbound">
            <legend>{t("outbound.title")}</legend>
            <p className="help">{t("outbound.help")}</p>
            <div className="choice-row">
              {(["inherit", "direct", "custom"] as OutboundMode[]).map((m) => (
                <label key={m} className={`choice-item choice-kind ${obMode === m ? "on" : ""}`}>
                  <input type="radio" name="outbound" value={m} checked={obMode === m} onChange={() => setObMode(m)} />
                  <span className="choice-title">{t(`outbound.${m}`)}</span>
                  <span className="help">{t(`outbound.${m}Help`)}</span>
                </label>
              ))}
            </div>
          </fieldset>
          {obMode === "custom" && (
            <>
              <Field label={t("outbound.url")} help={t("outbound.urlHelp")} htmlFor="ob-url">
                <input
                  id="ob-url"
                  className="mono"
                  value={obUrl}
                  onChange={(e) => setObUrl(e.target.value)}
                  required
                  pattern="(https?|socks5h?)://.+"
                  placeholder="socks5h://proxy.corp:1080"
                />
              </Field>
              <div className="two">
                <Field label={t("outbound.username")} htmlFor="ob-user">
                  <input id="ob-user" value={obUser} onChange={(e) => setObUser(e.target.value)} autoComplete="off" />
                </Field>
                <Field label={t("outbound.password")} htmlFor="ob-pass">
                  <input id="ob-pass" type="password" value={obPassword} onChange={(e) => setObPassword(e.target.value)} autoComplete="new-password" />
                </Field>
              </div>
              <Field label={t("outbound.noProxy")} help={t("outbound.noProxyHelp")} htmlFor="ob-np">
                <input id="ob-np" className="mono" value={obNoProxy} onChange={(e) => setObNoProxy(e.target.value)} placeholder=".corp.example,10.0.0.0/8" />
              </Field>
            </>
          )}
          <div className="actions">
            <button type="button" className="btn btn-quiet" onClick={testConnection} disabled={testing || !remoteUrl}>
              {testing ? t("outbound.testing") : t("outbound.test")}
            </button>
          </div>
          {testResult && (
            <p className={`note ${testResult.ok ? "note-ok" : "note-error"}`} role="status">
              {testResult.ok
                ? t("outbound.ok", { ms: testResult.elapsed_ms, message: testResult.message })
                : t("outbound.fail", { message: testResult.message })}
              <br />
              <code>{testResult.url}</code>
            </p>
          )}
        </div>
      )}

      {kind === "group" && (
        <div className="form-group">
          <div>
            <p className="label">{t("form.members")}</p>
            <p className="help">{t("form.membersHelp")}</p>
          </div>
          <ol className="member-list">
            {members.map((m, i) => (
              <li key={m}>
                <span className="member-name">{m}</span>
                <span className="member-actions">
                  <button type="button" className="btn btn-quiet btn-small" disabled={i === 0} onClick={() => move(i, -1)} aria-label={t("form.up")}>↑</button>
                  <button type="button" className="btn btn-quiet btn-small" disabled={i === members.length - 1} onClick={() => move(i, 1)} aria-label={t("form.down")}>↓</button>
                  <button type="button" className="btn btn-quiet btn-small" onClick={() => setMembers(members.filter((x) => x !== m))}>{t("form.remove")}</button>
                </span>
              </li>
            ))}
          </ol>
          <div className="inline">
            <select value={pick} onChange={(e) => setPick(e.target.value)} aria-label={t("form.members")}>
              <option value="">—</option>
              {candidates.map((r) => (
                <option key={r.id} value={r.name}>
                  {r.name} ({t(`kind.${r.kind}`)})
                </option>
              ))}
            </select>
            <button
              type="button"
              className="btn btn-quiet"
              disabled={!pick}
              onClick={() => {
                setMembers([...members, pick]);
                setPick("");
              }}
            >
              {t("form.addMember")}
            </button>
          </div>
          {GROUP_DEPLOYMENT_FORMATS.includes(format) && (
            <Field label={t("form.writable")} help={t("form.writableHelp")} htmlFor="writable">
              <select id="writable" value={writable} onChange={(e) => setWritable(e.target.value)}>
                <option value="">{t("form.writableNone")}</option>
                {members
                  .filter((m) => (allRepos ?? []).find((r) => r.name === m)?.kind === "hosted")
                  .map((m) => (
                    <option key={m} value={m}>
                      {m}
                    </option>
                  ))}
              </select>
            </Field>
          )}
          {format === "yum" && <p className="help">{t("form.yumGroupHelp")}</p>}
        </div>
      )}

      <ErrorNote error={error} />
      {saved && <Notice>{t("form.saved")}</Notice>}
      <div className="actions">
        {!editing && (
          <button type="button" className="btn btn-quiet" onClick={() => navigate(-1)}>
            {t("form.cancel")}
          </button>
        )}
        <button className="btn btn-primary" disabled={busy}>
          {editing ? t("form.save") : t("form.create")}
        </button>
      </div>
    </form>
  );
}
