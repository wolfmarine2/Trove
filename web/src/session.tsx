import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from "react";
import { api, type User } from "./api";

interface Session {
  user: User | null;
  anonymousRead: boolean;
  version: string;
  ready: boolean;
  refresh: () => Promise<void>;
  signOut: () => Promise<void>;
}

const SessionContext = createContext<Session | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [user, setUser] = useState<User | null>(null);
  const [anonymousRead, setAnonymousRead] = useState(false);
  const [version, setVersion] = useState("");
  const [ready, setReady] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const status = await api.status();
      setVersion(status.version);
      setAnonymousRead(status.anonymous_read);
    } catch {
      /* server unreachable; pages will surface errors */
    }
    try {
      const me = await api.me();
      setUser(me.user);
    } catch {
      setUser(null);
    }
    setReady(true);
  }, []);

  const signOut = useCallback(async () => {
    await api.logout().catch(() => undefined);
    setUser(null);
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return (
    <SessionContext.Provider value={{ user, anonymousRead, version, ready, refresh, signOut }}>
      {children}
    </SessionContext.Provider>
  );
}

export function useSession() {
  const ctx = useContext(SessionContext);
  if (!ctx) throw new Error("useSession outside provider");
  return ctx;
}
