import { useState, type FormEvent } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { api, ApiError } from "../api";
import { useI18n } from "../i18n";
import { useSession } from "../session";

export function LoginPage() {
  const { t, lang, setLang } = useI18n();
  const { refresh } = useSession();
  const navigate = useNavigate();
  const location = useLocation();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError("");
    try {
      await api.login(username, password);
      await refresh();
      const from = (location.state as { from?: string } | null)?.from;
      navigate(from && from !== "/login" ? from : "/", { replace: true });
    } catch (err) {
      setError(err instanceof ApiError && err.status === 401 ? t("login.failed") : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="login">
      <form className="login-form" onSubmit={submit}>
        <div className="login-mark" aria-hidden="true">
          <svg width="40" height="40" viewBox="0 0 32 32">
            <path d="M7 12h18v13H7z" fill="none" stroke="currentColor" strokeWidth="2.4" />
            <path d="M5 8h22v4H5z" fill="currentColor" />
            <path d="M14 16h4v4h-4z" fill="currentColor" />
          </svg>
        </div>
        <h1>{t("login.title")}</h1>
        <label htmlFor="u">{t("login.username")}</label>
        <input id="u" autoComplete="username" value={username} onChange={(e) => setUsername(e.target.value)} required autoFocus />
        <label htmlFor="p">{t("login.password")}</label>
        <input id="p" type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} required />
        {error && <p className="note note-error" role="alert">{error}</p>}
        <button className="btn btn-primary" disabled={busy}>{t("login.submit")}</button>
        <p className="help">{t("login.hint")}</p>
        <button type="button" className="link-button" onClick={() => setLang(lang === "zh" ? "en" : "zh")}>
          {t("common.language")}
        </button>
      </form>
    </div>
  );
}
