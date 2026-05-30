import React from "react";
import ReactDOM from "react-dom/client";
import { GluestackUIProvider } from "@okto/ui";
import "./global.css";
import App from "./App";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <GluestackUIProvider mode="light">
      <App />
    </GluestackUIProvider>
  </React.StrictMode>,
);
