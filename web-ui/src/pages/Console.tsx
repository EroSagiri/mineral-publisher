import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { api } from "../api/client";
import { start } from "../api/operations";
import { useOperations } from "../state/operations";
import type {
  ReviewListResponse,
  StatusResponse,
  OperationResult,
  OperationFailure,
} from "../api/types";

type Schedule = {
  kind: string;
  at: string;
  enabled: boolean;
  utc_offset_minutes: number;
  next_at_ms?: number;
};
type Dispatch = {
  kind: string;
  day: number;
  state: string;
  operation: string | null;
};
type Schedules = {
  enabled: boolean;
  schedules: Schedule[];
  dispatches: Dispatch[];
};
type Run = {
  id: string;
  kind: string;
  started_at_ms: number;
  finished_at_ms: number | null;
  state: string;
  outcome: { result?: OperationResult; failure?: OperationFailure } | null;
};
type Step = { sequence: number; at_ms: number; kind: string; message: string };
const names: Record<string, string> = {
  publish: "公开发布",
  backup: "私有备份",
  verify_backup: "备份验证",
  backup_init: "初始化备份",
  review_decision: "人工决定",
};
function date(ms: number | null | undefined) {
  return ms ? new Date(ms).toLocaleString() : "—";
}
function usePolling<T>(path: string) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    let alive = true;
    let pending = false;
    const load = async () => {
      if (pending) return;
      pending = true;
      try {
        const d = await api.get<T>(path);
        if (alive) {
          setData(d);
          setError("");
        }
      } catch (e) {
        if (alive) setError(String(e));
      } finally {
        pending = false;
      }
    };
    void load();
    const timer = setInterval(() => void load(), 4000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [path]);
  return { data, error };
}
export function Notifications() {
  const { data, error } = usePolling<ReviewListResponse>("/reviews");
  const count = (data?.markdown.length ?? 0) + (data?.assets.length ?? 0);
  useEffect(() => {
    document.title = count
      ? `(${count}) 待审核 · Mineral`
      : "Mineral Publisher";
  }, [count]);
  return (
    <Link
      className={count ? "notification pending" : "notification"}
      to="/reviews"
      role="status"
    >
      {error ? "审核通知连接失败" : `待人工审核 ${count}`}
    </Link>
  );
}
export function Overview() {
  const { data, error } = usePolling<StatusResponse>("/status");
  const { data: history } = usePolling<Run[]>("/history");
  const { run } = useOperations();
  const [actionError, setActionError] = useState("");
  const [busy, setBusy] = useState(false);
  async function begin(
    kind: "publish" | "backup" | "backupInit" | "backupVerify",
  ) {
    setBusy(true);
    try {
      await run(start[kind]);
      setActionError("");
    } catch (e) {
      setActionError(String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="page">
      <span className="eyebrow">WORKSPACE / OVERVIEW</span>
      <h1>发布与备份</h1>
      <p className="muted">从不可变快照，到审核、提交与远端确认。</p>
      {(error || actionError) && (
        <p className="failure" role="alert">
          {error || actionError}
        </p>
      )}
      <div className="metrics">
        <div className="card">
          <span className="label">数据源</span>
          <strong>{data?.source.kind ?? "…"}</strong>
          <p className="mono small">{data?.source.description}</p>
        </div>
        <div className="card">
          <span className="label">待人工审核</span>
          <strong>
            {data
              ? data.reviews.pending_markdown + data.reviews.pending_assets
              : "…"}
          </strong>
          <Link to="/reviews">查看审核队列 →</Link>
        </div>
        <div className="card">
          <span className="label">私有备份</span>
          <strong>{data?.backup.configured ? "已配置" : "未配置"}</strong>
          <Link to="/backups">备份与验证 →</Link>
        </div>
      </div>
      <section className="card">
        <h2>执行任务</h2>
        <p className="mono small">
          {data?.publication.remote} {data?.publication.reference}
        </p>
        <div className="actions">
          <button
            className="button primary"
            disabled={busy}
            onClick={() => void begin("publish")}
          >
            执行发布
          </button>
          <button
            className="button"
            disabled={busy || !data?.backup.configured}
            onClick={() => void begin("backup")}
          >
            立即备份
          </button>
          <Link className="button" to="/schedules">
            配置每天执行时间
          </Link>
        </div>
      </section>
      <h2>最近执行</h2>
      <RunTable runs={history?.slice(0, 8) ?? []} />
    </div>
  );
}
function ScheduleEditor({ initial }: { initial: Schedule }) {
  const [s, setS] = useState(initial);
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  async function save() {
    setBusy(true);
    try {
      const r = await fetch("/api/v1/schedules", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Mineral-Request": "1",
        },
        body: JSON.stringify({
          kind: s.kind,
          at: s.at,
          enabled: s.enabled,
          utc_offset_minutes: s.utc_offset_minutes,
        }),
      });
      if (r.status === 401)
        window.dispatchEvent(new Event("mineral-auth-expired"));
      if (!r.ok) throw Error((await r.json()).message);
      setMessage("已保存，下一次调度检查生效");
    } catch (e) {
      setMessage(String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <section className="card">
      <h2>{names[s.kind]}</h2>
      <label className="check">
        <input
          type="checkbox"
          checked={s.enabled}
          onChange={(e) => setS({ ...s, enabled: e.target.checked })}
        />
        每天自动执行
      </label>
      <div className="form-row">
        <label>
          执行时间
          <input
            type="time"
            required
            value={s.at}
            onChange={(e) => setS({ ...s, at: e.target.value })}
          />
        </label>
        <label>
          UTC 偏移（分钟）
          <input
            type="number"
            min={-720}
            max={840}
            value={s.utc_offset_minutes}
            onChange={(e) =>
              setS({ ...s, utc_offset_minutes: Number(e.target.value) })
            }
          />
        </label>
      </div>
      <p className="muted small">
        中国标准时间为 +480；此设置为固定偏移，不自动切换夏令时。
      </p>
      <button
        className="button primary"
        disabled={busy || !s.at}
        onClick={() => void save()}
      >
        保存计划
      </button>
      <p role="status">{message}</p>
    </section>
  );
}
export function SchedulesPage() {
  const { data, error } = usePolling<Schedules>("/schedules");
  return (
    <div className="page">
      <span className="eyebrow">AUTOMATION</span>
      <h1>调度计划</h1>
      {error && <p className="failure">{error}</p>}
      <p className="notice">
        {data?.enabled
          ? "调度器运行中，每 10 秒检查。"
          : "当前进程未启用调度；请使用 mineral daemon 启动。"}{" "}
        当天已过执行时间的未运行计划会补执行；更早的日期不补跑。工作区忙时顺延，同一天最多自动触发一次。
      </p>
      <div className="metrics">
        {data?.schedules.map((s) => (
          <ScheduleEditor key={s.kind} initial={s} />
        ))}
      </div>
      <h2>调度触发记录</h2>
      <table>
        <thead>
          <tr>
            <th>任务</th>
            <th>计划日期</th>
            <th>触发状态</th>
            <th>本次进程任务</th>
          </tr>
        </thead>
        <tbody>
          {data?.dispatches.map((d) => (
            <tr key={`${d.kind}-${d.day}`}>
              <td>{names[d.kind]}</td>
              <td>{new Date(d.day * 86400000).toISOString().slice(0, 10)}</td>
              <td>{d.state}</td>
              <td>{d.operation ?? "—"}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="muted small">
        interrupted
        表示中断时不能确认是否已开始；不会自动重试。请检查执行历史及远端状态后手动处理。
      </p>
    </div>
  );
}
function RunTable({ runs }: { runs: Run[] }) {
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>任务 / 执行标识</th>
            <th>开始时间</th>
            <th>状态</th>
            <th>耗时</th>
          </tr>
        </thead>
        <tbody>
          {runs.map((r) => (
            <tr key={r.id}>
              <td>
                <Link to={`/history?run=${r.id}`}>
                  {names[r.kind] ?? r.kind}
                </Link>
                <div className="mono small muted">{r.id}</div>
              </td>
              <td>{date(r.started_at_ms)}</td>
              <td>
                <span className={`badge badge-${r.state}`}>{r.state}</span>
                <div className="small muted">
                  {r.outcome?.result?.publish?.status ??
                    r.outcome?.result?.backup?.status ??
                    r.outcome?.result?.verify?.status}
                </div>
                {r.outcome?.result?.publish?.status ===
                  "waiting_for_human_review" && (
                  <Link to="/reviews">查看人工审核 →</Link>
                )}
              </td>
              <td>
                {r.finished_at_ms
                  ? `${((r.finished_at_ms - r.started_at_ms) / 1000).toFixed(1)} s`
                  : "运行中 / 未确认"}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {!runs.length && <p className="muted">还没有执行记录。</p>}
    </div>
  );
}
function Details({ run }: { run: Run }) {
  const { data: steps, error } = usePolling<Step[]>(`/history/${run.id}/steps`);
  const [filter, setFilter] = useState("");
  const [onlyStages, setOnlyStages] = useState(false);
  let stage = "任务开始";
  const groups: { title: string; steps: Step[] }[] = [];
  for (const line of steps ?? []) {
    if (line.kind === "stage" || !groups.length) {
      stage = line.kind === "stage" ? line.message : stage;
      groups.push({ title: stage, steps: [] });
    }
    groups[groups.length - 1]!.steps.push(line);
  }
  return (
    <section className="card">
      <h2>{names[run.kind] ?? run.kind} · 执行详情</h2>
      <p className="mono small">{run.id}</p>
      <p>
        {date(run.started_at_ms)} → {date(run.finished_at_ms)}
      </p>
      <div className="form-row">
        <label>
          搜索步骤
          <input
            type="search"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="路径、阶段、提交…"
          />
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={onlyStages}
            onChange={(e) => setOnlyStages(e.target.checked)}
          />
          只看阶段
        </label>
      </div>
      {error && <p className="failure">{error}</p>}
      <div className="timeline">
        {groups
          .filter((g) =>
            g.steps.some((s) =>
              s.message.toLowerCase().includes(filter.toLowerCase()),
            ),
          )
          .map((g, i) => (
            <details key={i} open>
              <summary>
                {g.title}
                <span className="muted small"> · {g.steps.length} 条</span>
              </summary>
              {g.steps
                .filter(
                  (s) =>
                    (!onlyStages || s.kind === "stage") &&
                    s.message.toLowerCase().includes(filter.toLowerCase()),
                )
                .map((s) => (
                  <div className="step" key={s.sequence}>
                    <time>{new Date(s.at_ms).toLocaleTimeString()}</time>
                    <span>{s.message}</span>
                  </div>
                ))}
            </details>
          ))}
      </div>
      {run.state === "interrupted" && (
        <p className="notice">
          进程曾中断。请核对持久化发布记录和远端状态后再重试。
        </p>
      )}
      {run.outcome && (
        <>
          <h3>提交、备份与审核结果</h3>
          <pre className="result-json">
            {JSON.stringify(run.outcome, null, 2)}
          </pre>
        </>
      )}
    </section>
  );
}
export function HistoryPage({ backup = false }: { backup?: boolean }) {
  const { data, error } = usePolling<Run[]>("/history");
  const [kind, setKind] = useState("all");
  const [selected, setSelected] = useState(
    new URLSearchParams(location.search).get("run"),
  );
  const { run } = useOperations();
  const [actionError, setActionError] = useState("");
  const [busy, setBusy] = useState(false);
  const rows = (data ?? []).filter((r) =>
    backup ? r.kind.includes("backup") : kind === "all" || r.kind === kind,
  );
  const detail = (data ?? []).find((r) => r.id === selected);
  async function action(starter: typeof start.backup) {
    setBusy(true);
    try {
      await run(starter);
      setActionError("");
    } catch (e) {
      setActionError(String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="page">
      <span className="eyebrow">EXECUTION / AUDIT</span>
      <h1>{backup ? "备份与恢复验证" : "执行历史"}</h1>
      <p className="muted">
        阶段与明细持续写入数据库，重启后可查看。最近 200 次执行。
      </p>
      {(error || actionError) && (
        <p className="failure">{error || actionError}</p>
      )}
      {backup ? (
        <div className="actions">
          <button
            disabled={busy}
            className="button primary"
            onClick={() => void action(start.backup)}
          >
            立即备份
          </button>
          <button
            disabled={busy}
            className="button"
            onClick={() => void action(start.backupVerify)}
          >
            验证远端备份
          </button>
          <button
            disabled={busy}
            className="button"
            onClick={() => void action(start.backupInit)}
          >
            初始化备份分支
          </button>
        </div>
      ) : (
        <label>
          任务类型
          <select value={kind} onChange={(e) => setKind(e.target.value)}>
            <option value="all">全部任务</option>
            {Object.entries(names).map(([k, n]) => (
              <option key={k} value={k}>
                {n}
              </option>
            ))}
          </select>
        </label>
      )}
      <div
        onClick={(e) => {
          const a = (e.target as HTMLElement).closest("a");
          if (a?.getAttribute("href")?.startsWith("/history?run=")) {
            e.preventDefault();
            const id = new URL(a.href).searchParams.get("run");
            setSelected(id);
            history.replaceState(null, "", `${location.pathname}?run=${id}`);
          }
        }}
      >
        <RunTable runs={rows} />
      </div>
      {detail && <Details key={detail.id} run={detail} />}
    </div>
  );
}
export function IntegrationsPage() {
  return (
    <div className="page">
      <span className="eyebrow">INTEGRATIONS</span>
      <h1>插件提交与 API</h1>
      <section className="card">
        <h2>触发发布 / 备份</h2>
        <p>
          插件使用管理员 Bearer 凭据提交任务。每个请求返回
          operation_id，可查询实时步骤；执行历史使用独立标识，跨重启保存。
        </p>
        <pre className="result-json">{`POST /api/v1/operations/publish\nPOST /api/v1/operations/backup\nAuthorization: Bearer <管理员凭据>\n\nGET /api/v1/operations/<operation_id>\nGET /api/v1/operations/<operation_id>/events\nGET /api/v1/history\nGET /api/v1/history/<history_id>/steps`}</pre>
        <p>
          任务冲突返回
          409，插件应稍后重试。发布内容仍来自配置的数据源，按完整审核流程执行。
        </p>
        <Link to="/history">查看提交与执行详情 →</Link>
      </section>
    </div>
  );
}
