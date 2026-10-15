import { app, BrowserWindow, dialog, shell } from "electron";
import { once } from "node:events";
import { server } from "./server.mjs";

let mainWindow;

async function startServer() {
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("Could not determine the local FRAUM server address.");
  }
  return `http://127.0.0.1:${address.port}`;
}

async function createMainWindow() {
  const appUrl = await startServer();
  mainWindow = new BrowserWindow({
    width: 1200,
    height: 850,
    minWidth: 720,
    minHeight: 600,
    show: false,
    backgroundColor: "#10120f",
    title: "FRAUM — MiNERVA Archive",
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true
    }
  });

  mainWindow.webContents.setWindowOpenHandler(({ url }) => {
    if (url.startsWith("https://minerva-archive.org/")) {
      void shell.openExternal(url);
    }
    return { action: "deny" };
  });

  mainWindow.webContents.on("will-navigate", (event, url) => {
    if (!url.startsWith(appUrl)) {
      event.preventDefault();
      if (url.startsWith("https://minerva-archive.org/")) {
        void shell.openExternal(url);
      }
    }
  });

  mainWindow.once("ready-to-show", () => mainWindow.show());
  mainWindow.on("closed", () => {
    mainWindow = undefined;
  });
  await mainWindow.loadURL(appUrl);
}

app.whenReady().then(createMainWindow).catch(error => {
  console.error("Could not start FRAUM:", error);
  dialog.showErrorBox("FRAUM could not start", error.message);
  app.quit();
});

app.on("activate", () => {
  if (BrowserWindow.getAllWindows().length === 0) {
    createMainWindow().catch(error => {
      console.error("Could not reopen FRAUM:", error);
      dialog.showErrorBox("FRAUM could not start", error.message);
      app.quit();
    });
  }
});

app.on("before-quit", () => {
  if (server.listening) server.close();
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
