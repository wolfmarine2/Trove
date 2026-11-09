import { useState, type FormEvent } from "react";
import { api } from "../api";
import { CopyButton, ErrorNote, Field, Loading, Notice, useAsync, useConfirm } from "../components";
import { useI18n } from "../i18n";
import { formatTime, relativeTime } from "../util";

export function TokensPage() {
  const { t, lang } = useI18n();
  const { data, error, loading, reload } = useAsync(() => api.tokens(), []);
  const [name, setName] = useState("");
  const [days, setDays] = useState(90);
  const [secret, setSecret] = useState("");
  const [formError, setFormError] = useState<unknown>();
  const { confirm, element } = useConfirm();

  const create = async (e: FormEvent) => {
    e.preventDefault();
    setFormError(undefined);
    try {
      const res = await api.createToken(name.trim(), days);
      setSecret(res.secret);
      setName("");
      reload();
    } catch (err) {
      setFormError(err);
    }
  };

  return (
    <section className="page narrow">
      <header className="page-head">
        <div>
          <h1>{t("tokens.title")}</h1>
          <p className="lede">{t("tokens.intro")}</p>
        </div>
      </header>

      <form className="form inline-form" onSubmit={create}>
        <Field label={t("tokens.name")} htmlFor="tname">
          <input id="tname" value={name} onChange={(e) => setName(e.target.value)} required maxLength={100} placeholder="ci-pipeline" />
        </Field>
        <Field label={t("tokens.expiry")} htmlFor="tdays">
          <select id="tdays" value={days} onChange={(e) => setDays(Number(e.target.value))}>
            {[30, 90, 365].map((d) => (
              <option key={d} value={d}>{t("tokens.days", { n: d })}</option>
            ))}
            <option value={0}>{t("tokens.never")}</option>
          </select>
        </Field>
        <button className="btn btn-primary">{t("tokens.create")}</button>
      </form>
      <ErrorNote error={formError} />
      {secret && (
        <div className="secret" role="status">
          <p>{t("tokens.secretOnce")}</p>
          <div className="secret-row">
            <code>{secret}</code>
            <CopyButton text={secret} />
          </div>
        </div>
      )}

      {loading && <Loading />}
      <ErrorNote error={error} />
      {data && data.length === 0 && <p className="empty">{t("tokens.empty")}</p>}
      {data && data.length > 0 && (
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>{t("tokens.name")}</th>
                <th>{t("tokens.col.prefix")}</th>
                <th>{t("tokens.col.created")}</th>
                <th>{t("tokens.expiry")}</th>
                <th>{t("tokens.col.lastUsed")}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {data.map((tk) => (
                <tr key={tk.id}>
                  <td>{tk.name}</td>
                  <td><code>{tk.prefix}…</code></td>
                  <td>{formatTime(tk.created_at, lang)}</td>
                  <td>{tk.expires_at ? formatTime(tk.expires_at, lang) : t("tokens.never")}</td>
                  <td>{relativeTime(tk.last_used_at, lang)}</td>
                  <td className="row-actions">
                    <button
                      className="btn btn-quiet btn-small"
                      onClick={async () => {
                        if (await confirm(t("tokens.revokeConfirm", { name: tk.name }))) {
                          await api.deleteToken(tk.id);
                          reload();
                        }
                      }}
                    >
                      {t("tokens.revoke")}
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <PasswordForm />
      {element}
    </section>
  );
}

function PasswordForm() {
  const { t } = useI18n();
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [done, setDone] = useState(false);
  const [error, setError] = useState<unknown>();
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setError(undefined);
    setDone(false);
    try {
      await api.changePassword(current, next);
      setDone(true);
      setCurrent("");
      setNext("");
    } catch (err) {
      setError(err);
    }
  };
  return (
    <form className="form subsection" onSubmit={submit}>
      <h2>{t("tokens.password")}</h2>
      <div className="two">
        <Field label={t("tokens.current")} htmlFor="pw-cur">
          <input id="pw-cur" type="password" autoComplete="current-password" value={current} onChange={(e) => setCurrent(e.target.value)} required />
        </Field>
        <Field label={t("tokens.new")} htmlFor="pw-new">
          <input id="pw-new" type="password" autoComplete="new-password" minLength={8} value={next} onChange={(e) => setNext(e.target.value)} required />
        </Field>
      </div>
      <ErrorNote error={error} />
      {done && <Notice>{t("tokens.passwordChanged")}</Notice>}
      <div className="actions">
        <button className="btn btn-quiet">{t("tokens.password")}</button>
      </div>
    </form>
  );
}
