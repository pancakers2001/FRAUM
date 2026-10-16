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

The packages are written to `dist/`. Windows builds include only a portable executable, macOS builds include DMGs for Intel and Apple Silicon, and Linux builds include an AppImage. The Windows app runs without an installer. GitHub Actions builds the Intel macOS package on `macos-15-intel` and Apple Silicon on `macos-15`.

Add files to the persistent download queue from the menu next to each Download button. Open the Queue tab to remove queued files or send all queued downloads to your browser. Items remain in the queue until you remove them.

Pushing a version tag such as `v1.0.3` to GitHub builds all supported packages and publishes them as assets on a GitHub Release. The workflow also runs tests before packaging each platform. To publish a release:

```sh
git tag v1.0.3
git push origin v1.0.3
```

Running the workflow manually from the Actions tab builds downloadable workflow artifacts but does not publish a release.

### macOS: “FRAUM is damaged and can’t be opened”

The macOS DMGs are not signed or notarized, so Gatekeeper may show a “damaged” warning even when the app downloaded correctly. Only use this workaround for a FRAUM copy downloaded from the [official GitHub Releases](https://github.com/pancakers2001/FRAUM/releases) page.

1. Cancel the warning, drag FRAUM from the mounted disk image into Applications, and eject the disk image.
2. Open Terminal and run:

   ```sh
   xattr -dr com.apple.quarantine /Applications/FRAUM.app
   ```

3. Open FRAUM from Applications.

## Run in a browser

Requires Node.js 18 or later.

```sh
npm start
```

Then open [http://localhost:4173](http://localhost:4173). Set `PORT` to use a different port.

The desktop app starts the local server on an available loopback port. The server proxies read-only search and directory requests to MiNERVA. Download buttons prepare torrent metadata when focused or hovered, show immediate startup feedback when clicked, then stream only the selected file. Temporary download data is removed when the transfer ends or is cancelled.

## Tests

```sh
npm test
```
