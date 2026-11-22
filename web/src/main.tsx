import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { applyTheme, loadTheme } from "./api/theme";
import "./styles/tokens.css";
import "./styles/app.css";
import "./vendor/light-ui/running-border.css";

// 画第一帧之前就把明暗定下来，免得先亮后暗闪一下。
applyTheme(loadTheme());

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
