import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@cloudflare/kumo/styles/standalone";
import "./app.css";
import { App } from "./App";
import { boot } from "./bridge";

boot();

createRoot(document.getElementById("root") as HTMLElement).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
