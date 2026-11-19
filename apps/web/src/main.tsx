import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { initializeBroadcastStorage } from "./storage";
import "@fontsource-variable/inter";
import "./styles.css";

async function main() {
  await initializeBroadcastStorage();
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}

void main();
