/* @refresh reload */
import { render } from "@solidjs/web";

import App from "./App";
import "./styles.css";

const root = document.getElementById("root");
if (!root) throw new Error("The root element is missing.");

render(() => <App />, root);
