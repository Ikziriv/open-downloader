const COMMON = [
  "zip", "rar", "7z", "exe",
  "msi", "iso", "apk", "mp4",
  "mkv", "avi", "mov", "webm",
  "mp3", "wav", "flac", "pdf",
  "docx", "xlsx", "pptx", "gz",
];

const DEFAULTS = {
  enabled: true,
  mode: "all",      // "all" | "list"
  exts: COMMON,
  minSizeMB: 0,
  excludedHosts: [],
};

const $ = (id) => document.getElementById(id);
const enabledEl = $("enabled");
const customEl = $("custom");
const minSizeEl = $("minSize");
const exclEl = $("exclusions");
const gridEl = $("ext-grid");

const checks = COMMON.map((ext) => {
  const label = document.createElement("label");
  label.innerHTML = `<input type="checkbox" value="${ext}"> ${ext}`;
  gridEl.appendChild(label);
  return label.firstChild;
});

function parseCustom(text) {
  return text
    .split(/[,\s]+/)
    .map((s) => s.replace(/^\./, "").toLowerCase())
    .filter(Boolean);
}

function save() {
  const mode = document.querySelector('input[name="mode"]:checked').value;
  const exts = [
    ...checks.filter((c) => c.checked).map((c) => c.value),
    ...parseCustom(customEl.value),
  ];
  chrome.storage.local.set({
    enabled: enabledEl.checked,
    mode,
    exts: [...new Set(exts)],
    minSizeMB: Math.max(0, parseFloat(minSizeEl.value) || 0),
    excludedHosts: [
      ...new Set(
        exclEl.value
          .split(/[\n,]+/)
          .map((h) => h.trim().toLowerCase())
          .filter(Boolean)
      ),
    ],
  });
  refresh();
}

function refresh() {
  const mode = document.querySelector('input[name="mode"]:checked').value;
  document
    .getElementById("filter-card")
    .classList.toggle("disabled-section", !enabledEl.checked);
  gridEl.classList.toggle("dim", mode !== "list");
  customEl.classList.toggle("dim", mode !== "list");
  document
    .getElementById("size-card")
    .classList.toggle("disabled-section", !enabledEl.checked);
}

chrome.storage.local.get(DEFAULTS, (s) => {
  enabledEl.checked = s.enabled;
  document.querySelector(`input[name="mode"][value="${s.mode}"]`).checked = true;
  const set = new Set(s.exts);
  checks.forEach((c) => (c.checked = set.has(c.value)));
  customEl.value = s.exts.filter((e) => !COMMON.includes(e)).join(", ");
  minSizeEl.value = s.minSizeMB;
  exclEl.value = (s.excludedHosts || []).join("\n");
  refresh();
});

// "+ this site" — add the active tab's host to the exclusion list.
$("exclude-site").addEventListener("click", () => {
  chrome.tabs.query({ active: true, currentWindow: true }, ([tab]) => {
    let host = "";
    try {
      host = new URL(tab.url).hostname;
    } catch {
      return;
    }
    if (!host) return;
    const cur = exclEl.value
      .split(/[\n,]+/)
      .map((h) => h.trim().toLowerCase())
      .filter(Boolean);
    if (!cur.includes(host)) exclEl.value = [...cur, host].join("\n");
    save();
  });
});

// Media sniffer list for the active tab.
chrome.tabs.query({ active: true, currentWindow: true }, ([tab]) => {
  if (!tab || tab.id < 0) return;
  chrome.runtime.sendMessage({ type: "media", tabId: tab.id }, (list) => {
    if (chrome.runtime.lastError || !list || !list.length) return;
    const card = $("media-card");
    card.style.display = "";
    const listEl = $("media-list");
    for (const m of list) {
      const item = document.createElement("div");
      item.className = "media-item";
      let name = m.url;
      try {
        name = new URL(m.url).pathname.split("/").pop() || m.url;
      } catch {}
      const mime = (m.mime || "").replace(/^(video|audio)\//, "");
      const nameEl = document.createElement("span");
      nameEl.className = "name";
      nameEl.title = m.url;
      nameEl.textContent = name || m.url;
      const mimeEl = document.createElement("span");
      mimeEl.className = "mime";
      mimeEl.textContent = mime;
      item.append(nameEl, mimeEl);
      const btn = document.createElement("button");
      btn.textContent = "Get";
      btn.addEventListener("click", () => {
        chrome.runtime.sendMessage({
          type: "grab",
          url: m.url,
          referer: m.ref,
        });
        btn.textContent = "sent";
        btn.classList.add("sent");
        btn.disabled = true;
      });
      item.appendChild(btn);
      listEl.appendChild(item);
    }
  });
});

document.body.addEventListener("change", save);
customEl.addEventListener("input", save);
minSizeEl.addEventListener("input", save);
exclEl.addEventListener("input", save);
