import type { ReactNode } from "react";
import { Navigate, NavLink, Route, Routes, useLocation, useNavigate } from "react-router-dom";
import { useI18n } from "./i18n";
import { SessionProvider, useSession } from "./session";
import { LoginPage } from "./pages/Login";
import { RepositoriesPage } from "./pages/Repositories";
import { RepositoryPage } from "./pages/Repository";
import { RepoFormPage } from "./pages/RepoForm";
import { SearchPage } from "./pages/Search";
import { TokensPage } from "./pages/Tokens";
import { UsersPage } from "./pages/Users";
import { AuditPage } from "./pages/Audit";
import { SystemPage } from "./pages/System";

export function App() {
  return (
    <SessionProvider>
      <Shell />
    </SessionProvider>
  );
}

function Shell() {
  const { ready, user, anonymousRead } = useSession();
  const location = useLocation();
  if (!ready) return null;
  if (location.pathname === "/login") return <LoginPage />;
  if (!user && !anonymousRead) return <Navigate to="/login" replace state={{ from: location.pathname }} />;
  return (
    <div className="shell">
      <Sidebar />
      <main className="main" id="main">
        <Routes>
          <Route path="/" element={<RepositoriesPage />} />
          <Route path="/repositories/new" element={<AdminOnly><RepoFormPage /></AdminOnly>} />
          <Route path="/repositories/:name/*" element={<RepositoryPage />} />
          <Route path="/search" element={<SearchPage />} />
          <Route path="/tokens" element={<SignedIn><TokensPage /></SignedIn>} />
          <Route path="/admin/users" element={<AdminOnly><UsersPage /></AdminOnly>} />
          <Route path="/admin/audit" element={<AdminOnly><AuditPage /></AdminOnly>} />
          <Route path="/admin/system" element={<AdminOnly><SystemPage /></AdminOnly>} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
    </div>
  );
}

function SignedIn({ children }: { children: ReactNode }) {
  const { user } = useSession();
  const location = useLocation();
  if (!user) return <Navigate to="/login" replace state={{ from: location.pathname }} />;
  return <>{children}</>;
}

function AdminOnly({ children }: { children: ReactNode }) {
  const { user } = useSession();
  const { t } = useI18n();
  const location = useLocation();
  if (!user) return <Navigate to="/login" replace state={{ from: location.pathname }} />;
  if (user.role !== "admin") return <p className="note note-error pad">{t("common.forbidden")}</p>;
  return <>{children}</>;
}

function Sidebar() {
  const { t, lang, setLang } = useI18n();
  const { user, signOut, version } = useSession();
  const navigate = useNavigate();
  const isAdmin = user?.role === "admin";
  return (
    <aside className="sidebar">
      <a className="skip" href="#main">Skip to content</a>
      <NavLink to="/" className="brand" aria-label="Trove">
        <svg width="26" height="26" viewBox="0 0 32 32" aria-hidden="true">
          <path d="M7 12h18v13H7z" fill="none" stroke="currentColor" strokeWidth="2.4" />
          <path d="M5 8h22v4H5z" fill="currentColor" />
          <path d="M14 16h4v4h-4z" fill="currentColor" />
        </svg>
        <span>Trove</span>
      </NavLink>
      <nav aria-label="Main">
        <NavLink to="/" end>{t("nav.repositories")}</NavLink>
        <NavLink to="/search">{t("nav.search")}</NavLink>
        {user && <NavLink to="/tokens">{t("nav.tokens")}</NavLink>}
      </nav>
      {isAdmin && (
        <nav aria-label={t("nav.admin")}>
          <h2>{t("nav.admin")}</h2>
          <NavLink to="/admin/users">{t("nav.users")}</NavLink>
          <NavLink to="/admin/audit">{t("nav.audit")}</NavLink>
          <NavLink to="/admin/system">{t("nav.system")}</NavLink>
        </nav>
      )}
      <div className="sidebar-foot">
        {user ? (
          <>
            <p className="who">
              <strong>{user.display_name || user.username}</strong>
              <span>{user.role}</span>
            </p>
            <button
              className="link-button"
              onClick={async () => {
                await signOut();
                navigate("/login");
              }}
            >
              {t("nav.signOut")}
            </button>
          </>
        ) : (
          <>
            <p className="who"><span>{t("nav.anonymous")}</span></p>
            <NavLink to="/login" className="link-button">{t("nav.signIn")}</NavLink>
          </>
        )}
        <button className="link-button" onClick={() => setLang(lang === "zh" ? "en" : "zh")}>
          {t("common.language")}
        </button>
        {version && <p className="version">{t("system.version", { v: version })}</p>}
      </div>
    </aside>
  );
}
