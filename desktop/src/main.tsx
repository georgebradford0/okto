import React from "react";
import ReactDOM from "react-dom/client";
import { OktoProvider } from "@okto/ui";
import "./global.css";
import App from "./App";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <OktoProvider mode="light">
      <App />
    </OktoProvider>
  </React.StrictMode>,
);
