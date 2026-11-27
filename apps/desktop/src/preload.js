// The launcher is the only page that may control the desktop shell. Space
// pages load like ordinary web pages, without a desktop bridge.
const { contextBridge, ipcRenderer } = require("electron");

if (window.location.protocol === "neha:") {
  contextBridge.exposeInMainWorld("nehaDesktop", {
    chooseLocal: (useLocalComputer) =>
      ipcRenderer.invoke("desktop:choose-local", useLocalComputer === true),
    connectRemote: (url) => ipcRenderer.invoke("desktop:connect-remote", String(url)),
  });
}
