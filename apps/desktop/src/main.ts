import { tauriBackend } from "./api";
import { App } from "./app";

async function boot(): Promise<void> {
  const root = document.getElementById("app");
  if (!root) return;
  try {
    const app = new App(await tauriBackend());
    root.replaceChildren(app.el);
    await app.start();
  } catch (e) {
    const p = document.createElement("p");
    p.textContent = `The backend did not answer: ${String(e)}`;
    root.replaceChildren(p);
  }
}

void boot();
