import type { FormEvent, ReactNode } from "react";
import { useEffect, useState } from "react";
export function Auth({ children }: { children: ReactNode }) {
  const [authenticated, setAuthenticated] = useState<boolean | null>(null);
  const [token, setToken] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const check = () =>
    fetch("/api/v1/auth/session", { cache: "no-store" })
      .then((r) => {
        if (!r.ok) throw Error("无法连接认证服务");
        return r.json();
      })
      .then((s) => setAuthenticated(s.authenticated))
      .catch((e) => setError(String(e)));
  useEffect(() => {
    void check();
    const expired = () => setAuthenticated(false);
    window.addEventListener("mineral-auth-expired", expired);
    return () => window.removeEventListener("mineral-auth-expired", expired);
  }, []);
  async function login(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError("");
    try {
      const r = await fetch("/api/v1/auth/login", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ token }),
      });
      if (!r.ok)
        throw Error(
          r.status === 429
            ? "尝试过于频繁，请一分钟后重试"
            : "认证失败，请检查访问凭据",
        );
      setToken("");
      setAuthenticated(true);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }
  if (authenticated) return <>{children}</>;
  return (
    <div className="login">
      <form className="card" onSubmit={(e) => void login(e)}>
        <span className="eyebrow">MINERAL PUBLISHER</span>
        <h1>登录控制台</h1>
        <p className="muted">管理发布计划、备份与人工审核。</p>
        <label>
          管理员访问凭据
          <input
            type="password"
            autoComplete="current-password"
            required
            minLength={32}
            value={token}
            onChange={(e) => setToken(e.target.value)}
          />
        </label>
        {error && (
          <p role="alert" className="failure">
            {error}
          </p>
        )}
        <button className="button primary" disabled={busy}>
          {busy ? "正在登录…" : "登录"}
        </button>
      </form>
    </div>
  );
}
