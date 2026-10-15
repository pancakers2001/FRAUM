import { createServer } from "node:http";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import WebTorrent from "webtorrent";

const root = fileURLToPath(new URL(".", import.meta.url));
const archiveOrigin = "https://minerva-archive.org";
const port = Number(process.env.PORT || 4173);
const mimeTypes = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml"
};

function decodeHtml(value) {
  return value
    .replace(/&amp;/g, "&")
    .replace(/&quot;/g, '"')
    .replace(/&#39;|&apos;/g, "'")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&#(\d+);/g, (_, code) => String.fromCodePoint(Number(code)))
    .replace(/&#x([\da-f]+);/gi, (_, code) => String.fromCodePoint(parseInt(code, 16)));
}

export function parseDirectory(html, directoryPath) {
  const entries = [];
  const entryPattern = /<div class="entry" data-name="([^"]*)"[^>]*>([\s\S]*?)<\/div>/g;

  for (const match of html.matchAll(entryPattern)) {
    const [, sortName, markup] = match;
    const linkMatch = markup.match(/<a href="([^"]+)"[^>]*>([\s\S]*?)<\/a>/);
    if (!linkMatch) continue;

    const [, href, labelMarkup] = linkMatch;
    const name = decodeHtml(labelMarkup.replace(/<[^>]+>/g, "").trim());
    const sizeMatch = markup.match(/<span>([^<]*)<\/span>/);
    const target = new URL(decodeHtml(href), archiveOrigin);
    const romId = target.pathname === "/rom" ? Number(target.searchParams.get("id")) : null;
    const targetPath = `${target.pathname}${target.search}`;

    entries.push({
      name,
      sortName: decodeHtml(sortName),
      path: targetPath,
      isDirectory: name.endsWith("/"),
      size: sizeMatch ? decodeHtml(sizeMatch[1]) : null,
      id: Number.isSafeInteger(romId) && romId > 0 ? romId : null
    });
  }

  const parentMatch = html.match(/<div class="entry search_back">\s*<a href="([^"]+)"/);
  const breadcrumbs = [{ name: "Root", path: "/browse/" }];
  const pathParts = decodeURIComponent(new URL(directoryPath, archiveOrigin).pathname)
    .replace(/^\/browse\/\.?\/?/, "")
    .split("/")
    .filter(Boolean);
  let accumulated = "/browse/";

  for (const part of pathParts) {
    accumulated += `${encodeURIComponent(part)}/`;
    breadcrumbs.push({ name: part, path: accumulated });
  }

  return {
    entries,
    parent: parentMatch ? new URL(decodeHtml(parentMatch[1]), archiveOrigin).pathname : null,
    breadcrumbs
  };
}

export function parseRomDetails(html) {
  const match = html.match(/window\.rom\s*=\s*(\{[^;]+\});/);
  if (!match) throw new Error("Archive ROM details are missing.");

  let rom;
  try {
    rom = JSON.parse(match[1]);
  } catch {
    throw new Error("Archive returned invalid ROM details.");
  }

  if (!Number.isSafeInteger(rom.id) || typeof rom.file_name !== "string" ||
      typeof rom.full_path !== "string" || typeof rom.torrents !== "string" ||
      !Number.isSafeInteger(rom.so_id) || rom.so_id < 0) {
    throw new Error("Archive ROM details are incomplete.");
  }

  return rom;
}

async function fetchArchive(path) {
  const response = await fetch(new URL(path, archiveOrigin));
  if (!response.ok) {
    throw new Error(`Archive returned ${response.status}`);
  }
  return response;
}

async function getRomTorrent(id) {
  const detailsResponse = await fetchArchive(`/rom?id=${id}`);
  const rom = parseRomDetails(await detailsResponse.text());
  if (rom.id !== id) throw new Error("Archive returned a different ROM record.");

  const torrentParts = rom.torrents.split("/");
  if (torrentParts.some(part => !part || part === "." || part === "..")) {
    throw new Error("Archive returned an invalid torrent path.");
  }
  const torrentUrl = `/assets/${torrentParts.map(encodeURIComponent).join("/")}`;
  const torrentResponse = await fetchArchive(torrentUrl);
  const torrent = Buffer.from(await torrentResponse.arrayBuffer());
  return {
    torrent,
    selectFile: files => {
      const file = files[rom.so_id];
      if (!file || file.name !== rom.file_name || file.length !== rom.size) {
        throw new Error("The requested ROM does not match its torrent entry.");
      }
      return file;
    }
  };
}

function streamTorrentFile(torrentSource, selectFile, response) {
  let client;
  let fileStream;
  let downloadPath;
  let cleanupPromise;
  let failureHandled = false;

  const cleanup = () => {
    if (cleanupPromise) return cleanupPromise;
    cleanupPromise = (async () => {
      if (client && !client.destroyed) {
        await new Promise(resolveDestroy => client.destroy(resolveDestroy));
      }
      if (downloadPath) await rm(downloadPath, { recursive: true, force: true });
    })();
    return cleanupPromise;
  };

  const fail = error => {
    if (failureHandled) return;
    failureHandled = true;
    console.error("Torrent download failed:", error);
    if (!response.headersSent) {
      response.writeHead(502, { "Content-Type": "application/json; charset=utf-8" });
      response.end(JSON.stringify({ error: error.message || "Could not download this archive file." }));
    } else {
      response.destroy(error);
    }
    void cleanup();
  };

  response.once("close", () => {
    if (!response.writableFinished) fileStream?.destroy();
    void cleanup();
  });
  response.once("finish", () => void cleanup());

  void mkdtemp(join(tmpdir(), "fraum-download-")).then(path => {
    downloadPath = path;
    if (response.destroyed) {
      return cleanup();
    }
    client = new WebTorrent();
    client.once("error", fail);

    const torrent = client.add(torrentSource, { path, deselect: true }, readyTorrent => {
      try {
        const file = selectFile(readyTorrent.files);
        file.select();
        const safeName = file.name.replace(/[\r\n"]/g, "_");
        const encodedName = encodeURIComponent(file.name).replace(/[!'()*]/g, char =>
          `%${char.charCodeAt(0).toString(16).toUpperCase()}`
        );
        response.writeHead(200, {
          "Content-Type": "application/octet-stream",
          "Content-Length": String(file.length),
          "Content-Disposition": `attachment; filename="${safeName}"; filename*=UTF-8''${encodedName}`,
          "Cache-Control": "no-store"
        });

        fileStream = file.createReadStream();
        fileStream.once("error", fail);
        fileStream.pipe(response);
      } catch (error) {
        fail(error);
      }
    });
    torrent.once("error", fail);
  }).catch(fail);
}

async function handleApi(requestUrl, response) {
  try {
    if (requestUrl.pathname === "/api/filters") {
      const archiveResponse = await fetchArchive("/v1/api/rom/filters");
      response.writeHead(archiveResponse.status, { "Content-Type": "application/json; charset=utf-8" });
      response.end(await archiveResponse.text());
      return;
    }

    if (requestUrl.pathname === "/api/search") {
      const query = requestUrl.searchParams.get("query")?.trim() ?? "";
      if (query.length < 3) {
        response.writeHead(400, { "Content-Type": "application/json; charset=utf-8" });
        response.end(JSON.stringify({ error: "Enter at least three characters to search." }));
        return;
      }
      const apiUrl = new URL("/v1/api/rom/search", archiveOrigin);
      for (const [key, value] of requestUrl.searchParams) apiUrl.searchParams.set(key, value);
      const archiveResponse = await fetch(apiUrl);
      response.writeHead(archiveResponse.status, { "Content-Type": "application/json; charset=utf-8" });
      response.end(await archiveResponse.text());
      return;
    }

    if (requestUrl.pathname === "/api/directory") {
      const directoryPath = requestUrl.searchParams.get("path") || "/browse/";
      const target = new URL(directoryPath, archiveOrigin);
      if (target.origin !== archiveOrigin || !target.pathname.startsWith("/browse/")) {
        response.writeHead(400, { "Content-Type": "application/json; charset=utf-8" });
        response.end(JSON.stringify({ error: "Invalid archive directory." }));
        return;
      }
      const archiveResponse = await fetchArchive(`${target.pathname}${target.search}`);
      const directory = parseDirectory(await archiveResponse.text(), target.pathname);
      response.writeHead(200, { "Content-Type": "application/json; charset=utf-8" });
      response.end(JSON.stringify(directory));
      return;
    }

    if (requestUrl.pathname === "/api/download") {
      const id = Number(requestUrl.searchParams.get("id"));
      if (!Number.isSafeInteger(id) || id <= 0) {
        response.writeHead(400, { "Content-Type": "application/json; charset=utf-8" });
        response.end(JSON.stringify({ error: "A valid ROM id is required." }));
        return;
      }
      const { torrent, selectFile } = await getRomTorrent(id);
      streamTorrentFile(torrent, selectFile, response);
      return;
    }

    response.writeHead(404);
    response.end("Not found");
  } catch (error) {
    if (!response.headersSent) {
      response.writeHead(502, { "Content-Type": "application/json; charset=utf-8" });
      response.end(JSON.stringify({ error: error.message || "Could not reach Minerva Archive." }));
    } else {
      response.destroy(error);
    }
  }
}

export const server = createServer(async (request, response) => {
  const requestUrl = new URL(request.url, `http://${request.headers.host}`);

  if (requestUrl.pathname.startsWith("/api/")) {
    await handleApi(requestUrl, response);
    return;
  }

  const requestedPath = requestUrl.pathname === "/" ? "/index.html" : requestUrl.pathname;
  const filePath = normalize(join(root, requestedPath));
  if (!filePath.startsWith(root)) {
    response.writeHead(403);
    response.end("Forbidden");
    return;
  }

  try {
    const body = await readFile(filePath);
    response.writeHead(200, { "Content-Type": mimeTypes[extname(filePath)] || "application/octet-stream" });
    response.end(body);
  } catch {
    response.writeHead(404);
    response.end("Not found");
  }
});

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  server.listen(port, "127.0.0.1", () => {
    console.log(`FRAUM is available at http://127.0.0.1:${port}`);
  });
}
