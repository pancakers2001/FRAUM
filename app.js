const archiveUrl = "https://minerva-archive.org";
const pageSize = 100;
const directoryPageSize = 60;
const searchTimeout = 60_000;
const slowSearchNotice = 5_000;
const searchForm = document.querySelector("#search-form");
const queryInput = document.querySelector("#query");
const resultList = document.querySelector("#results");
const searchStatus = document.querySelector("#search-status");
const resultTotal = document.querySelector("#result-total");
const searchEmpty = document.querySelector("#search-empty");
const pagination = document.querySelector("#pagination");
const pageInfo = document.querySelector("#page-info");
const directoryList = document.querySelector("#directory-list");
const directorySearch = document.querySelector("#directory-search");
const breadcrumbs = document.querySelector("#breadcrumbs");
const browseEmpty = document.querySelector("#browse-empty");
const connectionLabel = document.querySelector("#connection-label");
const queueList = document.querySelector("#queue-list");
const queueEmpty = document.querySelector("#queue-empty");
const queueStatus = document.querySelector("#queue-status");
const queueCount = document.querySelector("#queue-count");
const downloadAllButton = document.querySelector("#download-all");
const filters = {
  group: document.querySelector("#group-filter"),
  console: document.querySelector("#console-filter"),
  region: document.querySelector("#region-filter"),
  manufacturer: document.querySelector("#manufacturer-filter"),
  minSize: document.querySelector("#min-size"),
  maxSize: document.querySelector("#max-size"),
  tokenised: document.querySelector("#tokenised-search")
};

let currentOffset = 0;
let currentQuery = "";
let moreResults = false;
let searchTimer;
let requestController;
let directoryPath = "/browse/";
let directoryEntries = [];
let directoryOffset = 0;
const downloadPreparations = new WeakMap();
const queueStorageKey = "fraum-download-queue";
let downloadQueue = loadQueue();

function loadQueue() {
  try {
    const stored = JSON.parse(localStorage.getItem(queueStorageKey) || "[]");
    if (!Array.isArray(stored)) return [];
    return stored.filter(item =>
      Number.isSafeInteger(item.id) && item.id > 0 &&
      typeof item.name === "string" && typeof item.fullPath === "string"
    );
  } catch (error) {
    console.warn("Could not restore the download queue:", error);
    return [];
  }
}

function saveQueue() {
  try {
    localStorage.setItem(queueStorageKey, JSON.stringify(downloadQueue));
  } catch (error) {
    console.error("Could not save the download queue:", error);
    queueStatus.textContent = "Could not save queue changes in this browser.";
  }
}

function renderQueue() {
  queueList.replaceChildren(...downloadQueue.map((item, index) => {
    const card = document.createElement("article");
    card.className = "result-card queue-card";
    const details = document.createElement("div");
    details.className = "result-details";
    const title = document.createElement("div");
    title.className = "result-name";
    title.textContent = item.name;
    const path = document.createElement("div");
    path.className = "result-path";
    path.textContent = item.fullPath;
    details.append(title, path);

    const actions = document.createElement("div");
    actions.className = "result-actions";
    const download = createDownloadLink(item.id, "↓ Download");
    actions.append(download);
    const remove = document.createElement("button");
    remove.className = "queue-remove";
    remove.type = "button";
    remove.dataset.removeQueueId = String(item.id);
    remove.setAttribute("aria-label", `Remove ${item.name} from queue`);
    remove.textContent = "Remove";
    actions.append(remove);
    card.append(details, actions);
    card.style.animationDelay = `${Math.min(index, 12) * 20}ms`;
    return card;
  }));
  queueEmpty.hidden = downloadQueue.length > 0;
  queueCount.textContent = String(downloadQueue.length);
  queueCount.hidden = downloadQueue.length === 0;
  downloadAllButton.disabled = downloadQueue.length === 0;
  if (!downloadAllButton.disabled) queueStatus.textContent = `${downloadQueue.length} file${downloadQueue.length === 1 ? "" : "s"} ready to download.`;
  else if (!downloadAllButton.dataset.downloading) queueStatus.textContent = "Add files from search or browse.";
}

function enqueueDownload(item) {
  if (downloadQueue.some(queued => queued.id === item.id)) {
    queueStatus.textContent = `${item.name} is already in the queue.`;
    return;
  }
  downloadQueue.push(item);
  saveQueue();
  renderQueue();
  queueStatus.textContent = `Added ${item.name} to the queue.`;
}

function removeFromQueue(id) {
  downloadQueue = downloadQueue.filter(item => item.id !== id);
  saveQueue();
  renderQueue();
}

function createDownloadLink(id, label = "↓ Download") {
  const link = document.createElement("a");
  link.className = "download-action";
  link.href = `/api/download?id=${encodeURIComponent(id)}`;
  link.download = "";
  link.dataset.downloadId = String(id);
  link.textContent = label;
  return link;
}

function createItemMenu(item) {
  const menu = document.createElement("details");
  menu.className = "item-menu";
  const toggle = document.createElement("summary");
  toggle.setAttribute("aria-label", `More actions for ${item.name}`);
  toggle.title = "More actions";
  toggle.textContent = "☰";
  menu.append(toggle);

  const popover = document.createElement("div");
  popover.className = "item-menu-popover";
  const add = document.createElement("button");
  add.type = "button";
  add.dataset.addQueueId = String(item.id);
  add.dataset.queuePath = item.fullPath;
  add.textContent = "Add to queue";
  const details = document.createElement("a");
  details.href = `${archiveUrl}/rom?id=${encodeURIComponent(item.id)}`;
  details.target = "_blank";
  details.rel = "noreferrer";
  details.textContent = "View details ↗";
  popover.append(add, details);
  menu.append(popover);
  return menu;
}

async function api(path) {
  const response = await fetch(path);
  const payload = await response.json();
  if (!response.ok) throw new Error(payload.error || `Request failed (${response.status})`);
  return payload;
}

function setConnection(connected) {
  connectionLabel.textContent = connected ? "Live index" : "Archive unavailable";
  connectionLabel.parentElement.classList.toggle("offline", !connected);
}

function populateFilter(select, items) {
  for (const item of items) {
    const option = document.createElement("option");
    option.value = item;
    option.textContent = item;
    select.append(option);
  }
}

function showMessage(message, detail = "") {
  resultList.replaceChildren();
  searchStatus.textContent = message;
  resultTotal.textContent = "";
  searchEmpty.hidden = false;
  searchEmpty.querySelector("span:not(.empty-glyph)").textContent = message;
  searchEmpty.querySelector("small").textContent = detail;
  pagination.hidden = true;
}

function createResultCard(result, index) {
  const card = document.createElement("article");
  card.className = "result-card";
  card.style.animationDelay = `${Math.min(index, 12) * 20}ms`;

  const icon = document.createElement("span");
  icon.className = "result-icon";
  icon.textContent = "⌘";
  icon.setAttribute("aria-hidden", "true");

  const details = document.createElement("div");
  details.className = "result-details";
  const name = result.full_path.split("/").filter(Boolean).at(-1) || result.full_path;
  const title = document.createElement("div");
  title.className = "result-name";
  title.textContent = name;
  const path = document.createElement("div");
  path.className = "result-path";
  path.textContent = result.full_path.replace(/^\.\//, "");
  details.append(title, path);

  const actions = document.createElement("div");
  actions.className = "result-actions";
  actions.append(
    createDownloadLink(result.id),
    createItemMenu({ id: result.id, name, fullPath: result.full_path })
  );
  card.append(icon, details, actions);
  return card;
}

function prepareDownload(link) {
  const existing = downloadPreparations.get(link);
  if (existing) return existing;

  const id = new URL(link.href).searchParams.get("id");
  const preparation = fetch(`/api/download/prepare?id=${encodeURIComponent(id)}`)
    .then(async response => {
      if (response.ok) return;
      const payload = await response.json();
      throw new Error(payload.error || `Could not prepare download (${response.status})`);
    })
    .catch(error => {
      downloadPreparations.delete(link);
      throw error;
    });
  downloadPreparations.set(link, preparation);
  return preparation;
}

function warmDownload(link) {
  void prepareDownload(link).catch(error => {
    link.title = `Could not prepare download: ${error.message}`;
  });
}

function showDownloadFeedback(link) {
  if (link.dataset.busy) return;
  const originalLabel = link.textContent;
  link.dataset.busy = "true";
  link.setAttribute("aria-busy", "true");
  link.textContent = "Preparing…";

  const reset = () => {
    delete link.dataset.busy;
    link.removeAttribute("aria-busy");
    link.textContent = originalLabel;
  };
  setTimeout(() => {
    if (link.dataset.busy) link.textContent = "Starting download…";
  }, 1_500);
  setTimeout(reset, 8_000);
}

async function search() {
  const query = queryInput.value.trim();
  currentQuery = query;
  if (requestController) requestController.abort();
  if (query.length < 3) {
    resultList.replaceChildren();
    resultTotal.textContent = "";
    pagination.hidden = true;
    searchEmpty.hidden = false;
    searchStatus.textContent = query.length ? "A little more to go." : "Start with a title, system, or keyword.";
    searchEmpty.querySelector("span:not(.empty-glyph)").textContent = query.length ? "A little more to go." : "Good things take a moment.";
    searchEmpty.querySelector("small").textContent = "Enter at least three characters to begin your search.";
    return;
  }

  const params = new URLSearchParams({ query: query.toLowerCase(), offset: String(currentOffset) });
  if (filters.tokenised.checked) params.set("tokenised", "true");
  if (filters.minSize.value && Number(filters.minSize.value) > 0) params.set("min_size", filters.minSize.value);
  if (filters.maxSize.value && Number(filters.maxSize.value) > 0) params.set("max_size", filters.maxSize.value);
  for (const key of ["group", "console", "region", "manufacturer"]) {
    if (filters[key].value) params.set(key, filters[key].value);
  }

  requestController = new AbortController();
  const controller = requestController;
  const hasActiveFilters = Object.values(filters).some(control =>
    control.type === "checkbox" ? control.checked : control.value
  );
  const slowSearchTimer = setTimeout(() => {
    if (!controller.signal.aborted && requestController === controller) {
      searchStatus.textContent = hasActiveFilters
        ? "Still narrowing results in MiNERVA’s index…"
        : "Still searching MiNERVA’s index…";
    }
  }, slowSearchNotice);
  const timeoutTimer = setTimeout(() => {
    controller.abort(new DOMException("Search timed out.", "TimeoutError"));
  }, searchTimeout);
  searchStatus.textContent = "Searching the archive…";
  searchEmpty.hidden = true;
  pagination.hidden = true;
  resultList.replaceChildren();
  try {
    const response = await fetch(`/api/search?${params}`, { signal: controller.signal });
    const payload = await response.json();
    if (!response.ok) throw new Error(payload.error || `Search failed (${response.status})`);
    if (query !== currentQuery) return;

    moreResults = payload.length > pageSize;
    const records = moreResults ? payload.slice(0, pageSize) : payload;
    resultList.replaceChildren(...records.map(createResultCard));
    searchEmpty.hidden = records.length > 0;
    resultTotal.textContent = records.length ? `${records.length}${moreResults ? "+" : ""} RECORDS` : "";
    searchStatus.textContent = records.length
      ? `Showing ${currentOffset + 1}–${currentOffset + records.length} for “${query}”`
      : "No records found for that search.";
    if (!records.length) {
      searchEmpty.querySelector("span:not(.empty-glyph)").textContent = "No records found.";
      searchEmpty.querySelector("small").textContent = "Try another title or broaden your filters.";
    }
    pagination.hidden = !moreResults && currentOffset === 0;
    pageInfo.textContent = `Page ${Math.floor(currentOffset / pageSize) + 1}`;
    document.querySelector("#prev-page").disabled = currentOffset === 0;
    document.querySelector("#next-page").disabled = !moreResults;
    setConnection(true);
  } catch (error) {
    const timedOut = controller.signal.reason?.name === "TimeoutError";
    if (error.name === "AbortError" && !timedOut) return;
    searchStatus.textContent = timedOut ? "The archive search timed out." : "The archive could not be reached.";
    searchEmpty.hidden = false;
    searchEmpty.querySelector("span:not(.empty-glyph)").textContent = timedOut
      ? "MiNERVA is taking longer than usual."
      : "The archive is unavailable.";
    searchEmpty.querySelector("small").textContent = timedOut
      ? "Try the search again in a moment."
      : error.message;
    setConnection(false);
  } finally {
    clearTimeout(slowSearchTimer);
    clearTimeout(timeoutTimer);
  }
}

function scheduleSearch(resetPage = true, delay = 300) {
  clearTimeout(searchTimer);
  if (resetPage) currentOffset = 0;
  searchTimer = setTimeout(search, delay);
}

function renderBreadcrumbs(items) {
  breadcrumbs.replaceChildren();
  items.forEach((item, index) => {
    if (index) {
      const separator = document.createElement("span");
      separator.textContent = "/";
      breadcrumbs.append(separator);
    }
    const crumb = document.createElement("button");
    crumb.type = "button";
    crumb.textContent = item.name;
    crumb.addEventListener("click", () => loadDirectory(item.path));
    breadcrumbs.append(crumb);
  });
}

function renderDirectory(entries) {
  directoryList.replaceChildren();
  browseEmpty.hidden = entries.length > 0;
  const pageEntries = entries.slice(directoryOffset, directoryOffset + directoryPageSize);
  for (const entry of pageEntries) {
    const row = document.createElement("div");
    row.className = "directory-entry";
    const icon = document.createElement("span");
    icon.className = "folder-icon";
    icon.textContent = entry.isDirectory ? "▱" : "◫";
    icon.setAttribute("aria-hidden", "true");
    const name = entry.isDirectory ? document.createElement("button") : document.createElement("span");
    if (entry.isDirectory) {
      name.type = "button";
      name.addEventListener("click", () => loadDirectory(entry.path));
    }
    name.className = `directory-name${entry.isDirectory ? "" : " directory-file-name"}`;
    name.textContent = entry.name;
    name.title = entry.name;
    row.append(icon, name);
    if (entry.size) {
      const size = document.createElement("span");
      size.className = "directory-size";
      size.textContent = entry.size;
      row.append(size);
    }
    if (!entry.isDirectory && entry.id) {
      const actions = document.createElement("div");
      actions.className = "result-actions";
      const download = createDownloadLink(entry.id);
      download.setAttribute("aria-label", `Download ${entry.name}`);
      actions.append(
        download,
        createItemMenu({ id: entry.id, name: entry.name, fullPath: entry.path })
      );
      row.append(actions);
    }
    directoryList.append(row);
  }
  const directoryPagination = document.querySelector("#directory-pagination");
  directoryPagination.hidden = entries.length <= directoryPageSize;
  document.querySelector("#directory-page-info").textContent = `Page ${Math.floor(directoryOffset / directoryPageSize) + 1} · ${entries.length} entries`;
  document.querySelector("#directory-prev").disabled = directoryOffset === 0;
  document.querySelector("#directory-next").disabled = directoryOffset + directoryPageSize >= entries.length;
}

async function loadDirectory(path) {
  directoryPath = path;
  directoryOffset = 0;
  directorySearch.value = "";
  directoryList.innerHTML = '<div class="empty-state">Opening collection…</div>';
  try {
    const directory = await api(`/api/directory?path=${encodeURIComponent(path)}`);
    directoryEntries = directory.entries;
    renderBreadcrumbs(directory.breadcrumbs);
    filterDirectory();
    setConnection(true);
  } catch (error) {
    directoryEntries = [];
    renderDirectory([]);
    browseEmpty.querySelector("small").textContent = error.message;
    setConnection(false);
  }
}

function filterDirectory() {
  const filter = directorySearch.value.trim().toLocaleLowerCase();
  const visibleEntries = directoryEntries.filter(entry => entry.name.toLocaleLowerCase().includes(filter));
  directoryOffset = 0;
  renderDirectory(visibleEntries);
}

function openView(viewName) {
  for (const button of document.querySelectorAll(".nav-link")) {
    button.classList.toggle("active", button.dataset.view === viewName);
    button.setAttribute("aria-current", button.dataset.view === viewName ? "page" : "false");
  }
  const headings = {
    search: ["THE INDEX", "Search the archive"],
    browse: ["THE COLLECTIONS", "Browse collections"],
    queue: ["YOUR DOWNLOADS", "Download queue"]
  };
  document.querySelector("#section-kicker").textContent = headings[viewName][0];
  document.querySelector("#section-title").textContent = headings[viewName][1];
  document.querySelector("#search-view").hidden = viewName !== "search";
  document.querySelector("#browse-view").hidden = viewName !== "browse";
  document.querySelector("#queue-view").hidden = viewName !== "queue";
  if (viewName === "browse" && directoryEntries.length === 0) loadDirectory(directoryPath);
}

searchForm.addEventListener("submit", event => {
  event.preventDefault();
  clearTimeout(searchTimer);
  currentOffset = 0;
  search();
});
document.addEventListener("pointerover", event => {
  const link = event.target.closest?.("a.download-action");
  if (link) warmDownload(link);
});
document.addEventListener("focusin", event => {
  const link = event.target.closest?.("a.download-action");
  if (link) warmDownload(link);
});
document.addEventListener("click", event => {
  const addButton = event.target.closest?.("[data-add-queue-id]");
  if (addButton) {
    const row = addButton.closest(".result-card, .directory-entry");
    const id = Number(addButton.dataset.addQueueId);
    const itemName = row?.querySelector(".result-name, .directory-file-name")?.textContent;
    const itemPath = addButton.dataset.queuePath || row?.querySelector(".result-path")?.textContent || itemName;
    const item = itemName && { id, name: itemName, fullPath: itemPath };
    if (item) enqueueDownload(item);
    else queueStatus.textContent = "Could not find this file to add it.";
    addButton.closest("details")?.removeAttribute("open");
    return;
  }

  const removeButton = event.target.closest?.("[data-remove-queue-id]");
  if (removeButton) {
    removeFromQueue(Number(removeButton.dataset.removeQueueId));
    return;
  }

  const link = event.target.closest?.("a.download-action");
  if (!link) return;
  if (link.dataset.busy) {
    event.preventDefault();
    return;
  }
  showDownloadFeedback(link);
});
downloadAllButton.addEventListener("click", () => {
  const queuedItems = [...downloadQueue];
  if (!queuedItems.length || downloadAllButton.dataset.downloading) return;
  downloadAllButton.dataset.downloading = "true";
  downloadAllButton.disabled = true;
  const downloadLinks = queuedItems.map(item =>
    queueList.querySelector(`a[data-download-id="${item.id}"]`)
  ).filter(Boolean);
  for (const link of downloadLinks) {
    link.click();
  }
  delete downloadAllButton.dataset.downloading;
  downloadAllButton.disabled = downloadQueue.length === 0;
  if (downloadLinks.length === 0) {
    queueStatus.textContent = "Could not find any download links in the queue.";
    return;
  }
  queueStatus.textContent = `Sent ${downloadLinks.length} download request${downloadLinks.length === 1 ? "" : "s"} to your browser. Items remain queued until you remove them.`;
});
queryInput.addEventListener("input", () => scheduleSearch());
for (const control of Object.values(filters)) control.addEventListener("change", () => scheduleSearch(true, 900));
directorySearch.addEventListener("input", filterDirectory);
document.querySelector("#directory-next").addEventListener("click", () => {
  directoryOffset += directoryPageSize;
  const filter = directorySearch.value.trim().toLocaleLowerCase();
  renderDirectory(directoryEntries.filter(entry => entry.name.toLocaleLowerCase().includes(filter)));
});
document.querySelector("#directory-prev").addEventListener("click", () => {
  directoryOffset = Math.max(0, directoryOffset - directoryPageSize);
  const filter = directorySearch.value.trim().toLocaleLowerCase();
  renderDirectory(directoryEntries.filter(entry => entry.name.toLocaleLowerCase().includes(filter)));
});
document.querySelector("#next-page").addEventListener("click", () => {
  if (!moreResults) return;
  currentOffset += pageSize;
  search();
});
document.querySelector("#prev-page").addEventListener("click", () => {
  currentOffset = Math.max(0, currentOffset - pageSize);
  search();
});
for (const button of document.querySelectorAll(".nav-link")) {
  button.addEventListener("click", () => openView(button.dataset.view));
}

try {
  renderQueue();
  const availableFilters = await api("/api/filters");
  populateFilter(filters.group, availableFilters.groups);
  populateFilter(filters.console, availableFilters.consoles);
  populateFilter(filters.region, availableFilters.regions);
  populateFilter(filters.manufacturer, availableFilters.manufacturers);
  setConnection(true);
} catch (error) {
  setConnection(false);
  showMessage("The archive is taking a moment.", error.message);
}
