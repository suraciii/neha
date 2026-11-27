/* @refresh reload */
import { render } from "@solidjs/web";

import DesktopLauncher from "./components/DesktopLauncher";
import "./styles.css";

const root = document.getElementById("root");
if (!root) throw new Error("The root element is missing.");

render(() => <DesktopLauncher />, root);
