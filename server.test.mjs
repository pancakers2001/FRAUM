import test from "node:test";
import assert from "node:assert/strict";
import { parseDirectory, parseRomDetails } from "./server.mjs";

test("parses directory entries and breadcrumb paths", () => {
  const html = `
    <div class="entry search_back"><a href="/browse/./">../</a></div>
    <div class="entry" data-name="system &amp; games">
      <a href="/browse/./No-Intro/Some%20System/"><strong>Some System/</strong></a>
      <span>120.83 MB</span>
      <a onclick="downloadMagnet('magnet:?xt=urn:btih:abcd&amp;dn=Archive')" data-m="0">🧲</a>
    </div>
    <div class="entry" data-name="game.zip">
      <a href="/rom?id=42">game.zip</a>
      <span>1.20 MB</span>
    </div>`;

  assert.deepEqual(parseDirectory(html, "/browse/./No-Intro/"), {
    entries: [
      {
        name: "Some System/",
        sortName: "system & games",
        path: "/browse/No-Intro/Some%20System/",
        isDirectory: true,
        size: "120.83 MB",
        id: null
      },
      {
        name: "game.zip",
        sortName: "game.zip",
        path: "/rom?id=42",
        isDirectory: false,
        size: "1.20 MB",
        id: 42
      }
    ],
    parent: "/browse/",
    breadcrumbs: [
      { name: "Root", path: "/browse/" },
      { name: "No-Intro", path: "/browse/No-Intro/" }
    ]
  });
});

test("rejects unrelated links in directory markup", () => {
  const directory = parseDirectory('<a href="/browse/not-an-entry">not an entry</a>', "/browse/");
  assert.deepEqual(directory.entries, []);
  assert.equal(directory.parent, null);
});

test("parses and validates ROM download metadata", () => {
  const rom = {
    id: 581,
    file_name: "game.zip",
    full_path: "./Collection/game.zip",
    torrents: "Collection/collection.torrent",
    so_id: 3,
    size: 1234
  };
  assert.deepEqual(parseRomDetails(`<script>window.rom = ${JSON.stringify(rom)};</script>`), rom);
  assert.throws(() => parseRomDetails("<html>missing data</html>"), /details are missing/);
  assert.throws(
    () => parseRomDetails(`<script>window.rom = ${JSON.stringify({ ...rom, so_id: -1 })};</script>`),
    /incomplete/
  );
});
