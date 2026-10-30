import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import "./styles/base.css";
import "./styles/launcher.css";
import "./styles/overlays.css";
import "./styles/plait-board.css";
import "./styles/plait-text.css";
import "./styles/drawnix.css";
import "./styles/whiteboard.css";
import "./styles/annotation.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
