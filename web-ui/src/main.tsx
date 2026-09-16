import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";

import { Auth } from "./Auth";
import { App } from "./App";
import { OperationsProvider } from "./state/operations";
import "./styles.css";

const container = document.getElementById("root");
if (container === null) {
  throw new Error("the page has no #root element");
}

createRoot(container).render(
  <StrictMode>
    <BrowserRouter>
      <Auth><OperationsProvider>
        <App />
      </OperationsProvider></Auth>
    </BrowserRouter>
  </StrictMode>,
);
