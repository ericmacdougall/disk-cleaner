import { invoke } from "@tauri-apps/api/core";

interface AppInfo {
  version: string;
  schema_version: number;
  read_only: boolean;
}

async function start(): Promise<void> {
  const root = document.getElementById("app");
  if (!root) return;
  const heading = document.createElement("h1");
  heading.textContent = "PruneDisk";
  const line = document.createElement("p");
  root.append(heading, line);
  try {
    const info = await invoke<AppInfo>("app_info");
    line.textContent = `Version ${info.version}, scan schema ${info.schema_version}. Read-only.`;
  } catch (e) {
    line.textContent = `The backend did not answer: ${String(e)}`;
  }
}

void start();
