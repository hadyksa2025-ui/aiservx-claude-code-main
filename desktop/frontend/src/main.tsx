import React from "react";
import ReactDOM from "react-dom/client";
// Self-host the UI fonts so offline installs still get the intended
// typography (audit §7.6). Inter covers UI text, JetBrains Mono
// covers code / identifiers. We only pull weights we actually use
// (400/500/600) to keep the bundle small.
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "@fontsource/jetbrains-mono/600.css";
import App from "./App";
import "./styles.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
