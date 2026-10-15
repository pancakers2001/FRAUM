# FRAUM

FRAUM is a small web client for searching and browsing the MiNERVA Archive. Search results and individual files in the directory browser have one-click downloads: FRAUM retrieves the archive's torrent metadata, selects only the requested file, and streams it to the browser. No separate torrent application is needed; the transfer itself uses BitTorrent.

## Desktop apps

Install dependencies and launch FRAUM as a desktop app:

```sh
npm install
npm run start:desktop
```

Use Node.js 22 or later when building desktop packages. Build installable packages on the matching OS:

```sh
npm run pack:windows
npm run pack:macos
npm run pack:macos:arm64
npm run pack:linux
```

The packages are written to `dist/`. Windows builds include an installer and a portable executable, macOS builds include a disk image and ZIP, and Linux builds include AppImage and DEB packages. Build each package on its target operating system. The `pack:macos:arm64` script creates a native Apple Silicon build and must run on macOS. GitHub Actions builds a separate `fraum-macos-arm64` artifact on an Apple Silicon runner, alongside the other platform builds, from the Actions tab or a version tag.

## Run in a browser

Requires Node.js 18 or later.

```sh
npm start
```

Then open [http://localhost:4173](http://localhost:4173). Set `PORT` to use a different port.

The desktop app starts the local server on an available loopback port. The server proxies read-only search and directory requests to MiNERVA. For downloads it temporarily stores selected torrent pieces under the operating system's temporary directory, streams the requested file to the browser, and removes the temporary data when the transfer ends or is cancelled.

## Tests

```sh
npm test
```
