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
  const recordLink = document.createElement("a");
  recordLink.href = `${archiveUrl}/rom?id=${encodeURIComponent(result.id)}`;
  recordLink.target = "_blank";
  recordLink.rel = "noreferrer";
  recordLink.textContent = "Details ↗";
  actions.append(recordLink);
  const downloadLink = document.createElement("a");
  downloadLink.className = "download-action";
  downloadLink.href = `/api/download?id=${encodeURIComponent(result.id)}`;
  downloadLink.download = "";
  downloadLink.textContent = "↓ Download";
  actions.append(downloadLink);
  card.append(icon, details, actions);
  return card;
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
      const download = document.createElement("a");
      download.className = "download-action";
      download.href = `/api/download?id=${encodeURIComponent(entry.id)}`;
      download.download = "";
      download.setAttribute("aria-label", `Download ${entry.name}`);
      download.textContent = "↓ Download";
      row.append(download);
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
  document.querySelector("#section-kicker").textContent = viewName === "browse" ? "THE COLLECTIONS" : "THE INDEX";
  document.querySelector("#section-title").textContent = viewName === "browse" ? "Browse collections" : "Search the archive";
  document.querySelector("#search-view").hidden = viewName !== "search";
  document.querySelector("#browse-view").hidden = viewName !== "browse";
  if (viewName === "browse" && directoryEntries.length === 0) loadDirectory(directoryPath);
}

searchForm.addEventListener("submit", event => {
  event.preventDefault();
  clearTimeout(searchTimer);
  currentOffset = 0;
  search();
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
