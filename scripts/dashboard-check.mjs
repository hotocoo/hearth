// Drives hearthd's dashboard in a real headless Chrome, over the DevTools
// protocol (Node's built-in WebSocket, no packages): keys select, restart,
// stop (twice), start and open output; the drawer and the shortcuts dialog
// open and close; the page throws nothing and breaks no CSP rule; it fits a
// 375 px phone; reduced motion turns transitions off.
//
// Usage: node scripts/dashboard-check.mjs [path/to/target/debug]
// Needs python3 (the fake servers) and Chrome or Chromium; exits 0 with
// "skipped" when either is missing.

import { spawn, execFileSync } from "node:child_process";
import { existsSync, mkdirSync, rmSync, writeFileSync, chmodSync, readFileSync } from "node:fs";
import { createServer } from "node:net";
import { join, resolve } from "node:path";

const bin = resolve(process.argv[2] || "target/debug");
const LIMIT_MS = 30000;

const chrome = [
  process.env.CHROME,
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/Applications/Chromium.app/Contents/MacOS/Chromium",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
  "/usr/bin/chromium-browser",
].find((p) => p && existsSync(p));
const hasPython = (() => {
  try { execFileSync("python3", ["--version"]); return true; } catch { return false; }
})();
if (!chrome || !hasPython) {
  console.log(`dashboard check skipped: ${chrome ? "" : "no Chrome "}${hasPython ? "" : "no python3"}`);
  process.exit(0);
}

const freePort = () => new Promise((ok) => {
  const s = createServer().listen(0, "127.0.0.1", () => {
    const { port } = s.address();
    s.close(() => ok(port));
  });
});
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(what, test) {
  const t = Date.now();
  for (;;) {
    const v = await test();
    if (v) return v;
    if (Date.now() - t > LIMIT_MS) throw new Error(`timed out waiting for ${what}`);
    await sleep(100);
  }
}

// A short, private state directory: socket paths are limited in length.
const dir = resolve("target/tmp/dash");
rmSync(dir, { recursive: true, force: true });
mkdirSync(dir, { recursive: true });
chmodSync(dir, 0o700);
const server = `
import sys, json
from http.server import BaseHTTPRequestHandler, HTTPServer
print("fake server starting", flush=True)
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        b = json.dumps({"data": [{"id": "fake-model"}]}).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
`;
writeFileSync(join(dir, "server.py"), server);
const [a, b, dash] = [await freePort(), await freePort(), await freePort()];
writeFileSync(join(dir, "hearth.toml"), `
[watch]
in_hearthd = false
[dashboard]
listen = "127.0.0.1:${dash}"
refresh_ms = 300
[servers.alpha]
port = ${a}
command = ["python3", "${join(dir, "server.py")}", "${a}"]
[servers.beta]
port = ${b}
command = ["python3", "${join(dir, "server.py")}", "${b}"]
`);
const env = { ...process.env, HEARTH_STATE: join(dir, "state"), HEARTH_CONFIG: join(dir, "hearth.toml") };
const hearthd = spawn(join(bin, "hearthd"), [], { env, stdio: "ignore" });
const status = () => {
  try {
    return JSON.parse(execFileSync(join(bin, "hearth"), ["--json", "status"], { env }).toString()).servers;
  } catch { return null; }
};
const stateOf = (name) => (status() || []).find((s) => s.name === name) || {};

const profile = join(dir, "chrome");
const browser = spawn(chrome, [
  "--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check",
  `--user-data-dir=${profile}`, "--remote-debugging-port=0", "about:blank",
], { stdio: "ignore" });

let failed = null;
try {
  await until("both servers ready", () => ["alpha", "beta"].every((n) => stateOf(n).state === "ready"));
  const port = await until("Chrome's debugging port", () => {
    try { return readFileSync(join(profile, "DevToolsActivePort"), "utf8").split("\n")[0]; } catch { return null; }
  });
  const { webSocketDebuggerUrl } = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
  const ws = new WebSocket(webSocketDebuggerUrl);
  await new Promise((ok, no) => { ws.onopen = ok; ws.onerror = no; });
  let next = 0;
  const pending = new Map();
  const problems = [];
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && pending.has(msg.id)) {
      const { ok, no } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? no(new Error(JSON.stringify(msg.error))) : ok(msg.result);
    } else if (msg.method === "Runtime.exceptionThrown") {
      problems.push(msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text);
    } else if (msg.method === "Log.entryAdded" && msg.params.entry.level === "error") {
      problems.push(msg.params.entry.text);
    }
  };
  const send = (method, params = {}, sessionId) => new Promise((ok, no) => {
    const id = ++next;
    pending.set(id, { ok, no });
    ws.send(JSON.stringify({ id, method, params, sessionId }));
  });
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
  const page = (method, params) => send(method, params, sessionId);
  await page("Runtime.enable");
  await page("Log.enable");
  const js = async (expr) => {
    const r = await page("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
    if (r.exceptionDetails) throw new Error(`${expr}: ${r.exceptionDetails.text}`);
    return r.result.value;
  };
  const key = async (k, code = `Key${k.toUpperCase()}`) => {
    for (const type of ["keyDown", "keyUp"]) {
      await page("Input.dispatchKeyEvent", { type, key: k, code, text: type === "keyDown" && k.length === 1 ? k : undefined });
    }
  };

  await page("Emulation.setDeviceMetricsOverride", { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false });
  await page("Page.navigate", { url: `http://127.0.0.1:${dash}/` });
  await until("two server rows", () => js(`document.querySelectorAll(".server").length === 2`));
  const current = () => js(`document.querySelector('.server[aria-current="true"]')?.dataset.name`);
  if (await current() !== "alpha") throw new Error("the first server is not selected at first");

  await key("j");
  await until("j to select beta", async () => (await current()) === "beta");

  const before = stateOf("beta").pid;
  await key("r");
  await until("r to restart beta", () => {
    const s = stateOf("beta");
    return s.state === "ready" && s.pid && s.pid !== before;
  });

  await key("s");
  const armed = await js(`document.querySelector('.server[data-name="beta"] [data-op="stop"]').textContent`);
  if (armed !== "Confirm stop") throw new Error(`one s should arm the stop, button says ${armed}`);
  if (stateOf("beta").state !== "ready") throw new Error("one s stopped the server");
  await key("s");
  await until("s s to stop beta", () => stateOf("beta").state === "stopped");
  await until("the page to show it stopped", () =>
    js(`document.querySelector('.server[data-name="beta"] .state').textContent === "stopped"`));

  await key("S", "KeyS");
  await until("S to start beta", () => stateOf("beta").state === "ready");

  await key("l");
  await until("l to open the output", () =>
    js(`document.getElementById("drawer").classList.contains("open") && document.getElementById("log").textContent.includes("fake server starting")`));
  if (!(await js(`document.getElementById("main").inert`))) throw new Error("the page behind the drawer is not inert");
  await key("Escape", "Escape");
  await until("Escape to close the output", () => js(`!document.getElementById("drawer").classList.contains("open")`));

  await key("?", "Slash");
  await until("? to open the shortcuts", () => js(`document.getElementById("help").open`));
  await js(`document.querySelector("#help button").click()`);
  await until("Done to close the shortcuts", () => js(`!document.getElementById("help").open`));

  await page("Emulation.setDeviceMetricsOverride", { width: 375, height: 800, deviceScaleFactor: 2, mobile: true });
  await sleep(300);
  const wide = await js(`document.documentElement.scrollWidth`);
  if (wide > 375) throw new Error(`the page is ${wide} px wide on a 375 px phone`);

  await page("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
  const motion = await js(`getComputedStyle(document.getElementById("drawer")).transitionDuration`);
  if (!motion.split(",").every((d) => parseFloat(d) <= 0.001)) throw new Error(`reduced motion still animates: ${motion}`);

  if (problems.length) throw new Error(`the page reported: ${problems.join("; ")}`);
  ws.close();
  console.log("dashboard check passed: keys, stop twice, drawer, dialog, 375 px, reduced motion, no errors");
} catch (e) {
  failed = e;
} finally {
  browser.kill();
  hearthd.kill("SIGKILL");
  await sleep(300);
  rmSync(dir, { recursive: true, force: true });
}
if (failed) {
  console.error(`dashboard check failed: ${failed.message}`);
  process.exit(1);
}
