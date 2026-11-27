const HOST = "com.opendownloader.nmh";

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({
    id: "od-send-link",
    title: "Download with Open Downloader",
    contexts: ["link"],
  });
});

function sendDownload(url, filename, referer) {
  chrome.cookies.getAll({ url }, (cookies) => {
    const cookie = chrome.runtime.lastError
      ? ""
      : cookies.map((c) => `${c.name}=${c.value}`).join("; ");
    chrome.runtime.sendNativeMessage(HOST, {
      type: "add",
      url,
      filename: filename || undefined,
      cookie,
      referer: referer || undefined,
    });
  });
}

chrome.contextMenus.onClicked.addListener((info) => {
  if (info.menuItemId === "od-send-link" && /^https?:/.test(info.linkUrl || "")) {
    sendDownload(info.linkUrl, undefined, info.pageUrl);
  }
});

// Capture settings from the toolbar popup (chrome.storage.local).
const DEFAULTS = {
  enabled: true,
  mode: "all",        // "all" | "list"
  exts: [],           // extensions captured when mode === "list"
  minSizeMB: 0,       // skip files smaller than this; 0 = capture any size
  excludedHosts: [],  // never capture from these domains (subdomains too)
};

function extOf(item) {
  const name =
    item.filename || item.url.split("?")[0].split("/").pop() || "";
  const i = name.lastIndexOf(".");
  return i > 0 ? name.slice(i + 1).toLowerCase() : "";
}

// Exclusions match the site the download came from (referrer), falling
// back to the file host when the browser reports no referrer.
function hostExcluded(item, s) {
  try {
    const host = new URL(item.referrer || item.url).hostname;
    return (s.excludedHosts || []).some(
      (d) => host === d || host.endsWith("." + d)
    );
  } catch {
    return false;
  }
}

function shouldCapture(item, s) {
  if (!s.enabled) return false;
  if (hostExcluded(item, s)) return false;
  if (s.mode === "list" && !s.exts.includes(extOf(item))) return false;
  if (
    s.minSizeMB > 0 &&
    item.fileSize > 0 &&
    item.fileSize < s.minSizeMB * 1048576
  )
    return false;
  return true;
}

// Intercept downloads the way IDM does: hand the URL (plus cookies so
// authenticated downloads still work) to the app, then cancel the browser's
// own download. If the host is missing we call suggest() and the browser
// proceeds normally. Settings are read per event — a service worker can
// wake with this listener firing before any module-level cache is warm.
chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  if (!/^https?:/.test(item.url) || item.byExtensionId === chrome.runtime.id) {
    return;
  }
  chrome.storage.local.get(DEFAULTS, (s) => {
    if (!shouldCapture(item, s)) {
      suggest();
      return;
    }
    chrome.cookies.getAll({ url: item.url }, (cookies) => {
    const cookie = chrome.runtime.lastError
      ? ""
      : cookies.map((c) => `${c.name}=${c.value}`).join("; ");
    chrome.runtime.sendNativeMessage(
      HOST,
      {
        type: "add",
        url: item.url,
        filename: item.filename,
        cookie,
        referer: item.referrer,
      },
      (resp) => {
        if (chrome.runtime.lastError || !resp || !resp.ok) {
          suggest();
          return;
        }
        chrome.downloads.cancel(item.id, () => chrome.downloads.erase(item.id));
      }
    );
    });
  });
  return true; // keep suggest() usable asynchronously
});

// --- Media sniffing -------------------------------------------------------
// Remember recent video/audio requests per tab so the popup can offer them
// for download (IDM's "grab videos on this page", minus in-page injection).
const mediaSeen = new Map(); // tabId -> [{url, mime, ref, ts}]
const MAX_MEDIA = 12;
const MEDIA_EXT = /\.(mp4|webm|mkv|mov|m3u8|mpd|mp3|m4a|ogg|opus|flac)(\?|#|$)/i;
const MEDIA_MIME = /^(video|audio)\/|mpegurl|dash\+xml/i;
// HLS/Dash segment types that would flood the list with chunks.
const SEGMENT = /\.(ts|m4s|m4f)(\?|#|$)|mp2t|mpegts/i;

function noteMedia(tabId, url, mime, ref) {
  if (tabId < 0 || !/^https?:/.test(url) || SEGMENT.test(url) || SEGMENT.test(mime))
    return;
  const list = mediaSeen.get(tabId) || [];
  if (list.some((m) => m.url === url)) return;
  list.unshift({ url, mime, ref, ts: Date.now() });
  if (list.length > MAX_MEDIA) list.pop();
  mediaSeen.set(tabId, list);
  chrome.action.setBadgeText({ tabId, text: String(list.length) });
  chrome.action.setBadgeBackgroundColor({ tabId, color: "#3b6ef0" });
}

chrome.webRequest.onCompleted.addListener(
  (d) => {
    const mime =
      (d.responseHeaders || []).find(
        (h) => h.name.toLowerCase() === "content-type"
      )?.value || "";
    if (MEDIA_MIME.test(mime) || MEDIA_EXT.test(d.url)) {
      noteMedia(d.tabId, d.url, mime.split(";")[0], d.initiator || "");
    }
  },
  {
    urls: ["http://*/*", "https://*/*"],
    types: ["media", "xmlhttprequest", "other"],
  }
);

chrome.tabs.onRemoved.addListener((tabId) => mediaSeen.delete(tabId));

// Popup asks for the tab's media list, or grabs one of the items.
chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg.type === "media") {
    sendResponse(mediaSeen.get(msg.tabId) || []);
    return true;
  }
  if (msg.type === "grab" && /^https?:/.test(msg.url || "")) {
    sendDownload(msg.url, msg.filename, msg.referer);
    sendResponse({ ok: true });
    return true;
  }
});
