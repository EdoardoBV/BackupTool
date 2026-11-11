import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import Popover from "./Popover";
import "./styles.css";

// The tray popover is the same page opened with `?view=popover`.
const popover = new URLSearchParams(location.search).get("view") === "popover";

createRoot(document.getElementById("root")!).render(
  <StrictMode>{popover ? <Popover /> : <App />}</StrictMode>,
);
