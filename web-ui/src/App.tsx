import { NavLink, Route, Routes } from "react-router-dom";

import { Dashboard } from "./pages/Dashboard";
import { Doctor } from "./pages/Doctor";
import { Operations } from "./pages/Operations";
import { Reviews } from "./pages/Reviews";
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
        <span className="brand">Mineral</span>
        <NavLink to="/" end>
          Dashboard
        </NavLink>
        <NavLink to="/reviews">Reviews</NavLink>
        <NavLink to="/operations">Operations</NavLink>
        <NavLink to="/doctor">Doctor</NavLink>
      </nav>
      <main className="content">
        <Routes>
          <Route path="/" element={<Dashboard />} />
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
