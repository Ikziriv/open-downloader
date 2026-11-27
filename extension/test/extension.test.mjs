// Node harness for extension/background.js — stubs the chrome.* APIs,
// runs the real file in a vm, and exercises the capture decision paths.
// Run: node extension/test/extension.test.mjs
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import vm from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, "..", "background.js"), "utf8");

let store, calls, nativeReply;
const listeners = {};

// Resets mutable state; listeners stay registered — background.js binds
// them once at load.
function fresh() {
  store = {};
  calls = { native: [], cancel: [], erase: [], badge: [], suggest: 0 };
  nativeReply = { ok: true };
}
fresh();

const chrome = {
  runtime: {
    id: "test-ext",
    lastError: null,
    onInstalled: { addListener() {} },
    onMessage: { addListener: (f) => (listeners.message = f) },
    sendNativeMessage: (_host, msg, cb) => {
      calls.native.push(msg);
      if (cb) cb(nativeReply);
    },
    sendMessage() {},
  },
  contextMenus: {
    create() {},
    onClicked: { addListener() {} },
  },
  cookies: { getAll: (_q, cb) => cb([{ name: "sess", value: "tok" }]) },
  downloads: {
    onDeterminingFilename: { addListener: (f) => (listeners.det = f) },
    cancel: (id, cb) => {
      calls.cancel.push(id);
      cb && cb();
    },
    erase: (id) => calls.erase.push(id),
  },
  storage: {
    local: {
      get: (d, cb) => cb({ ...d, ...store }),
      set: (o) => Object.assign(store, o),
    },
    onChanged: { addListener() {} },
  },
  tabs: {
    onRemoved: { addListener: (f) => (listeners.tabRemoved = f) },
    query: () => {},
  },
  action: {
    setBadgeText: (o) => calls.badge.push(o),
    setBadgeBackgroundColor() {},
  },
  webRequest: { onCompleted: { addListener: (f) => (listeners.req = f) } },
};

// URL is a host object (not a JS intrinsic) — the browser provides it.
const sandbox = { chrome, console, URL };
vm.createContext(sandbox);
vm.runInContext(src, sandbox);

let pass = 0,
  fail = 0;
const t = (name, cond) => {
  if (cond) pass++;
  else {
    fail++;
    console.error("FAIL:", name);
  }
};

const item = (over = {}) => ({
  url: "https://cdn.example.com/file.zip",
  filename: "file.zip",
  fileSize: 5 * 1048576,
  referrer: "https://example.com/page",
  id: 7,
  ...over,
});
const suggest = () => calls.suggest++;

// --- capture path ---------------------------------------------------------
fresh();
listeners.det(item(), suggest);
t("captures to native host", calls.native.length === 1);
t("forwards url", calls.native[0].url === item().url);
t("forwards cookies", calls.native[0].cookie === "sess=tok");
t("forwards referer", calls.native[0].referer === "https://example.com/page");
t("cancels browser download", calls.cancel[0] === 7);
t("erases record", calls.erase[0] === 7);
t("no suggest on capture", calls.suggest === 0);

fresh();
store.enabled = false;
listeners.det(item(), suggest);
t("disabled → browser keeps it", calls.suggest === 1 && !calls.native.length);

fresh();
listeners.det(item({ url: "ftp://x/f.zip" }), suggest);
t("non-http url ignored", !calls.native.length && calls.suggest === 0);
listeners.det(item({ byExtensionId: "test-ext" }), suggest);
t("own downloads ignored", calls.suggest === 0);

// --- type filter ----------------------------------------------------------
fresh();
store.mode = "list";
store.exts = ["zip"];
listeners.det(item(), suggest);
t("listed ext captured", calls.native.length === 1);
fresh();
store.mode = "list";
store.exts = ["zip"];
listeners.det(item({ filename: "pic.png" }), suggest);
t("unlisted ext passed through", calls.suggest === 1);
fresh();
store.mode = "list";
store.exts = ["zip"];
listeners.det(item({ filename: "", url: "https://x/y.zip?a=1" }), suggest);
t("url ext fallback", calls.native.length === 1);

// --- size filter ----------------------------------------------------------
fresh();
store.minSizeMB = 10;
listeners.det(item({ fileSize: 5 * 1048576 }), suggest);
t("small file skipped", calls.suggest === 1);
fresh();
store.minSizeMB = 10;
listeners.det(item({ fileSize: -1 }), suggest);
t("unknown size still captures", calls.native.length === 1);

// --- exclusions -----------------------------------------------------------
fresh();
store.excludedHosts = ["example.com"];
listeners.det(item(), suggest);
t("excluded referrer host", calls.suggest === 1);
fresh();
store.excludedHosts = ["example.com"];
listeners.det(item({ referrer: "https://www.example.com/p" }), suggest);
t("excluded subdomain", calls.suggest === 1);
fresh();
store.excludedHosts = ["example.com"];
listeners.det(item({ referrer: "https://other.org/p" }), suggest);
t("other host captures", calls.native.length === 1);
fresh();
store.excludedHosts = ["example.com"];
listeners.det(item({ referrer: "" }), suggest);
t("empty referrer falls back to url host", calls.suggest === 1);

// --- host down fallback ---------------------------------------------------
fresh();
nativeReply = undefined;
listeners.det(item(), suggest);
t("host missing → suggest", calls.suggest === 1 && !calls.cancel.length);

// --- media sniffing -------------------------------------------------------
fresh();
const req = (url, mime, tabId = 1) =>
  listeners.req({
    tabId,
    url,
    initiator: "https://v.example.com",
    responseHeaders: mime ? [{ name: "Content-Type", value: mime }] : [],
  });
req("https://v.example.com/a.mp4", "video/mp4");
req("https://v.example.com/a.mp4", "video/mp4"); // dup
req("https://v.example.com/seg1.ts", "video/mp2t"); // hls segment
req("https://v.example.com/pl.m3u8", "application/vnd.apple.mpegurl");
req("https://v.example.com/doc.pdf", "application/pdf");
let media;
listeners.message({ type: "media", tabId: 1 }, null, (r) => (media = r));
t("media list has mp4+m3u8", media.length === 2);
t("dedupes + skips segments+pdf", !media.some((m) => m.url.includes("seg1")));
t("badge set", calls.badge.length === 2);
t("keeps initiator as referer", media[0].ref === "https://v.example.com");

fresh();
req("https://v.example.com/x.mp4", "", 2);
listeners.message({ type: "grab", url: "https://v.example.com/x.mp4", referer: "https://v.example.com" }, null, () => {});
t("grab forwards to native", calls.native[0]?.url.includes("x.mp4"));
listeners.tabRemoved(2);
listeners.message({ type: "media", tabId: 2 }, null, (r) => (media = r));
t("tab close clears media", media.length === 0);

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
