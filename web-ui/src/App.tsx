import { NavLink, Route, Routes } from "react-router-dom";

import {
  Overview,
  SchedulesPage,
  HistoryPage,
  Notifications,
  IntegrationsPage,
} from "./pages/Console";
import { Doctor } from "./pages/Doctor";
import { Operations } from "./pages/Operations";
import { Reviews } from "./pages/Reviews";
import { BusyNotice } from "./components/BusyNotice";
import { OperationCard } from "./components/OperationCard";
import { useOperations } from "./state/operations";

/** The active operation, on every page, so progress is never out of sight. */
function ActiveOperationStrip() {
  const { active } = useOperations();
  if (active === null) {
    return null;
  }
  return (
    <div className="strip">
      <OperationCard operation={active} />
    </div>
  );
}

export function App() {
  return (
    <div className="app">
      <nav className="nav">
        <span className="brand">
          ◈ Mineral<span className="small muted">PUBLISHER CONSOLE</span>
        </span>
        <NavLink to="/" end>
          总览
        </NavLink>
        <NavLink to="/schedules">调度计划</NavLink>
        <NavLink to="/history">执行历史</NavLink>
        <NavLink to="/reviews">人工审核</NavLink>
        <NavLink to="/backups">备份与验证</NavLink>
        <NavLink to="/integrations">插件与提交</NavLink>
        <NavLink to="/operations">实时任务</NavLink>
        <NavLink to="/doctor">健康检查</NavLink>
        <Notifications />
        <button
          className="button"
          onClick={() =>
            void fetch("/api/v1/auth/logout", {
              method: "POST",
              headers: { "X-Mineral-Request": "1" },
            }).then(() => location.reload())
          }
        >
          退出登录
        </button>
      </nav>
      <main className="content">
        <BusyNotice />
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/schedules" element={<SchedulesPage />} />
          <Route path="/history" element={<HistoryPage />} />
          <Route path="/backups" element={<HistoryPage backup />} />
          <Route path="/integrations" element={<IntegrationsPage />} />
          <Route path="/reviews" element={<Reviews />} />
          <Route path="/reviews/:attempt" element={<Reviews />} />
          <Route path="/operations" element={<Operations />} />
          <Route path="/operations/:id" element={<Operations />} />
          <Route path="/doctor" element={<Doctor />} />
          <Route path="*" element={<p className="muted">No such page.</p>} />
        </Routes>
      </main>
      <ActiveOperationStrip />
    </div>
  );
}
