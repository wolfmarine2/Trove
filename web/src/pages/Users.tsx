import { useState, type FormEvent } from "react";
import { api, type Role, type User } from "../api";
import { Dialog, ErrorNote, Field, Loading, useAsync, useConfirm } from "../components";
import { useI18n } from "../i18n";
import { useSession } from "../session";
import { formatTime } from "../util";

const ROLES: Role[] = ["viewer", "deployer", "admin"];

export function UsersPage() {
  const { t, lang } = useI18n();
  const { user: me } = useSession();
  const { data, error, loading, reload } = useAsync(() => api.users(), []);
  const [editing, setEditing] = useState<User | "new" | null>(null);
  const { confirm, element } = useConfirm();
  const [actionError, setActionError] = useState<unknown>();

  return (
    <section className="page">
      <header className="page-head">
        <h1>{t("users.title")}</h1>
        <button className="btn btn-primary" onClick={() => setEditing("new")}>{t("users.create")}</button>
      </header>
      {loading && <Loading />}
      <ErrorNote error={error ?? actionError} />
      {data && (
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>{t("users.username")}</th>
                <th>{t("users.displayName")}</th>
                <th>{t("users.email")}</th>
                <th>{t("users.role")}</th>
                <th>{t("asset.created")}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {data.map((u) => (
                <tr key={u.id} className={u.disabled ? "dim" : undefined}>
                  <td>
                    <strong>{u.username}</strong>
                    {u.disabled && <span className="badge badge-off">{t("users.disabled")}</span>}
                  </td>
                  <td>{u.display_name}</td>
                  <td>{u.email}</td>
                  <td><span className={`role role-${u.role}`}>{u.role}</span></td>
                  <td>{formatTime(u.created_at, lang)}</td>
                  <td className="row-actions">
                    <button className="btn btn-quiet btn-small" onClick={() => setEditing(u)}>{t("users.edit")}</button>
                    {u.id !== me?.id && (
                      <button
                        className="btn btn-quiet btn-small"
                        onClick={async () => {
                          if (!(await confirm(t("users.deleteConfirm", { name: u.username })))) return;
                          try {
                            await api.deleteUser(u.id);
                            reload();
                          } catch (e) {
                            setActionError(e);
                          }
                        }}
                      >
                        {t("users.delete")}
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <Dialog
        open={editing !== null}
        title={editing === "new" ? t("users.create") : t("users.edit")}
        onClose={() => setEditing(null)}
      >
        {editing !== null && (
          <UserForm
            key={editing === "new" ? "new" : editing.id}
            user={editing === "new" ? undefined : editing}
            onDone={() => {
              setEditing(null);
              reload();
            }}
          />
        )}
      </Dialog>
      {element}
    </section>
  );
}

function UserForm({ user, onDone }: { user?: User; onDone: () => void }) {
  const { t } = useI18n();
  const [username, setUsername] = useState(user?.username ?? "");
  const [displayName, setDisplayName] = useState(user?.display_name ?? "");
  const [email, setEmail] = useState(user?.email ?? "");
  const [role, setRole] = useState<Role>(user?.role ?? "deployer");
  const [password, setPassword] = useState("");
  const [disabled, setDisabled] = useState(user?.disabled ?? false);
  const [error, setError] = useState<unknown>();

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setError(undefined);
    try {
      if (user) await api.updateUser(user.id, { display_name: displayName, email, role, disabled, password: password || undefined });
      else await api.createUser({ username, password, display_name: displayName, email, role });
      onDone();
    } catch (err) {
      setError(err);
    }
  };

  return (
    <form className="form" onSubmit={submit}>
      {!user && (
        <Field label={t("users.username")} htmlFor="uf-name">
          <input id="uf-name" value={username} onChange={(e) => setUsername(e.target.value)} required autoFocus />
        </Field>
      )}
      <Field label={t("users.displayName")} htmlFor="uf-dn">
        <input id="uf-dn" value={displayName} onChange={(e) => setDisplayName(e.target.value)} />
      </Field>
      <Field label={t("users.email")} htmlFor="uf-mail">
        <input id="uf-mail" type="email" value={email} onChange={(e) => setEmail(e.target.value)} />
      </Field>
      <Field label={t("users.role")} htmlFor="uf-role">
        <select id="uf-role" value={role} onChange={(e) => setRole(e.target.value as Role)}>
          {ROLES.map((r) => (
            <option key={r} value={r}>{t(`users.role.${r}`)}</option>
          ))}
        </select>
      </Field>
      <Field label={user ? t("users.newPassword") : t("users.password")} htmlFor="uf-pw">
        <input id="uf-pw" type="password" autoComplete="new-password" minLength={8} value={password} onChange={(e) => setPassword(e.target.value)} required={!user} />
      </Field>
      {user && (
        <label className="check">
          <input type="checkbox" checked={disabled} onChange={(e) => setDisabled(e.target.checked)} />
          <span>{t("users.disabled")}</span>
        </label>
      )}
      <ErrorNote error={error} />
      <div className="actions">
        <button type="button" className="btn btn-quiet" onClick={onDone}>{t("common.cancel")}</button>
        <button className="btn btn-primary">{user ? t("form.save") : t("users.create")}</button>
      </div>
    </form>
  );
}
