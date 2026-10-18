// Hide the console window that would otherwise open behind the Windows app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use directories::ProjectDirs;
use eframe::egui;
use librqbit::storage::StorageFactoryExt;
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Session};
use regex::Regex;
use reqwest::{Client, StatusCode, Url};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

mod torrent_storage;

const ARCHIVE: &str = "https://minerva-archive.org";
const PAGE_SIZE: usize = 100;
const DIRECTORY_PAGE_SIZE: usize = 60;

#[derive(Clone, Debug, Deserialize)]
struct Rom {
    id: u64,
    full_path: String,
}

fn selected_filter(options: &[String], index: usize) -> Option<&String> {
    index.checked_sub(1).and_then(|index| options.get(index))
}

impl Rom {
    fn name(&self) -> String {
        self.full_path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&self.full_path)
            .to_owned()
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Filters {
    groups: Vec<String>,
    consoles: Vec<String>,
    regions: Vec<String>,
    manufacturers: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct DirectoryEntry {
    name: String,
    path: String,
    #[serde(rename = "isDirectory")]
    is_directory: bool,
    size: Option<String>,
    id: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
struct Breadcrumb {
    name: String,
    path: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Directory {
    entries: Vec<DirectoryEntry>,
    parent: Option<String>,
    breadcrumbs: Vec<Breadcrumb>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ArchiveFile {
    file_name: String,
    torrents: String,
    so_id: usize,
    size: u64,
    // Set for collections whose torrent doesn't name a single system (e.g. FinalBurn Neo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct QueueItem {
    id: u64,
    name: String,
    full_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<ArchiveFile>,
}

struct PresetGroup {
    name: String,
    // The region code (NA, EU or JP) this list belongs to.
    region: &'static str,
    items: Vec<QueueItem>,
}

struct Preset {
    name: String,
    note: String,
    // Set for presets the user imported; the copy of the file kept in the config directory.
    custom_file: Option<PathBuf>,
    // The lists that cover the whole preset, such as "All Games (NA)".
    top: Vec<PresetGroup>,
    // The per-system lists, such as "NES (NA)".
    groups: Vec<PresetGroup>,
}

#[derive(Deserialize)]
struct PresetEntry {
    id: u64,
    name: String,
    full_path: String,
    system: String,
    torrents: String,
    so_id: usize,
    size: u64,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    // The regional lists (NA, EU, JP) this file belongs to.
    #[serde(default)]
    regions: Vec<String>,
}

const REGIONS: [&str; 3] = ["NA", "EU", "JP"];

// The Genesis is the Mega Drive, and the Sega CD the Mega CD, outside North America.
fn system_label(system: &str, region: &str) -> String {
    match system {
        "Sega Genesis" if region != "NA" => "Mega Drive".to_owned(),
        "Sega CD" if region != "NA" => "Mega CD".to_owned(),
        _ => system.to_owned(),
    }
}

fn named_group(name: String, region: &'static str, items: Vec<QueueItem>) -> Option<PresetGroup> {
    (!items.is_empty()).then_some(PresetGroup {
        name,
        region,
        items,
    })
}

fn preset_from_json(name: &'static str, note: &'static str, json: &str) -> Preset {
    let entries: Vec<PresetEntry> =
        serde_json::from_str(json).expect("bundled preset data is valid");
    let mut systems: Vec<String> = Vec::new();
    let mut members: Vec<(QueueItem, String, Vec<String>)> = Vec::new();
    for entry in entries {
        if !systems.contains(&entry.system) {
            systems.push(entry.system.clone());
        }
        let item = QueueItem {
            id: entry.id,
            file: Some(ArchiveFile {
                file_name: entry.file_name.unwrap_or_else(|| entry.name.clone()),
                torrents: entry.torrents,
                so_id: entry.so_id,
                size: entry.size,
                folder: entry.folder,
            }),
            name: entry.name,
            full_path: entry.full_path,
        };
        members.push((item, entry.system, entry.regions));
    }
    let in_region = |regions: &[String], code: &str| regions.iter().any(|region| region == code);
    let top = REGIONS
        .iter()
        .filter_map(|&code| {
            let items = members
                .iter()
                .filter(|(_, _, regions)| in_region(regions, code))
                .map(|(item, ..)| item.clone())
                .collect();
            named_group(format!("All Games ({code})"), code, items)
        })
        .collect();
    // With one system the per-system lists would repeat the whole-preset lists.
    let groups = {
        let mut groups = Vec::new();
        for system in systems.iter().filter(|_| systems.len() > 1) {
            for code in REGIONS {
                let items = members
                    .iter()
                    .filter(|(_, member_system, regions)| {
                        member_system == system && in_region(regions, code)
                    })
                    .map(|(item, ..)| item.clone())
                    .collect();
                groups.extend(named_group(
                    format!("{} ({code})", system_label(system, code)),
                    code,
                    items,
                ));
            }
        }
        groups
    };
    Preset {
        name: name.to_owned(),
        note: note.to_owned(),
        custom_file: None,
        top,
        groups,
    }
}

const MAX_PRESET_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PRESET_ITEMS: usize = 100_000;

// The file format for importing and exporting presets.
#[derive(Serialize, Deserialize)]
struct PresetFile {
    #[serde(default)]
    name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    note: String,
    items: Vec<PresetFileItem>,
}

#[derive(Serialize, Deserialize)]
struct PresetFileItem {
    id: u64,
    name: String,
    #[serde(default)]
    full_path: String,
    // Without it the download looks the file up in the archive, as it does for searched files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<ArchiveFile>,
    // The regional lists (NA, EU, JP) this file belongs to, if the file has any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    regions: Vec<String>,
}

fn parse_preset_file(content: &str, fallback_name: &str) -> anyhow::Result<PresetFile> {
    let mut file: PresetFile =
        serde_json::from_str(content).context("This is not a FRAUM preset file")?;
    let name = file.name.trim();
    let name = if name.is_empty() {
        fallback_name.trim()
    } else {
        name
    };
    if name.is_empty() || name.chars().count() > 60 {
        bail!("A preset needs a name of 1 to 60 characters.");
    }
    file.name = name.to_owned();
    if file.items.len() > MAX_PRESET_ITEMS {
        bail!("A preset can hold at most {MAX_PRESET_ITEMS} files.");
    }
    let mut seen = HashSet::new();
    file.items
        .retain(|item| item.id > 0 && !item.name.trim().is_empty() && seen.insert(item.id));
    if file.items.is_empty() {
        bail!("The preset has no files in it.");
    }
    for item in &mut file.items {
        item.regions
            .retain(|region| REGIONS.contains(&region.as_str()));
    }
    Ok(file)
}

fn preset_from_file(file: PresetFile, custom_file: Option<PathBuf>) -> Preset {
    let has_regions = file.items.iter().any(|item| !item.regions.is_empty());
    let members: Vec<(QueueItem, Vec<String>)> = file
        .items
        .into_iter()
        .map(|item| {
            let queued = QueueItem {
                id: item.id,
                name: item.name,
                full_path: item.full_path,
                file: item.file,
            };
            (queued, item.regions)
        })
        .collect();
    let mut top = vec![PresetGroup {
        name: "All Games".to_owned(),
        region: "",
        items: members.iter().map(|(item, _)| item.clone()).collect(),
    }];
    if has_regions {
        for code in REGIONS {
            let items = members
                .iter()
                .filter(|(_, regions)| regions.iter().any(|region| region == code))
                .map(|(item, _)| item.clone())
                .collect();
            top.extend(named_group(format!("All Games ({code})"), code, items));
        }
    }
    Preset {
        name: file.name,
        note: if file.note.is_empty() {
            "Imported preset.".to_owned()
        } else {
            file.note
        },
        custom_file,
        top,
        groups: Vec::new(),
    }
}

fn queue_as_preset_file(name: &str, queue: &[QueueItem]) -> PresetFile {
    PresetFile {
        name: name.to_owned(),
        note: String::new(),
        items: queue
            .iter()
            .map(|item| PresetFileItem {
                id: item.id,
                name: item.name.clone(),
                full_path: item.full_path.clone(),
                file: item.file.clone(),
                regions: Vec::new(),
            })
            .collect(),
    }
}

fn preset_slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() { "preset" } else { slug }.to_owned()
}

fn write_atomically(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)?;
    Ok(())
}

// Keeps a copy of an imported preset so it is still there the next time FRAUM starts.
fn save_custom_preset(dir: &Path, file: &PresetFile) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", preset_slug(&file.name)));
    write_atomically(&path, &serde_json::to_vec_pretty(file)?)?;
    Ok(path)
}

fn load_custom_presets(dir: &Path) -> (Vec<Preset>, Vec<String>) {
    let mut paths: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect(),
        Err(_) => return (Vec::new(), Vec::new()),
    };
    paths.sort();
    let mut presets = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let loaded = fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|content| parse_preset_file(&content, &stem));
        match loaded {
            Ok(file) => presets.push(preset_from_file(file, Some(path))),
            Err(error) => errors.push(format!("{}: {error:#}", path.display())),
        }
    }
    (presets, errors)
}

// Returns the preset's name and every file in it, which the import adds to the queue.
fn import_preset_file(
    path: &Path,
    dir: &Path,
    presets: &mut Vec<Preset>,
) -> anyhow::Result<(String, Vec<QueueItem>)> {
    if fs::metadata(path)?.len() > MAX_PRESET_FILE_BYTES {
        bail!("The file is too large to be a preset.");
    }
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file = parse_preset_file(&fs::read_to_string(path)?, &stem)?;
    if presets
        .iter()
        .any(|preset| preset.custom_file.is_none() && preset.name == file.name)
    {
        bail!(
            "{} is the name of a built-in preset; rename it first.",
            file.name
        );
    }
    let saved = save_custom_preset(dir, &file)?;
    let name = file.name.clone();
    let preset = preset_from_file(file, Some(saved.clone()));
    let items = preset.top[0].items.clone();
    match presets
        .iter()
        .position(|existing| existing.custom_file.as_ref() == Some(&saved))
    {
        Some(index) => presets[index] = preset,
        None => presets.push(preset),
    }
    Ok((name, items))
}

fn custom_presets_dir() -> anyhow::Result<PathBuf> {
    config_file("presets")
}

fn load_presets() -> Vec<Preset> {
    vec![
        preset_from_json(
            "Nintendo Classics",
            "Every Nintendo Classics game in each region, using that region's version where MiNERVA has one. Wikipedia lists North America and Europe together, so those two lists hold the same games. Each list is about 9 GB, and about 8 GB of that is the 12 GameCube games (around 700 MB each).",
            include_str!("../presets/nintendo_classics.json"),
        ),
        preset_from_json(
            "Virtual Console",
            "Every game on the Wii, Wii U and 3DS Virtual Console in each region, using that region's version of the game where MiNERVA has one. North America and Europe are about 10 GB each and Japan about 19 GB; most of the size is PC Engine CD games of 100–700 MB. Neo Geo and arcade lists include the Neo Geo BIOS and any parent sets they need.",
            include_str!("../presets/virtual_console.json"),
        ),
        preset_from_json(
            "PlayStation Classic",
            "The games built into the PlayStation Classic, using the Redump disc image for each region. Wikipedia lists North America and Europe together, so those two lists hold the same games. Each list is about 9–10 GB, and the multi-disc games (Final Fantasy VII, Metal Gear Solid, Parasite Eve) include every disc. Revelations: Persona has no European release, so the European list uses the US disc, as the console did.",
            include_str!("../presets/playstation_classic.json"),
        ),
        preset_from_json(
            "NES Classic Edition",
            "The 30 games built into each region's NES Classic Edition (the Famicom Classic Mini in Japan), using that region's version where MiNERVA has one. Wikipedia lists North America and Europe together, so those two lists hold the same games. Each list is under 3 MB.",
            include_str!("../presets/nes_classic.json"),
        ),
        preset_from_json(
            "Super NES Classic Edition",
            "The 21 games built into each region's Super NES Classic Edition (the Super Famicom Classic Mini in Japan), using that region's version where MiNERVA has one. Wikipedia lists North America and Europe together, so those two lists hold the same games. Each list is about 27 MB.",
            include_str!("../presets/snes_classic.json"),
        ),
        preset_from_json(
            "Sega Genesis Mini",
            "The 42 games built into each region's Genesis Mini (Mega Drive Mini in Europe and Japan), using the ROM that console shipped where MiNERVA has it. Wikipedia lists North America and Europe together, so those two lists hold the same games. The Asia-only games are left out. Each list is under 50 MB.",
            include_str!("../presets/genesis_mini.json"),
        ),
        preset_from_json(
            "Sega Genesis Mini 2",
            "The games built into each region's Genesis Mini 2 (Mega Drive Mini 2 in Europe and Japan): Genesis cartridge games plus about 20 Sega CD games. About 60 games per region, 5.5 GB for North America and Europe and 8 GB for Japan, nearly all of it Sega CD discs. Wikipedia lists North America and Europe together, so those two lists hold the same games.",
            include_str!("../presets/genesis_mini_2.json"),
        ),
    ]
}

#[derive(Deserialize)]
struct RomDetails {
    id: u64,
    file_name: String,
    full_path: String,
    torrents: String,
    magnet: String,
    so_id: usize,
    size: u64,
}

#[derive(Clone, Copy)]
enum View {
    Search,
    Browse,
    Queue,
    Seeding,
    Settings,
}

enum UiEvent {
    Filters(Result<Filters, String>),
    Search(Result<Vec<Rom>, String>),
    Directory(Result<Directory, String>),
    DownloadStatus(String),
    DownloadFinished(Result<(), String>),
}

struct NativeApp {
    client: Client,
    runtime: Runtime,
    tx: Sender<UiEvent>,
    rx: Receiver<UiEvent>,
    view: View,
    filters: Option<Filters>,
    filter_indices: [usize; 4],
    filter_queries: [String; 4],
    query: String,
    min_size: String,
    max_size: String,
    tokenised: bool,
    results: Vec<Rom>,
    offset: usize,
    has_more: bool,
    searching: bool,
    search_status: String,
    directory_filter: String,
    directory: Option<Directory>,
    directory_offset: usize,
    queue: Vec<QueueItem>,
    presets: Vec<Preset>,
    queue_status: String,
    download_dir: Option<PathBuf>,
    download_status: Option<String>,
    downloading: bool,
    settings: Settings,
    settings_status: Option<String>,
    connection_status: String,
}

impl NativeApp {
    fn new() -> Self {
        let client = Client::builder()
            .user_agent("FRAUM/1.0 (MiNERVA archive client)")
            .timeout(Duration::from_secs(60))
            .build()
            .expect("HTTP client configuration is valid");
        let runtime = Runtime::new().expect("Tokio runtime can be created");
        let (tx, rx) = mpsc::channel();
        let mut presets = load_presets();
        let custom_errors = match custom_presets_dir() {
            Ok(dir) => {
                let (custom, errors) = load_custom_presets(&dir);
                presets.extend(custom);
                errors
            }
            Err(_) => Vec::new(),
        };
        let (mut queue, queue_status) = match load_queue() {
            Ok(queue) if custom_errors.is_empty() => {
                (queue, "Add files from search or browse.".to_owned())
            }
            Ok(queue) => (
                queue,
                format!("Could not load an imported preset: {}", custom_errors[0]),
            ),
            Err(error) => (
                Vec::new(),
                format!("Could not restore the saved queue: {error}"),
            ),
        };
        attach_preset_files(&mut queue, &presets);
        let (settings, settings_status) = match load_settings() {
            Ok(settings) => (settings, None),
            Err(error) => (
                Settings::default(),
                Some(format!("Could not restore saved settings: {error}")),
            ),
        };
        let app = Self {
            client,
            runtime,
            tx,
            rx,
            view: View::Search,
            filters: None,
            filter_indices: [0; 4],
            filter_queries: Default::default(),
            query: String::new(),
            min_size: String::new(),
            max_size: String::new(),
            tokenised: false,
            results: Vec::new(),
            offset: 0,
            has_more: false,
            searching: false,
            search_status: "Search the MiNERVA archive.".to_owned(),
            directory_filter: String::new(),
            directory: None,
            directory_offset: 0,
            queue,
            presets,
            queue_status,
            download_dir: dirs_download_dir(),
            download_status: None,
            downloading: false,
            settings,
            settings_status,
            connection_status: "Connecting…".to_owned(),
        };
        app.fetch_filters();
        app
    }

    fn fetch_filters(&self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = async {
                client
                    .get(format!("{ARCHIVE}/v1/api/rom/filters"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<Filters>()
                    .await
                    .map_err(anyhow::Error::from)
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = tx.send(UiEvent::Filters(result));
        });
    }

    fn start_search(&mut self) {
        let query = self.query.trim().to_lowercase();
        if query.chars().count() < 3 {
            self.search_status = "Enter at least three characters to search.".to_owned();
            self.results.clear();
            return;
        }
        let Some(filters) = &self.filters else {
            self.search_status = "Search filters are still loading.".to_owned();
            return;
        };

        let selected = [
            selected_filter(&filters.groups, self.filter_indices[0]),
            selected_filter(&filters.consoles, self.filter_indices[1]),
            selected_filter(&filters.regions, self.filter_indices[2]),
            selected_filter(&filters.manufacturers, self.filter_indices[3]),
        ]
        .map(|value| value.cloned());
        let min_size = self.min_size.clone();
        let max_size = self.max_size.clone();
        let tokenised = self.tokenised;
        let offset = self.offset;
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.search_status = "Searching MiNERVA…".to_owned();
        self.results.clear();
        self.searching = true;
        self.runtime.spawn(async move {
            let mut url = Url::parse(&format!("{ARCHIVE}/v1/api/rom/search"))
                .expect("archive search URL is valid");
            {
                let mut params = url.query_pairs_mut();
                params.append_pair("query", &query);
                params.append_pair("offset", &offset.to_string());
                if tokenised {
                    params.append_pair("tokenised", "true");
                }
                if let Ok(size) = min_size.parse::<f64>()
                    && size.is_finite()
                    && size > 0.0
                {
                    params.append_pair("min_size", &min_size);
                }
                if let Ok(size) = max_size.parse::<f64>()
                    && size.is_finite()
                    && size > 0.0
                {
                    params.append_pair("max_size", &max_size);
                }
                for (key, value) in ["group", "console", "region", "manufacturer"]
                    .into_iter()
                    .zip(selected)
                {
                    if let Some(value) = value {
                        params.append_pair(key, &value);
                    }
                }
            }
            let result = async {
                client
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<Vec<Rom>>()
                    .await
                    .map_err(anyhow::Error::from)
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = tx.send(UiEvent::Search(result));
        });
    }

    fn load_directory(&mut self, path: String) {
        self.directory_filter.clear();
        self.directory_offset = 0;
        self.directory = None;
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = fetch_directory(&client, &path)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(UiEvent::Directory(result));
        });
    }

    fn enqueue(&mut self, item: QueueItem) {
        if self.queue.iter().any(|queued| queued.id == item.id) {
            self.queue_status = format!("{} is already in the queue.", item.name);
            return;
        }
        self.queue.push(item);
        if let Err(error) = save_queue(&self.queue) {
            self.queue_status = format!("Could not save the queue: {error}");
            return;
        }
        self.queue_status = "Added to the queue.".to_owned();
    }

    fn add_preset_items(&mut self, label: &str, items: Vec<QueueItem>) {
        let queued: HashSet<u64> = self.queue.iter().map(|item| item.id).collect();
        let new_items: Vec<_> = items
            .iter()
            .filter(|item| !queued.contains(&item.id))
            .cloned()
            .collect();
        let skipped = items.len() - new_items.len();
        let added = new_items.len();
        self.queue.extend(new_items);
        self.queue_status = match save_queue(&self.queue) {
            Ok(()) if skipped == 0 => format!("Added {added} games from {label}."),
            Ok(()) => format!("Added {added} games from {label} ({skipped} already queued)."),
            Err(error) => format!("Could not save the queue: {error}"),
        };
    }

    fn import_preset(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("FRAUM preset", &["json"])
            .set_title("Import preset")
            .pick_file()
        else {
            return;
        };
        let imported =
            custom_presets_dir().and_then(|dir| import_preset_file(&path, &dir, &mut self.presets));
        match imported {
            Ok((name, items)) => {
                self.add_preset_items(&format!("the imported preset {name}"), items);
            }
            Err(error) => {
                self.queue_status = format!("Could not import {}: {error:#}", path.display());
            }
        }
    }

    fn export_queue(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("FRAUM preset", &["json"])
            .set_title("Export the queue as a preset")
            .set_file_name("fraum-preset.json")
            .save_file()
        else {
            return;
        };
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = if name.trim().is_empty() {
            "My preset".to_owned()
        } else {
            name
        };
        let file = queue_as_preset_file(&name, &self.queue);
        let result = serde_json::to_vec_pretty(&file)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| write_atomically(&path, &bytes));
        self.queue_status = match result {
            Ok(()) => format!("Exported {} files to {}.", self.queue.len(), path.display()),
            Err(error) => format!("Could not export the queue: {error:#}"),
        };
    }

    fn remove_custom_preset(&mut self, index: usize) {
        let Some(file) = self.presets.get(index).and_then(|p| p.custom_file.clone()) else {
            return;
        };
        self.queue_status = match fs::remove_file(&file) {
            Ok(()) => {
                let removed = self.presets.remove(index);
                format!("Removed the preset {}.", removed.name)
            }
            Err(error) => format!("Could not remove the preset: {error}"),
        };
    }

    fn start_downloads(&mut self, items: Vec<QueueItem>) {
        if items.is_empty() || self.downloading {
            return;
        }
        let Some(folder) = self.download_dir.clone() else {
            let status = "Choose a download folder in the Queue tab first.".to_owned();
            self.queue_status = status.clone();
            self.download_status = Some(status);
            return;
        };
        self.downloading = true;
        let status = format!(
            "Starting {} download(s) in {}…",
            items.len(),
            folder.display()
        );
        self.queue_status = status.clone();
        self.download_status = Some(status);
        let client = self.client.clone();
        let tx = self.tx.clone();
        let organize_by_system = self.settings.organize_by_system;
        self.runtime.spawn(async move {
            let result =
                download_queue(client, tx.clone(), items, folder, organize_by_system).await;
            let _ = tx.send(UiEvent::DownloadFinished(
                result.map_err(|error| format!("{error:#}")),
            ));
        });
    }

    fn choose_download_folder(&mut self) {
        let mut dialog = rfd::FileDialog::new();
        if let Some(directory) = &self.download_dir {
            dialog = dialog.set_directory(directory);
        }
        if let Some(folder) = dialog.pick_folder() {
            self.download_dir = Some(folder.clone());
            let status = format!("Download folder: {}", folder.display());
            self.queue_status = status.clone();
            self.download_status = Some(status);
        }
    }

    fn poll_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                UiEvent::Filters(result) => match result {
                    Ok(filters) => {
                        self.filters = Some(filters);
                        self.connection_status = "Live index".to_owned();
                    }
                    Err(error) => {
                        self.connection_status = "Archive unavailable".to_owned();
                        self.search_status = error;
                    }
                },
                UiEvent::Search(result) => match result {
                    Ok(mut results) => {
                        self.searching = false;
                        self.has_more = results.len() > PAGE_SIZE;
                        results.truncate(PAGE_SIZE);
                        self.search_status = if results.is_empty() {
                            "No records found.".to_owned()
                        } else {
                            format!(
                                "Showing {}–{} for “{}”",
                                self.offset + 1,
                                self.offset + results.len(),
                                self.query
                            )
                        };
                        self.results = results;
                        self.connection_status = "Live index".to_owned();
                    }
                    Err(error) => {
                        self.searching = false;
                        self.search_status = format!("Search failed: {error}");
                        self.connection_status = "Archive unavailable".to_owned();
                    }
                },
                UiEvent::Directory(result) => match result {
                    Ok(directory) => {
                        self.directory = Some(directory);
                        self.connection_status = "Live index".to_owned();
                    }
                    Err(error) => {
                        self.directory = None;
                        self.search_status = format!("Could not open folder: {error}");
                        self.connection_status = "Archive unavailable".to_owned();
                    }
                },
                UiEvent::DownloadStatus(status) => {
                    self.queue_status = status.clone();
                    self.download_status = Some(status);
                }
                UiEvent::DownloadFinished(result) => {
                    self.downloading = false;
                    self.queue_status = match result {
                        Ok(()) => {
                            format!(
                                "Downloads finished in {}. Queue items remain until removed.",
                                self.download_dir
                                    .as_deref()
                                    .unwrap_or_else(|| Path::new("the download folder"))
                                    .display()
                            )
                        }
                        Err(error) => format!("Download failed: {error}"),
                    };
                    self.download_status = Some(self.queue_status.clone());
                }
            }
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(egui::RichText::new("FRAUM").color(egui::Color32::from_rgb(199, 243, 106)));
            ui.separator();
            for (label, view) in [
                ("Search", 0),
                ("Browse", 1),
                ("Queue", 2),
                ("Seeding", 3),
                ("Settings", 4),
            ] {
                let selected = matches!(
                    (&self.view, view),
                    (View::Search, 0)
                        | (View::Browse, 1)
                        | (View::Queue, 2)
                        | (View::Seeding, 3)
                        | (View::Settings, 4)
                );
                let label = if view == 2 {
                    format!("Queue ({})", self.queue.len())
                } else {
                    label.to_owned()
                };
                if ui.selectable_label(selected, label).clicked() {
                    self.view = match view {
                        0 => View::Search,
                        1 => View::Browse,
                        2 => View::Queue,
                        3 => View::Seeding,
                        _ => View::Settings,
                    };
                    if view == 1 && self.directory.is_none() {
                        self.load_directory("/browse/".to_owned());
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .link(
                        egui::RichText::new("MiNERVA")
                            .color(egui::Color32::from_rgb(159, 122, 234)),
                    )
                    .on_hover_text("Open the MiNERVA homepage")
                    .clicked()
                    && let Err(error) = open::that(ARCHIVE)
                {
                    self.download_status =
                        Some(format!("Could not open the MiNERVA homepage: {error}"));
                }
                ui.separator();
                ui.label(egui::RichText::new(&self.connection_status).weak());
            });
        });
        if let Some(status) = &self.download_status {
            ui.label(status);
        }
        ui.separator();
    }

    fn search_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("Search the archive");
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .desired_width(400.0)
                    .hint_text("Search games, systems, publishers…"),
            );
            if !self.searching
                && response.lost_focus()
                && ui.input(|input| input.key_pressed(egui::Key::Enter))
            {
                self.offset = 0;
                self.start_search();
            }
            if ui
                .add_enabled(!self.searching, egui::Button::new("Search"))
                .clicked()
            {
                self.offset = 0;
                self.start_search();
            }
        });
        self.search_filters(ui);
        ui.label(&self.search_status);
        egui::ScrollArea::vertical()
            .max_height(500.0)
            .show(ui, |ui| {
                for rom in self.results.clone() {
                    self.rom_row(ui, &rom);
                }
            });
        ui.horizontal(|ui| {
            if navigation_button(ui, false, self.offset > 0 && !self.searching).clicked() {
                self.offset = self.offset.saturating_sub(PAGE_SIZE);
                self.start_search();
            }
            ui.label(format!("Page {}", self.offset / PAGE_SIZE + 1));
            if navigation_button(ui, true, self.has_more && !self.searching).clicked() {
                self.offset += PAGE_SIZE;
                self.start_search();
            }
        });
    }

    fn search_filters(&mut self, ui: &mut egui::Ui) {
        let Some(filters) = &self.filters else {
            ui.label("Loading archive filters…");
            return;
        };
        ui.horizontal_wrapped(|ui| {
            combo(
                ui,
                "Collection",
                "group",
                &filters.groups,
                &mut self.filter_indices[0],
                &mut self.filter_queries[0],
            );
            combo(
                ui,
                "System",
                "console",
                &filters.consoles,
                &mut self.filter_indices[1],
                &mut self.filter_queries[1],
            );
            combo(
                ui,
                "Region",
                "region",
                &filters.regions,
                &mut self.filter_indices[2],
                &mut self.filter_queries[2],
            );
            combo(
                ui,
                "Publisher",
                "manufacturer",
                &filters.manufacturers,
                &mut self.filter_indices[3],
                &mut self.filter_queries[3],
            );
        });
        ui.horizontal(|ui| {
            ui.label("Size MB");
            ui.add(
                egui::TextEdit::singleline(&mut self.min_size)
                    .desired_width(70.0)
                    .hint_text("Min"),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.max_size)
                    .desired_width(70.0)
                    .hint_text("Max"),
            );
            ui.checkbox(&mut self.tokenised, "Match words");
        });
    }

    fn rom_row(&mut self, ui: &mut egui::Ui, rom: &Rom) {
        let item = QueueItem {
            id: rom.id,
            name: rom.name(),
            full_path: rom.full_path.clone(),
            file: None,
        };
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.strong(&item.name);
                    ui.label(egui::RichText::new(&item.full_path).weak().small());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    more_menu(ui, |ui| {
                        if ui.button("Add to queue").clicked() {
                            self.enqueue(item.clone());
                            ui.close_menu();
                        }
                        if ui.button("View details").clicked() {
                            if let Err(error) = open::that(format!("{ARCHIVE}/rom?id={}", item.id))
                            {
                                self.queue_status = format!("Could not open ROM details: {error}");
                            }
                            ui.close_menu();
                        }
                    });
                    if download_button(ui, "Download", true).clicked() {
                        self.start_downloads(vec![item.clone()]);
                    }
                });
            });
        });
        ui.add_space(5.0);
    }

    fn browse_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("Browse collections");
        ui.horizontal_wrapped(|ui| {
            if let Some(directory) = &self.directory {
                for crumb in &directory.breadcrumbs {
                    if ui.link(&crumb.name).clicked() {
                        self.load_directory(crumb.path.clone());
                        return;
                    }
                    ui.label("/");
                }
                if let Some(parent) = &directory.parent
                    && parent_button(ui).clicked()
                {
                    self.load_directory(parent.clone());
                    return;
                }
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.directory_filter)
                    .desired_width(220.0)
                    .hint_text("Filter this directory…"),
            );
        });
        let Some(directory) = &self.directory else {
            ui.label("Opening collection…");
            return;
        };
        let filter = self.directory_filter.to_lowercase();
        let entries: Vec<_> = directory
            .entries
            .iter()
            .filter(|entry| entry.name.to_lowercase().contains(&filter))
            .cloned()
            .collect();
        egui::ScrollArea::vertical()
            .max_height(470.0)
            .show(ui, |ui| {
                for entry in entries
                    .iter()
                    .skip(self.directory_offset)
                    .take(DIRECTORY_PAGE_SIZE)
                {
                    ui.horizontal(|ui| {
                        if entry.is_directory {
                            folder_icon(ui);
                            if ui.link(&entry.name).clicked() {
                                self.load_directory(entry.path.clone());
                            }
                        } else {
                            ui.label(&entry.name);
                        }
                        if let Some(size) = &entry.size {
                            ui.label(egui::RichText::new(size).weak().small());
                        }
                        if let Some(id) = entry.id {
                            let item = QueueItem {
                                id,
                                name: entry.name.clone(),
                                full_path: entry.path.clone(),
                                file: None,
                            };
                            if download_button(ui, "Download", true).clicked() {
                                self.start_downloads(vec![item.clone()]);
                            }
                            more_menu(ui, |ui| {
                                if ui.button("Add to queue").clicked() {
                                    self.enqueue(item.clone());
                                    ui.close_menu();
                                }
                                if ui.button("View details").clicked() {
                                    if let Err(error) = open::that(format!("{ARCHIVE}/rom?id={id}"))
                                    {
                                        self.queue_status =
                                            format!("Could not open ROM details: {error}");
                                    }
                                    ui.close_menu();
                                }
                            });
                        }
                    });
                }
            });
        ui.horizontal(|ui| {
            if navigation_button(ui, false, self.directory_offset > 0).clicked() {
                self.directory_offset = self.directory_offset.saturating_sub(DIRECTORY_PAGE_SIZE);
            }
            ui.label(format!(
                "Page {} · {} entries",
                self.directory_offset / DIRECTORY_PAGE_SIZE + 1,
                entries.len()
            ));
            if navigation_button(
                ui,
                true,
                self.directory_offset + DIRECTORY_PAGE_SIZE < entries.len(),
            )
            .clicked()
            {
                self.directory_offset += DIRECTORY_PAGE_SIZE;
            }
        });
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.add_space(12.0);
        let mut organize = self.settings.organize_by_system;
        let response = ui
            .horizontal(|ui| {
                let toggled = toggle_switch(ui, &mut organize);
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Sort downloads into RetroBat / Batocera system folders")
                        .size(18.0),
                );
                toggled
            })
            .inner
            .on_hover_text(
                "Saves each game in a folder named after its system, such as nes, snes or megadrive.",
            );
        if response.changed() {
            let mut updated = self.settings.clone();
            updated.organize_by_system = organize;
            self.settings_status = match save_settings(&updated) {
                Ok(()) => {
                    self.settings = updated;
                    None
                }
                Err(error) => Some(format!("Could not save settings: {error}")),
            };
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "Folders use the system names shared by RetroBat and Batocera (nes, snes, gba, megadrive…) and are created inside your download folder. Files from systems FRAUM doesn't recognize are saved directly in it. Applies to downloads you start from now on.",
            )
            .size(15.0)
            .weak(),
        );
        ui.add_space(20.0);
        ui.heading("Presets");
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Regions shown in the Presets menu").size(18.0));
        let mut regions = self.settings.preset_regions.clone();
        ui.horizontal(|ui| {
            ui.checkbox(&mut regions.na, "NA");
            ui.checkbox(&mut regions.eu, "EU");
            ui.checkbox(&mut regions.jp, "JP");
        });
        ui.label(
            egui::RichText::new(
                "Untick a region to hide its lists, such as All Games (EU) or NES (EU), from every preset. Your queue is not changed.",
            )
            .size(15.0)
            .weak(),
        );
        if regions != self.settings.preset_regions {
            let mut updated = self.settings.clone();
            updated.preset_regions = regions;
            self.settings_status = match save_settings(&updated) {
                Ok(()) => {
                    self.settings = updated;
                    None
                }
                Err(error) => Some(format!("Could not save settings: {error}")),
            };
        }
        if let Some(status) = &self.settings_status {
            ui.colored_label(egui::Color32::from_rgb(255, 120, 120), status);
        }
    }

    fn queue_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Queued downloads");
            if ui.button("Choose folder…").clicked() {
                self.choose_download_folder();
            }
            let mut chosen = None;
            let mut remove = None;
            let (mut import, mut export) = (false, false);
            ui.menu_button("Presets", |ui| {
                for (index, preset) in self.presets.iter().enumerate() {
                    ui.menu_button(preset.name.as_str(), |ui| {
                        if let Some(picked) = preset_menu(ui, preset, &self.settings.preset_regions)
                        {
                            chosen = Some(picked);
                            ui.close_menu();
                        }
                        if preset.custom_file.is_some() {
                            ui.separator();
                            if ui.button("Remove this preset").clicked() {
                                remove = Some(index);
                                ui.close_menu();
                            }
                        }
                    });
                }
                if !self.settings.preset_regions.any() {
                    ui.label("Every region is hidden. Turn some back on in Settings.");
                }
                ui.separator();
                if ui.button("Import preset…").clicked() {
                    import = true;
                    ui.close_menu();
                }
                let can_export = !self.queue.is_empty();
                if ui
                    .add_enabled(can_export, egui::Button::new("Export queue as preset…"))
                    .clicked()
                {
                    export = true;
                    ui.close_menu();
                }
            });
            if let Some((label, items)) = chosen {
                self.add_preset_items(&label, items);
            }
            if let Some(index) = remove {
                self.remove_custom_preset(index);
            }
            if import {
                self.import_preset();
            }
            if export {
                self.export_queue();
            }
        });
        ui.horizontal(|ui| {
            ui.label(
                self.download_dir
                    .as_deref()
                    .map(|path| {
                        if self.settings.organize_by_system {
                            format!("Saving to {} (in system folders)", path.display())
                        } else {
                            format!("Saving to {}", path.display())
                        }
                    })
                    .unwrap_or_else(|| "No download folder selected.".to_owned()),
            );
            if download_button(
                ui,
                "Download all",
                !self.queue.is_empty() && !self.downloading && self.download_dir.is_some(),
            )
            .clicked()
            {
                self.start_downloads(self.queue.clone());
            }
            if ui
                .add_enabled(!self.queue.is_empty(), egui::Button::new("Remove all"))
                .clicked()
            {
                match save_queue(&[]) {
                    Ok(()) => {
                        self.queue.clear();
                        self.queue_status = "Removed all queued files.".to_owned();
                    }
                    Err(error) => {
                        self.queue_status = format!("Could not clear the saved queue: {error}");
                    }
                }
            }
        });
        ui.label(&self.queue_status);
        if self.queue.is_empty() {
            ui.label("Your queue is empty. Use the More menu beside a download to add files.");
            return;
        }
        let mut remove = HashSet::new();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for item in self.queue.clone() {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.strong(&item.name);
                            ui.label(egui::RichText::new(&item.full_path).weak().small());
                        });
                        if download_button(ui, "Download", !self.downloading).clicked() {
                            self.start_downloads(vec![item.clone()]);
                        }
                        if ui.button("Remove").clicked() {
                            remove.insert(item.id);
                        }
                    });
                    ui.separator();
                }
            });
        if !remove.is_empty() {
            self.queue.retain(|item| !remove.contains(&item.id));
            if let Err(error) = save_queue(&self.queue) {
                self.queue_status = format!("Could not save the queue: {error}");
            }
        }
    }
}

impl eframe::App for NativeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        egui::CentralPanel::default().show(ctx, |ui| {
            self.toolbar(ui);
            match self.view {
                View::Search => self.search_view(ui),
                View::Browse => self.browse_view(ui),
                View::Queue => self.queue_view(ui),
                View::Seeding => {
                    ui.heading("Seeding");
                    ui.label("Seeding controls are coming soon. This tab is a placeholder.");
                }
                View::Settings => self.settings_view(ui),
            }
        });
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

// Lists are named like "NES (NA)", so the count follows after a dot.
fn list_label(name: &str, count: usize) -> String {
    format!("{name} · {count}")
}

// The whole-preset lists first, then each system's list; returns what the user picked.
fn preset_menu(
    ui: &mut egui::Ui,
    preset: &Preset,
    regions: &PresetRegions,
) -> Option<(String, Vec<QueueItem>)> {
    let mut picked = None;
    let mut entry = |ui: &mut egui::Ui, group: &PresetGroup, hover: Option<&str>| {
        let mut response = ui.button(list_label(&group.name, group.items.len()));
        if let Some(hover) = hover {
            response = response.on_hover_text(hover);
        }
        if response.clicked() {
            picked = Some((
                format!("{}, {}", preset.name, group.name),
                group.items.clone(),
            ));
        }
    };
    let shown = |group: &&PresetGroup| regions.shows(group.region);
    for group in preset.top.iter().filter(shown) {
        entry(ui, group, Some(preset.note.as_str()));
    }
    if preset
        .groups
        .iter()
        .any(|group| regions.shows(group.region))
    {
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(420.0)
            .show(ui, |ui| {
                for group in preset.groups.iter().filter(shown) {
                    entry(ui, group, None);
                }
            });
    }
    picked
}

fn paint_action_icon(ui: &egui::Ui, response: &egui::Response, hamburger: bool) {
    let center = egui::pos2(response.rect.left() + 10.0, response.rect.center().y);
    let stroke = egui::Stroke::new(1.5, ui.style().interact(response).text_color());
    let painter = ui.painter();
    if hamburger {
        for y in [-4.0, 0.0, 4.0] {
            painter.line_segment(
                [center + egui::vec2(-4.0, y), center + egui::vec2(4.0, y)],
                stroke,
            );
        }
    } else {
        painter.line_segment(
            [
                center + egui::vec2(0.0, -5.0),
                center + egui::vec2(0.0, 5.0),
            ],
            stroke,
        );
        for x in [-4.0, 4.0] {
            painter.line_segment(
                [center + egui::vec2(x, 1.0), center + egui::vec2(0.0, 5.0)],
                stroke,
            );
        }
    }
}

fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(52.0, 28.0);
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, "")
    });
    if ui.is_rect_visible(rect) {
        let progress = ui.ctx().animate_bool_responsive(response.id, *on);
        let accent = egui::Color32::from_rgb(159, 122, 234);
        let off_color = egui::Color32::from_gray(70);
        let track = off_color.lerp_to_gamma(accent, progress);
        let radius = rect.height() / 2.0;
        let painter = ui.painter();
        painter.rect_filled(rect, radius, track);
        if response.hovered() {
            painter.rect_stroke(
                rect,
                radius,
                egui::Stroke::new(1.5, egui::Color32::from_white_alpha(70)),
                egui::StrokeKind::Inside,
            );
        }
        let knob_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), progress);
        painter.circle_filled(
            egui::pos2(knob_x, rect.center().y),
            radius - 3.5,
            egui::Color32::WHITE,
        );
    }
    response
}

fn folder_icon(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 14.0), egui::Sense::hover());
    let color = egui::Color32::from_rgb(159, 122, 234);
    let painter = ui.painter();
    let tab = egui::Rect::from_min_size(rect.min + egui::vec2(1.0, 1.0), egui::vec2(6.0, 3.0));
    let body = egui::Rect::from_min_max(
        rect.min + egui::vec2(1.0, 3.0),
        rect.max - egui::vec2(1.0, 1.0),
    );
    painter.rect_filled(tab, 1.0, color);
    painter.rect_filled(body, 1.5, color);
}

fn parent_button(ui: &mut egui::Ui) -> egui::Response {
    let response = ui.button("    Parent");
    let center = egui::pos2(response.rect.left() + 10.0, response.rect.center().y);
    let stroke = egui::Stroke::new(1.5, ui.style().interact(&response).text_color());
    let painter = ui.painter();
    painter.line_segment(
        [
            center + egui::vec2(0.0, 5.0),
            center + egui::vec2(0.0, -5.0),
        ],
        stroke,
    );
    for x in [-4.0, 4.0] {
        painter.line_segment(
            [center + egui::vec2(x, -1.0), center + egui::vec2(0.0, -5.0)],
            stroke,
        );
    }
    response
}

fn download_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let response = ui.button(format!("    {label}"));
        paint_action_icon(ui, &response, false);
        response
    })
    .inner
}

fn more_menu(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui)) {
    let response = ui.menu_button("    More", contents);
    paint_action_icon(ui, &response.response, true);
}

fn navigation_button(ui: &mut egui::Ui, next: bool, enabled: bool) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let response = ui.button(if next { "Next    " } else { "    Previous" });
        let center = egui::pos2(
            if next {
                response.rect.right() - 10.0
            } else {
                response.rect.left() + 10.0
            },
            response.rect.center().y,
        );
        let direction = if next { 1.0 } else { -1.0 };
        let stroke = egui::Stroke::new(1.5, ui.style().interact(&response).text_color());
        let tip = center + egui::vec2(5.0 * direction, 0.0);
        ui.painter()
            .line_segment([center + egui::vec2(-5.0 * direction, 0.0), tip], stroke);
        for y in [-4.0, 4.0] {
            ui.painter()
                .line_segment([center + egui::vec2(direction, y), tip], stroke);
        }
        response
    })
    .inner
}

fn matching_filters(options: &[String], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    let mut matches: Vec<_> = options
        .iter()
        .enumerate()
        .filter(|(_, value)| value.to_lowercase().contains(&query))
        .map(|(index, _)| index)
        .collect();
    matches.sort_by_key(|&index| !options[index].to_lowercase().starts_with(&query));
    matches
}

fn combo(
    ui: &mut egui::Ui,
    label: &str,
    id: &str,
    options: &[String],
    selected: &mut usize,
    query: &mut String,
) {
    let state_id = ui.make_persistent_id((id, "was_open"));
    let was_open = ui.data(|data| data.get_temp::<bool>(state_id).unwrap_or(false));
    let response = egui::ComboBox::from_id_salt(id)
        .width(140.0)
        .height(280.0)
        .wrap_mode(egui::TextWrapMode::Truncate)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .selected_text(
            selected
                .checked_sub(1)
                .and_then(|index| options.get(index))
                .map(String::as_str)
                .unwrap_or(label),
        )
        .show_ui(ui, |ui| {
            ui.set_min_width(280.0);
            ui.set_min_height(280.0);
            if !was_open {
                query.clear();
            }
            let input = ui.add(
                egui::TextEdit::singleline(query)
                    .id_salt((id, "search"))
                    .hint_text(format!("Type to find a {label}"))
                    .desired_width(260.0),
            );
            if !was_open {
                input.request_focus();
            }
            let matches = matching_filters(options, query);
            if (input.has_focus() || input.lost_focus())
                && ui.input(|input| input.key_pressed(egui::Key::Enter))
                && let Some(&index) = matches.first()
            {
                *selected = index + 1;
                ui.close_menu();
            }
            ui.separator();
            if ui
                .selectable_value(selected, 0, format!("All {label}s"))
                .clicked()
            {
                ui.close_menu();
            }
            if matches.is_empty() {
                ui.label("No matching tags.");
            }
            for index in matches {
                if ui
                    .selectable_value(selected, index + 1, &options[index])
                    .clicked()
                {
                    ui.close_menu();
                }
            }
        });
    ui.data_mut(|data| data.insert_temp(state_id, response.inner.is_some()));
}

async fn fetch_directory(client: &Client, path: &str) -> anyhow::Result<Directory> {
    let url = Url::parse(ARCHIVE)?.join(path)?;
    if url.origin().ascii_serialization() != ARCHIVE || !url.path().starts_with("/browse/") {
        bail!("Invalid archive directory.");
    }
    let html = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    parse_directory(&html, path)
}

fn parse_directory(html: &str, path: &str) -> anyhow::Result<Directory> {
    let document = Html::parse_document(html);
    let entry_selector = Selector::parse("div.entry").expect("valid selector");
    let anchor_selector = Selector::parse("a").expect("valid selector");
    let span_selector = Selector::parse("span").expect("valid selector");
    let mut entries = Vec::new();
    let archive_url = Url::parse(ARCHIVE)?;

    for entry in document.select(&entry_selector) {
        if entry.value().classes().any(|class| class == "search_back") {
            continue;
        }
        let Some(anchor) = entry.select(&anchor_selector).next() else {
            continue;
        };
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let name = anchor.text().collect::<String>().trim().to_owned();
        if name.is_empty() {
            continue;
        }
        let target = archive_url.join(href)?;
        if target.origin().ascii_serialization() != ARCHIVE {
            continue;
        }
        let is_directory = name.ends_with('/');
        let id = if target.path() == "/rom" {
            target
                .query_pairs()
                .find(|(key, _)| key == "id")
                .and_then(|(_, value)| value.parse::<u64>().ok())
                .filter(|id| *id > 0)
        } else {
            None
        };
        entries.push(DirectoryEntry {
            name,
            path: format!(
                "{}{}",
                target.path(),
                target
                    .query()
                    .map(|query| format!("?{query}"))
                    .unwrap_or_default()
            ),
            is_directory,
            size: entry
                .select(&span_selector)
                .next()
                .map(|span| span.text().collect::<String>().trim().to_owned()),
            id,
        });
    }

    let parent_selector = Selector::parse("div.entry.search_back a").expect("valid selector");
    let parent = document
        .select(&parent_selector)
        .next()
        .and_then(|anchor| anchor.value().attr("href"))
        .and_then(|href| archive_url.join(href).ok())
        .filter(|url| url.origin().ascii_serialization() == ARCHIVE)
        .map(|url| url.path().to_owned());
    let mut breadcrumbs = vec![Breadcrumb {
        name: "Root".to_owned(),
        path: "/browse/".to_owned(),
    }];
    let current_url = archive_url.join(path)?;
    let decoded_path = current_url.path().trim_start_matches("/browse/");
    let mut accumulated = "/browse/".to_owned();
    for part in decoded_path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
    {
        let decoded = url::form_urlencoded::parse(format!("part={part}").as_bytes())
            .next()
            .map(|(_, value)| value.into_owned())
            .unwrap_or_else(|| part.to_owned());
        accumulated.push_str(
            &url::form_urlencoded::byte_serialize(decoded.as_bytes()).collect::<String>(),
        );
        accumulated.push('/');
        breadcrumbs.push(Breadcrumb {
            name: decoded,
            path: accumulated.clone(),
        });
    }
    Ok(Directory {
        entries,
        parent,
        breadcrumbs,
    })
}

async fn get_with_retries(
    client: &Client,
    url: Url,
    attempt_timeout: Duration,
    attempts: u32,
    mut on_retry: impl FnMut(u32, u32),
) -> anyhow::Result<Vec<u8>> {
    let mut last_error = None;
    for attempt in 1..=attempts {
        if attempt > 1 {
            on_retry(attempt, attempts);
            tokio::time::sleep(Duration::from_secs(u64::from(attempt - 1))).await;
        }
        // The archive sometimes leaves a request hanging that succeeds when retried.
        let result: Result<Vec<u8>, (bool, anyhow::Error)> = async {
            let response = client
                .get(url.clone())
                .timeout(attempt_timeout)
                .send()
                .await
                .map_err(|error| (true, error.into()))?;
            let status = response.status();
            if !status.is_success() {
                let retry = status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS;
                return Err((retry, anyhow!("MiNERVA returned HTTP {status}")));
            }
            response
                .bytes()
                .await
                .map(|bytes| bytes.to_vec())
                .map_err(|error| (true, error.into()))
        }
        .await;
        match result {
            Ok(bytes) => return Ok(bytes),
            Err((false, error)) => return Err(error),
            Err((true, error)) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("No attempts were made")))
        .with_context(|| format!("MiNERVA did not respond after {attempts} attempts"))
}

async fn fetch_rom_details(
    client: &Client,
    id: u64,
    on_retry: impl FnMut(u32, u32),
) -> anyhow::Result<RomDetails> {
    let url = Url::parse(&format!("{ARCHIVE}/rom?id={id}"))?;
    let html = get_with_retries(client, url, Duration::from_secs(10), 4, on_retry).await?;
    parse_rom_details(&String::from_utf8_lossy(&html))
}

fn parse_rom_details(html: &str) -> anyhow::Result<RomDetails> {
    let assignment = Regex::new(r"window\.rom\s*=\s*(\{[^;]+\});")?;
    let json = assignment
        .captures(html)
        .and_then(|captures| captures.get(1))
        .context("Archive ROM details are missing.")?
        .as_str();
    let details: RomDetails =
        serde_json::from_str(json).context("Archive returned invalid ROM details.")?;
    if details.file_name.is_empty()
        || details.full_path.is_empty()
        || details.torrents.is_empty()
        || details.magnet.is_empty()
        || details.size == 0
    {
        bail!("Archive ROM details are incomplete.");
    }
    Ok(details)
}

const STALL_TIMEOUT: Duration = Duration::from_secs(120);

async fn download_queue(
    client: Client,
    tx: Sender<UiEvent>,
    items: Vec<QueueItem>,
    output_dir: PathBuf,
    organize_by_system: bool,
) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(&output_dir)
        .await
        .with_context(|| format!("Could not create download folder {}", output_dir.display()))?;
    let staging = tempfile::tempdir().context("Could not create temporary download folder")?;
    let session = Session::new(staging.path().to_owned())
        .await
        .context("Could not start BitTorrent session")?;
    let mut torrent_cache: HashMap<String, Vec<u8>> = HashMap::new();
    let context = DownloadContext {
        client: &client,
        tx: &tx,
        session: &session,
        staging: staging.path(),
        output_dir: &output_dir,
        organize_by_system,
    };
    let mut failures = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let result =
            download_item(&context, &mut torrent_cache, item, (index + 1, items.len())).await;
        if let Err(error) = result {
            let message = format!("{}: {error:#}", item.name);
            let _ = tx.send(UiEvent::DownloadStatus(format!("Skipped {message}")));
            failures.push(message);
        }
    }
    session.stop().await;
    if failures.is_empty() {
        return Ok(());
    }
    let shown = failures
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    let more = failures.len().saturating_sub(3);
    bail!(
        "{} of {} downloads failed — {shown}{}",
        failures.len(),
        items.len(),
        if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        }
    )
}

struct DownloadContext<'a> {
    client: &'a Client,
    tx: &'a Sender<UiEvent>,
    session: &'a Arc<Session>,
    staging: &'a Path,
    output_dir: &'a Path,
    organize_by_system: bool,
}

const MAX_CACHED_TORRENTS: usize = 6;

async fn download_item(
    context: &DownloadContext<'_>,
    torrent_cache: &mut HashMap<String, Vec<u8>>,
    item: &QueueItem,
    position: (usize, usize),
) -> anyhow::Result<()> {
    let DownloadContext { client, tx, .. } = context;
    let Some(file) = &item.file else {
        let file = fetch_live_file(client, tx, item).await?;
        return download_file(context, torrent_cache, item, &file, position).await;
    };
    let error = match download_file(context, torrent_cache, item, file, position).await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    // Information saved with the item can go out of date, so try the live record once.
    match fetch_live_file(client, tx, item).await {
        Ok(mut live) => {
            live.folder = file.folder.clone();
            if live == *file {
                Err(error)
            } else {
                download_file(context, torrent_cache, item, &live, position).await
            }
        }
        Err(_) => Err(error),
    }
}

async fn fetch_live_file(
    client: &Client,
    tx: &Sender<UiEvent>,
    item: &QueueItem,
) -> anyhow::Result<ArchiveFile> {
    let _ = tx.send(UiEvent::DownloadStatus(format!(
        "Fetching ROM details for {}…",
        item.name
    )));
    let details = fetch_rom_details(client, item.id, |attempt, attempts| {
        let _ = tx.send(UiEvent::DownloadStatus(format!(
            "MiNERVA is slow to respond · retrying ROM details for {} ({attempt}/{attempts})…",
            item.name
        )));
    })
    .await?;
    if details.id != item.id {
        bail!("Archive returned a different ROM record.");
    }
    Ok(ArchiveFile {
        file_name: details.file_name,
        torrents: details.torrents,
        so_id: details.so_id,
        size: details.size,
        folder: None,
    })
}

async fn download_file(
    context: &DownloadContext<'_>,
    torrent_cache: &mut HashMap<String, Vec<u8>>,
    item: &QueueItem,
    file: &ArchiveFile,
    (position, total): (usize, usize),
) -> anyhow::Result<()> {
    let DownloadContext {
        client,
        tx,
        session,
        staging,
        output_dir,
        organize_by_system,
    } = context;
    let destination = destination_path(output_dir, file, *organize_by_system)?;
    if tokio::fs::metadata(&destination)
        .await
        .is_ok_and(|metadata| metadata.len() == file.size)
    {
        let _ = tx.send(UiEvent::DownloadStatus(format!(
            "{position}/{total} · {} is already downloaded.",
            file.file_name
        )));
        return Ok(());
    }
    let _ = tx.send(UiEvent::DownloadStatus(format!(
        "{position}/{total} · fetching torrent metadata for {}…",
        item.name
    )));
    let torrent_bytes = match torrent_cache.get(&file.torrents) {
        Some(bytes) => bytes.clone(),
        None => {
            let bytes = fetch_torrent_file(client, &file.torrents, |attempt, attempts| {
                let _ = tx.send(UiEvent::DownloadStatus(format!(
                    "MiNERVA is slow to respond · retrying torrent metadata for {} ({attempt}/{attempts})…",
                    item.name
                )));
            })
            .await?;
            if torrent_cache.len() >= MAX_CACHED_TORRENTS {
                torrent_cache.clear();
            }
            torrent_cache.insert(file.torrents.clone(), bytes.clone());
            bytes
        }
    };
    let temp =
        tempfile::tempdir_in(staging).context("Could not create temporary download folder")?;
    let mut retried = false;
    let (id, handle) = loop {
        let response = session
            .add_torrent(
                AddTorrent::from_bytes(torrent_bytes.clone()),
                Some(AddTorrentOptions {
                    storage_factory: Some(
                        torrent_storage::OnDemandStorageFactory(temp.path().to_owned()).boxed(),
                    ),
                    only_files: Some(vec![file.so_id]),
                    output_folder: Some(temp.path().to_string_lossy().into_owned()),
                    overwrite: false,
                    ..Default::default()
                }),
            )
            .await
            .context("Could not add archive torrent")?;
        match response {
            AddTorrentResponse::Added(id, handle) => break (id, handle),
            // A leftover entry would keep another file's selection, so drop it and add again.
            AddTorrentResponse::AlreadyManaged(id, _) if !retried => {
                retried = true;
                let _ = session.delete(id.into(), true).await;
            }
            AddTorrentResponse::AlreadyManaged(..) => {
                bail!("The archive torrent is still active from a previous download.")
            }
            AddTorrentResponse::ListOnly(_) => {
                bail!("Torrent metadata was listed without starting a download.")
            }
        }
    };
    let result = transfer_file(tx, &handle, file, item, temp.path(), &destination).await;
    let _ = session.delete(id.into(), true).await;
    result
}

async fn transfer_file(
    tx: &Sender<UiEvent>,
    handle: &librqbit::ManagedTorrent,
    details: &ArchiveFile,
    item: &QueueItem,
    temp: &Path,
    destination: &Path,
) -> anyhow::Result<()> {
    let expected_path = handle
        .with_metadata(|metadata| {
            metadata
                .info
                .info()
                .files
                .as_ref()
                .and_then(|files| files.get(details.so_id))
                .map(|file| {
                    file.path.iter().fold(PathBuf::new(), |mut path, part| {
                        path.push(String::from_utf8_lossy(part.as_ref()).as_ref());
                        path
                    })
                })
        })?
        .context("Requested file is missing from the torrent.")?;
    if expected_path.file_name().and_then(|name| name.to_str()) != Some(details.file_name.as_str())
    {
        bail!("Archive details do not match the torrent file.");
    }
    let mut progress_timer = tokio::time::interval(Duration::from_secs(1));
    let completion = handle.wait_until_completed();
    tokio::pin!(completion);
    let mut best_received = 0;
    let mut last_progress = Instant::now();
    loop {
        tokio::select! {
            result = &mut completion => {
                result.context("BitTorrent transfer failed")?;
                break;
            }
            _ = progress_timer.tick() => {
                let received = handle.stats().file_progress
                    .get(details.so_id)
                    .copied()
                    .unwrap_or_default()
                    .min(details.size);
                if received > best_received {
                    best_received = received;
                    last_progress = Instant::now();
                }
                let stalled = last_progress.elapsed();
                if stalled >= STALL_TIMEOUT {
                    bail!(
                        "No download progress for {} seconds; no peers are sharing this file right now.",
                        STALL_TIMEOUT.as_secs()
                    );
                }
                let status = if received == 0 {
                    format!(
                        "Waiting for peers · {} ({}s)",
                        item.name,
                        stalled.as_secs()
                    )
                } else {
                    format!(
                        "Downloading {} · {} / {}",
                        item.name,
                        format_bytes(received),
                        format_bytes(details.size)
                    )
                };
                let _ = tx.send(UiEvent::DownloadStatus(status));
            }
        }
    }
    let _ = tx.send(UiEvent::DownloadStatus(format!(
        "Saving {}…",
        details.file_name
    )));
    let source_path = temp.join(expected_path);
    let metadata = tokio::fs::metadata(&source_path)
        .await
        .context("Downloaded archive file is missing")?;
    if metadata.len() != details.size {
        bail!("Downloaded file size does not match archive metadata.");
    }
    let mut source = tokio::fs::File::open(&source_path).await?;
    if let Some(folder) = destination.parent() {
        tokio::fs::create_dir_all(folder)
            .await
            .with_context(|| format!("Could not create folder {}", folder.display()))?;
    }
    let mut target = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .await
        .with_context(|| format!("Could not create {}", destination.display()))?;
    if let Err(error) = tokio::io::copy(&mut source, &mut target).await {
        let _ = tokio::fs::remove_file(destination).await;
        return Err(error).context("Could not save downloaded file");
    }
    Ok(())
}

async fn fetch_torrent_file(
    client: &Client,
    torrent_path: &str,
    on_retry: impl FnMut(u32, u32),
) -> anyhow::Result<Vec<u8>> {
    let parts: Vec<_> = torrent_path.split('/').collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        bail!("Archive returned an invalid torrent path.");
    }

    let mut url = Url::parse(ARCHIVE)?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow!("Archive URL cannot contain a path"))?;
        segments.push("assets");
        for part in parts {
            segments.push(part);
        }
    }
    get_with_retries(client, url, Duration::from_secs(45), 3, on_retry).await
}

// MiNERVA torrents are named "Minerva_Myrient - <Collection> - <System>.torrent", so the
// system name maps straight to the folder name that RetroBat and Batocera use for that
// system. The two projects agree on every name below; a few systems (rx78, videopacplus,
// beena, pico, sc3000) exist only in Batocera. Odyssey² is left out because the two
// differ (`odyssey²` vs `odyssey2`).
const SYSTEM_FOLDERS: &[(&str, &str)] = &[
    ("Nintendo - Nintendo Entertainment System (Headered)", "nes"),
    (
        "Nintendo - Nintendo Entertainment System (Headerless)",
        "nes",
    ),
    ("Nintendo - Family Computer Disk System (FDS)", "fds"),
    ("Nintendo - Super Nintendo Entertainment System", "snes"),
    ("Nintendo - Sufami Turbo", "sufami"),
    ("Nintendo - Satellaview", "satellaview"),
    ("Nintendo - Game Boy", "gb"),
    ("Nintendo - Game Boy Color", "gbc"),
    ("Nintendo - Game Boy Advance", "gba"),
    ("Nintendo - Nintendo 64 (BigEndian)", "n64"),
    ("Nintendo - Nintendo 64 (ByteSwapped)", "n64"),
    ("Nintendo - Nintendo 64DD", "n64dd"),
    ("Nintendo - Nintendo DS (Decrypted)", "nds"),
    ("Nintendo - Nintendo 3DS (Decrypted)", "3ds"),
    ("Nintendo - Virtual Boy", "virtualboy"),
    ("Nintendo - Pokemon Mini", "pokemini"),
    ("Nintendo - Game & Watch", "gameandwatch"),
    ("Nintendo - GameCube - NKit RVZ [zstd-19-128k]", "gamecube"),
    ("Nintendo - Wii - NKit RVZ [zstd-19-128k]", "wii"),
    ("Sega - Mega Drive - Genesis", "megadrive"),
    ("Sega - Master System - Mark III", "mastersystem"),
    ("Sega - Game Gear", "gamegear"),
    ("Sega - 32X", "sega32x"),
    ("Sega - SG-1000", "sg1000"),
    ("Sega - Mega CD & Sega CD", "megacd"),
    ("Sega - Saturn", "saturn"),
    ("Sega - Dreamcast", "dreamcast"),
    ("Sony - PlayStation", "psx"),
    ("Sony - PlayStation 2", "ps2"),
    ("Sony - PlayStation Portable", "psp"),
    ("Atari - Atari 2600", "atari2600"),
    ("Atari - Atari 5200", "atari5200"),
    ("Atari - Atari 7800 (BIN)", "atari7800"),
    ("Atari - Atari Lynx (LNX)", "lynx"),
    ("Atari - Atari Jaguar (J64)", "jaguar"),
    ("NEC - PC Engine - TurboGrafx-16", "pcengine"),
    ("NEC - PC Engine CD & TurboGrafx CD", "pcenginecd"),
    ("SNK - NeoGeo Pocket", "ngp"),
    ("SNK - NeoGeo Pocket Color", "ngpc"),
    ("SNK - Neo Geo CD", "neogeocd"),
    ("Bandai - WonderSwan", "wswan"),
    ("Bandai - WonderSwan Color", "wswanc"),
    ("Coleco - ColecoVision", "colecovision"),
    ("Mattel - Intellivision", "intellivision"),
    ("Philips - CD-i", "cdi"),
    ("Panasonic - 3DO Interactive Multiplayer", "3do"),
    ("Microsoft - Xbox", "xbox"),
    ("Microsoft - Xbox 360", "xbox360"),
    ("Atari - Atari ST", "atarist"),
    ("Atari - 8-bit Family", "atari800"),
    ("Atari - Atari 2600 (BIN)", "atari2600"),
    ("Atari - Atari Jaguar (JAG)", "jaguar"),
    ("Atari - Atari Jaguar (ROM)", "jaguar"),
    ("Atari - Atari Lynx (LYX)", "lynx"),
    (
        "Atari - Jaguar CD Interactive Multimedia System",
        "jaguarcd",
    ),
    ("Acorn - Archimedes", "archimedes"),
    ("Apple - II (WOZ)", "apple2"),
    ("Apple - IIGS (WOZ)", "apple2gs"),
    ("Bally - Astrocade", "astrocade"),
    ("Bandai - Gundam RX-78", "rx78"),
    ("Bit Corporation - Gamate", "gamate"),
    ("Casio - PV-1000", "pv1000"),
    ("Casio - Loopy (BigEndian)", "loopy"),
    ("Commodore - Amiga", "amiga500"),
    ("Commodore - Amiga CD32", "amigacd32"),
    ("Commodore - Amiga CDTV", "amigacdtv"),
    ("Commodore - Commodore 64", "c64"),
    ("Commodore - Commodore 64 (PP)", "c64"),
    ("Commodore - Commodore 64 (Tapes)", "c64"),
    ("Commodore - Plus-4", "cplus4"),
    ("Commodore - VIC-20", "c20"),
    ("Emerson - Arcadia 2001", "arcadia"),
    ("Entex - Adventure Vision", "advision"),
    ("Epoch - Game Pocket Computer", "gamepock"),
    ("Epoch - Super Cassette Vision", "scv"),
    ("Fairchild - Channel F", "channelf"),
    ("Fujitsu - FM-Towns", "fmtowns"),
    ("Funtech - Super Acan", "supracan"),
    ("GamePark - GP32", "gp32"),
    ("GCE - Vectrex", "vectrex"),
    ("Hartung - Game Master", "gmaster"),
    ("Interton - VC 4000", "vc4000"),
    ("Philips - Videopac+", "videopacplus"),
    ("Microsoft - MSX", "msx1"),
    ("Microsoft - MSX2", "msx2"),
    ("NEC - PC Engine SuperGrafx", "supergrafx"),
    ("NEC - PC-88 Series", "pc88"),
    ("NEC - PC-98", "pc98"),
    ("NEC - PC-98 Series", "pc98"),
    ("NEC - PC-FX & PC-FXGA", "pcfx"),
    ("Nintendo - Nintendo DS (Encrypted)", "nds"),
    ("Sega - Beena", "beena"),
    ("Sega - Pico", "pico"),
    ("Sega - SG-1000 - SC-3000", "sc3000"),
    ("Sharp - X68000", "x68000"),
    ("Sinclair - ZX Spectrum +3", "zxspectrum"),
    ("Tiger - Game.com", "gamecom"),
    ("VTech - CreatiVision", "crvision"),
    ("VTech - V.Smile", "vsmile"),
    ("Watara - Supervision", "supervision"),
    ("Welback - Mega Duck", "megaduck"),
    ("Sony - PlayStation 3", "ps3"),
    ("Arcade - Sega - Naomi", "naomi"),
    ("Arcade - Sega - Naomi 2", "naomi2"),
    ("Arcade - Sega - Chihiro", "chihiro"),
    ("Arcade - Sega - Lindbergh", "lindbergh"),
    ("Arcade - Namco - System 246", "namco2x6"),
    ("Arcade - Namco - Sega - Nintendo - Triforce", "triforce"),
];

fn system_folder(torrent_path: &str) -> Option<&'static str> {
    let file = torrent_path.rsplit('/').next()?;
    let name = file
        .strip_suffix(".torrent")?
        .strip_prefix("Minerva_Myrient - ")?;
    let (_collection, system) = name.split_once(" - ")?;
    SYSTEM_FOLDERS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(system))
        .map(|(_, folder)| *folder)
}

fn safe_file_name(name: &str) -> anyhow::Result<String> {
    let basename = name.replace('\\', "/");
    let basename = basename.rsplit('/').next().unwrap_or_default();
    if basename.is_empty() || basename == "." || basename == ".." {
        bail!("Archive returned an invalid file name.");
    }
    Ok(basename.to_owned())
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn config_file(name: &str) -> anyhow::Result<PathBuf> {
    let dirs = ProjectDirs::from("org", "FRAUM", "FRAUM")
        .context("Could not determine the FRAUM configuration directory")?;
    Ok(dirs.config_dir().join(name))
}

fn queue_file() -> anyhow::Result<PathBuf> {
    config_file("queue.json")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    organize_by_system: bool,
    preset_regions: PresetRegions,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            organize_by_system: true,
            preset_regions: PresetRegions::default(),
        }
    }
}

// Which regions' lists the Presets menu offers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct PresetRegions {
    na: bool,
    eu: bool,
    jp: bool,
}

impl Default for PresetRegions {
    fn default() -> Self {
        Self {
            na: true,
            eu: true,
            jp: true,
        }
    }
}

impl PresetRegions {
    fn shows(&self, region: &str) -> bool {
        match region {
            "NA" => self.na,
            "EU" => self.eu,
            "JP" => self.jp,
            _ => true,
        }
    }

    fn any(&self) -> bool {
        self.na || self.eu || self.jp
    }
}

fn load_settings() -> anyhow::Result<Settings> {
    let content = match fs::read_to_string(config_file("settings.json")?) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(error) => return Err(error.into()),
    };
    serde_json::from_str(&content).context("Saved settings are not valid JSON")
}

fn save_settings(settings: &Settings) -> anyhow::Result<()> {
    let path = config_file("settings.json")?;
    let parent = path
        .parent()
        .context("Settings path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(settings)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn destination_path(
    output_dir: &Path,
    file: &ArchiveFile,
    organize_by_system: bool,
) -> anyhow::Result<PathBuf> {
    let mut destination = output_dir.to_path_buf();
    if organize_by_system {
        let folder = file
            .folder
            .as_deref()
            .filter(|folder| {
                !folder.is_empty()
                    && folder
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            })
            .or_else(|| system_folder(&file.torrents));
        if let Some(folder) = folder {
            destination.push(folder);
        }
    }
    destination.push(safe_file_name(&file.file_name)?);
    Ok(destination)
}

// Queues saved before presets carried torrent details still need no archive lookup.
fn attach_preset_files(queue: &mut [QueueItem], presets: &[Preset]) {
    let known: HashMap<u64, &ArchiveFile> = presets
        .iter()
        .flat_map(|preset| &preset.groups)
        .flat_map(|group| &group.items)
        .filter_map(|item| item.file.as_ref().map(|file| (item.id, file)))
        .collect();
    for item in queue.iter_mut().filter(|item| item.file.is_none()) {
        item.file = known.get(&item.id).map(|file| (*file).clone());
    }
}

fn load_queue() -> anyhow::Result<Vec<QueueItem>> {
    let content = match fs::read_to_string(queue_file()?) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let queue = serde_json::from_str::<Vec<QueueItem>>(&content)
        .context("Saved queue is not valid JSON")?;
    Ok(queue
        .into_iter()
        .filter(|item| item.id > 0 && !item.name.is_empty() && !item.full_path.is_empty())
        .collect())
}

fn save_queue(queue: &[QueueItem]) -> anyhow::Result<()> {
    let path = queue_file()?;
    let parent = path
        .parent()
        .context("Queue path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(queue)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn dirs_download_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| {
        dirs.download_dir()
            .map_or_else(|| dirs.home_dir().join("Downloads"), Path::to_owned)
    })
}

fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon-256.png"))
        .expect("bundled app icon is a valid PNG")
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([760.0, 560.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "FRAUM — MiNERVA Archive",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(NativeApp::new()))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_app_icon_decodes_to_a_square_rgba_image() {
        let icon = app_icon();
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        // Rounded corners are transparent while the centre of the tile is opaque.
        assert_eq!(icon.rgba[3], 0);
        let centre = (128 * 256 + 20) * 4;
        assert_eq!(icon.rgba[centre + 3], 255);
    }

    fn serve_once(responses: Vec<Option<&'static str>>) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut hung = Vec::new();
            for (stream, response) in listener.incoming().zip(responses) {
                let mut stream = stream.unwrap();
                match response {
                    None => hung.push(stream),
                    Some(response) => {
                        let _ = stream.read(&mut [0; 1024]);
                        let _ = stream.write_all(response.as_bytes());
                    }
                }
            }
        });
        port
    }

    #[tokio::test]
    async fn retries_requests_the_archive_leaves_hanging() {
        let port = serve_once(vec![
            None,
            Some("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"),
        ]);
        let mut retries = Vec::new();
        let bytes = get_with_retries(
            &Client::new(),
            Url::parse(&format!("http://127.0.0.1:{port}/rom")).unwrap(),
            Duration::from_millis(300),
            3,
            |attempt, attempts| retries.push((attempt, attempts)),
        )
        .await
        .unwrap();
        assert_eq!(bytes, b"ok");
        assert_eq!(retries, [(2, 3)]);
    }

    #[tokio::test]
    async fn does_not_retry_missing_records_and_gives_up_on_hangs() {
        let port = serve_once(vec![Some(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )]);
        let mut retries = 0;
        let error = get_with_retries(
            &Client::new(),
            Url::parse(&format!("http://127.0.0.1:{port}/rom")).unwrap(),
            Duration::from_millis(300),
            3,
            |_, _| retries += 1,
        )
        .await
        .unwrap_err();
        assert_eq!(retries, 0);
        assert!(error.to_string().contains("404"), "{error:#}");

        let port = serve_once(vec![None, None]);
        let error = get_with_retries(
            &Client::new(),
            Url::parse(&format!("http://127.0.0.1:{port}/rom")).unwrap(),
            Duration::from_millis(200),
            2,
            |_, _| {},
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("did not respond after 2 attempts"),
            "{error:#}"
        );
    }

    #[test]
    fn old_saved_queues_gain_preset_details_without_losing_other_items() {
        let presets = load_presets();
        let preset_item = &presets[0].groups[0].items[0];
        let old_format = format!(
            r#"[{{"id":{},"name":"{}","full_path":"x"}},{{"id":1,"name":"Other.zip","full_path":"y"}}]"#,
            preset_item.id, preset_item.name
        );
        let mut queue: Vec<QueueItem> = serde_json::from_str(&old_format).unwrap();
        attach_preset_files(&mut queue, &presets);
        assert_eq!(queue[0].file, preset_item.file);
        assert!(queue[1].file.is_none());
        let saved = serde_json::to_string(&queue[1]).unwrap();
        assert!(!saved.contains("file"), "{saved}");
    }

    #[test]
    fn toggle_switch_flips_when_clicked_and_reports_the_change() {
        let ctx = egui::Context::default();
        let mut on = true;
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 200.0));
        let run = |events: Vec<egui::Event>, on: &mut bool| {
            let mut result = (egui::Rect::NOTHING, false);
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let response = toggle_switch(ui, on);
                        result = (response.rect, response.changed());
                    });
                },
            );
            result
        };
        let (rect, _) = run(vec![], &mut on);
        assert!(rect.width() >= 48.0 && rect.height() >= 24.0, "{rect:?}");
        let button = |pressed| egui::Event::PointerButton {
            pos: rect.center(),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        run(vec![egui::Event::PointerMoved(rect.center())], &mut on);
        run(vec![button(true)], &mut on);
        let (_, changed) = run(vec![button(false)], &mut on);
        assert!(changed);
        assert!(!on, "first click should turn the switch off");
        run(vec![button(true)], &mut on);
        let (_, changed) = run(vec![button(false)], &mut on);
        assert!(changed);
        assert!(on, "second click should turn it back on");
    }

    #[test]
    fn virtual_console_has_complete_regional_lists_for_every_system() {
        let presets = load_presets();
        let preset = presets
            .iter()
            .find(|preset| preset.name == "Virtual Console")
            .expect("Virtual Console preset exists");
        let top: Vec<_> = preset
            .top
            .iter()
            .map(|group| (group.name.as_str(), group.items.len()))
            .collect();
        assert_eq!(
            top,
            [
                ("All Games (NA)", 678),
                ("All Games (EU)", 669),
                ("All Games (JP)", 1049),
            ]
        );

        // Games per system in North America, Europe and Japan (0 where there are none).
        let expected: &[(&str, [usize; 3])] = &[
            ("NES", [120, 112, 199]),
            ("SNES", [81, 81, 125]),
            ("Nintendo 64", [25, 25, 24]),
            ("Game Boy / Color", [82, 79, 98]),
            ("Game Boy Advance", [74, 70, 104]),
            ("Nintendo DS", [31, 31, 31]),
            ("TurboGrafx-16 / PC Engine", [70, 68, 134]),
            ("Master System", [16, 16, 14]),
            ("Sega Genesis", [75, 76, 92]),
            ("Game Gear", [16, 16, 21]),
            ("Neo Geo", [57, 55, 76]),
            ("Commodore 64", [8, 17, 0]),
            ("MSX", [0, 0, 27]),
            ("Arcade", [23, 23, 104]),
        ];
        let mut names = Vec::new();
        for (system, counts) in expected {
            for (code, count) in ["NA", "EU", "JP"].into_iter().zip(counts) {
                if *count > 0 {
                    names.push((format!("{} ({code})", system_label(system, code)), *count));
                }
            }
        }
        let actual: Vec<_> = preset
            .groups
            .iter()
            .map(|group| (group.name.clone(), group.items.len()))
            .collect();
        assert_eq!(actual, names);

        // Each region's system lists split its All Games list exactly, with no repeats.
        for (code, all) in ["NA", "EU", "JP"].into_iter().zip(&preset.top) {
            let suffix = format!(" ({code})");
            let mut from_systems = Vec::new();
            for group in preset.groups.iter().filter(|g| g.name.ends_with(&suffix)) {
                from_systems.extend(group.items.iter().map(|item| item.id));
            }
            let unique: HashSet<_> = from_systems.iter().copied().collect();
            assert_eq!(unique.len(), from_systems.len(), "{code} repeats a game");
            let everything: HashSet<_> = all.items.iter().map(|item| item.id).collect();
            assert_eq!(
                unique, everything,
                "{code} system lists differ from All Games"
            );
        }

        // Every game belongs to some list, and each folder suits its system.
        let allowed: &[(&str, &[&str])] = &[
            ("NES", &["nes", "fds"]),
            ("SNES", &["snes"]),
            ("Nintendo 64", &["n64"]),
            ("Game Boy / Color", &["gb", "gbc"]),
            ("Game Boy Advance", &["gba"]),
            ("Nintendo DS", &["nds"]),
            (
                "TurboGrafx-16 / PC Engine",
                &["pcengine", "pcenginecd", "supergrafx"],
            ),
            ("Master System", &["mastersystem"]),
            ("Sega Genesis", &["megadrive"]),
            ("Mega Drive", &["megadrive"]),
            ("Game Gear", &["gamegear"]),
            ("Commodore 64", &["c64"]),
            ("MSX", &["msx1", "msx2"]),
        ];
        let mut distinct = HashSet::new();
        for group in &preset.groups {
            let (system, _) = group.name.rsplit_once(" (").unwrap();
            for item in &group.items {
                distinct.insert(item.id);
                let file = item.file.as_ref().expect("preset items carry torrent info");
                assert!(file.size > 0 && !file.file_name.is_empty(), "{}", item.name);
                let destination = destination_path(Path::new("/roms"), file, true).unwrap();
                let folder = destination
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .unwrap();
                match system {
                    "Neo Geo" => assert_eq!(folder, "neogeo", "{}", item.name),
                    "Arcade" => assert_eq!(folder, "fbneo", "{}", item.name),
                    name => {
                        let (_, folders) = allowed.iter().find(|(n, _)| *n == name).unwrap();
                        assert!(
                            folders.contains(&folder),
                            "{} landed in {folder}, expected one of {folders:?}",
                            item.name
                        );
                    }
                }
            }
        }
        assert_eq!(distinct.len(), 1784);
        // Neo Geo games need the BIOS, which leads each region's Neo Geo list.
        for group in preset
            .groups
            .iter()
            .filter(|g| g.name.starts_with("Neo Geo ("))
        {
            assert_eq!(
                group.items[0].file.as_ref().unwrap().file_name,
                "neogeo.zip",
                "{}",
                group.name
            );
        }
    }

    #[test]
    fn each_region_list_prefers_that_regions_version_of_a_game() {
        let presets = load_presets();
        let preset = presets
            .iter()
            .find(|preset| preset.name == "Virtual Console")
            .unwrap();
        let list = |name: &str| {
            preset
                .groups
                .iter()
                .find(|group| group.name == name)
                .unwrap_or_else(|| panic!("{name} exists"))
        };
        let has = |name: &str, file: &str| {
            list(name)
                .items
                .iter()
                .any(|item| item.file.as_ref().unwrap().file_name == file)
        };
        // Mega Man X is Rockman X in Japan, with a different ROM from the USA one.
        assert!(has("SNES (NA)", "Mega Man X (USA).zip"));
        assert!(!has("SNES (NA)", "Rockman X (Japan).zip"));
        assert!(has("SNES (JP)", "Rockman X (Japan).zip"));
        assert!(!has("SNES (JP)", "Mega Man X (USA).zip"));
    }

    #[test]
    fn nintendo_classics_has_regional_lists_that_split_exactly_by_system() {
        let presets = load_presets();
        let preset = &presets[0];
        assert_eq!(preset.name, "Nintendo Classics");
        let top: Vec<_> = preset
            .top
            .iter()
            .map(|g| (g.name.as_str(), g.items.len()))
            .collect();
        assert_eq!(
            top,
            [
                ("All Games (NA)", 362),
                ("All Games (EU)", 362),
                ("All Games (JP)", 352),
            ]
        );
        // North America and Europe are one column on Wikipedia, so they hold the same games.
        let expected: &[(&str, [usize; 3])] = &[
            ("NES", [87, 87, 86]),
            ("SNES", [78, 78, 73]),
            ("Game Boy / Color", [46, 46, 44]),
            ("Nintendo 64", [43, 43, 36]),
            ("Sega Genesis", [50, 50, 50]),
            ("Game Boy Advance", [32, 32, 36]),
            ("GameCube", [12, 12, 12]),
            ("Virtual Boy", [14, 14, 15]),
        ];
        let mut names = Vec::new();
        for (system, counts) in expected {
            for (code, count) in ["NA", "EU", "JP"].into_iter().zip(counts) {
                names.push((format!("{} ({code})", system_label(system, code)), *count));
            }
        }
        let actual: Vec<_> = preset
            .groups
            .iter()
            .map(|g| (g.name.clone(), g.items.len()))
            .collect();
        assert_eq!(actual, names);
        for (code, all) in ["NA", "EU", "JP"].into_iter().zip(&preset.top) {
            let suffix = format!(" ({code})");
            let mut from_systems = Vec::new();
            for group in preset.groups.iter().filter(|g| g.name.ends_with(&suffix)) {
                from_systems.extend(group.items.iter().map(|item| item.id));
            }
            let unique: HashSet<_> = from_systems.iter().copied().collect();
            assert_eq!(unique.len(), from_systems.len(), "{code} repeats a game");
            let everything: HashSet<_> = all.items.iter().map(|item| item.id).collect();
            assert_eq!(
                unique, everything,
                "{code} system lists differ from All Games"
            );
        }
    }

    #[test]
    fn the_genesis_is_called_the_mega_drive_outside_north_america() {
        assert_eq!(system_label("Sega Genesis", "NA"), "Sega Genesis");
        assert_eq!(system_label("Sega Genesis", "EU"), "Mega Drive");
        assert_eq!(system_label("Sega Genesis", "JP"), "Mega Drive");
        assert_eq!(system_label("NES", "JP"), "NES");
        assert_eq!(system_label("Sega CD", "NA"), "Sega CD");
        assert_eq!(system_label("Sega CD", "EU"), "Mega CD");
        assert_eq!(system_label("Sega CD", "JP"), "Mega CD");
        for preset in load_presets().iter().filter(|p| !p.groups.is_empty()) {
            let names: Vec<_> = preset.groups.iter().map(|g| g.name.as_str()).collect();
            assert!(names.contains(&"Sega Genesis (NA)"), "{}", preset.name);
            assert!(names.contains(&"Mega Drive (EU)"), "{}", preset.name);
            assert!(names.contains(&"Mega Drive (JP)"), "{}", preset.name);
            assert!(!names.contains(&"Sega Genesis (EU)") && !names.contains(&"Sega Genesis (JP)"));
        }
    }

    #[test]
    fn nintendo_classics_regional_lists_use_that_regions_version() {
        let presets = load_presets();
        let preset = &presets[0];
        let has = |list: &str, file: &str| {
            preset
                .groups
                .iter()
                .find(|g| g.name == list)
                .unwrap_or_else(|| panic!("{list} exists"))
                .items
                .iter()
                .any(|item| item.name == file)
        };
        // Beyond Oasis is The Story of Thor in Japan, with its own ROM.
        assert!(has("Sega Genesis (NA)", "Beyond Oasis (USA).zip"));
        assert!(!has(
            "Sega Genesis (NA)",
            "Story of Thor, The - Hikari o Tsugumono (Japan).zip"
        ));
        assert!(has(
            "Mega Drive (JP)",
            "Story of Thor, The - Hikari o Tsugumono (Japan).zip"
        ));
        assert!(!has("Mega Drive (JP)", "Beyond Oasis (USA).zip"));
        // Japan-only games are only in the Japan lists.
        assert!(has("NES (JP)", "Tsuppari Oozumou (Japan).zip"));
        assert!(!has("NES (NA)", "Tsuppari Oozumou (Japan).zip"));
        assert!(!has("NES (EU)", "Tsuppari Oozumou (Japan).zip"));
    }

    #[test]
    fn an_explicit_folder_wins_and_unsafe_folders_are_ignored() {
        let file = |folder: Option<&str>| ArchiveFile {
            file_name: "mslug.zip".to_owned(),
            torrents: "Minerva_Myrient_v0.3/Minerva_Myrient - FinalBurn Neo.torrent".to_owned(),
            so_id: 4346,
            size: 13_165_412,
            folder: folder.map(str::to_owned),
        };
        let root = Path::new("/roms");
        assert_eq!(
            destination_path(root, &file(Some("neogeo")), true).unwrap(),
            Path::new("/roms/neogeo/mslug.zip")
        );
        assert_eq!(
            destination_path(root, &file(Some("neogeo")), false).unwrap(),
            Path::new("/roms/mslug.zip")
        );
        for unsafe_folder in ["../etc", "Neo Geo", "a/b", ""] {
            assert_eq!(
                destination_path(root, &file(Some(unsafe_folder)), true).unwrap(),
                Path::new("/roms/mslug.zip"),
                "{unsafe_folder:?}"
            );
        }
    }

    #[test]
    fn system_folders_are_on_by_default_and_can_be_turned_off() {
        assert!(Settings::default().organize_by_system);
        // Missing or older settings files keep the default.
        let empty: Settings = serde_json::from_str("{}").unwrap();
        assert!(empty.organize_by_system);
        let off: Settings = serde_json::from_str(r#"{"organize_by_system":false}"#).unwrap();
        assert!(!off.organize_by_system);

        let file = ArchiveFile {
            file_name: "Game (USA).zip".to_owned(),
            torrents: "Minerva_Myrient_v0.3/Minerva_Myrient - No-Intro - Nintendo - Super Nintendo Entertainment System.torrent".to_owned(),
            so_id: 0,
            size: 1,
            folder: None,
        };
        let root = Path::new("/roms");
        assert_eq!(
            destination_path(root, &file, true).unwrap(),
            Path::new("/roms/snes/Game (USA).zip")
        );
        assert_eq!(
            destination_path(root, &file, false).unwrap(),
            Path::new("/roms/Game (USA).zip")
        );
        let unknown = ArchiveFile {
            torrents: "Minerva_Myrient_v0.3/Minerva_Myrient - TOSEC - Sharp.torrent".to_owned(),
            ..file
        };
        assert_eq!(
            destination_path(root, &unknown, true).unwrap(),
            Path::new("/roms/Game (USA).zip")
        );
    }

    #[test]
    fn maps_minerva_systems_to_system_folders() {
        let torrent = |name: &str| format!("Minerva_Myrient_v0.3/Minerva_Myrient - {name}.torrent");
        assert_eq!(
            system_folder(&torrent("No-Intro - Sega - Mega Drive - Genesis")),
            Some("megadrive")
        );
        assert_eq!(
            system_folder(&torrent(
                "redump - nintendo - gamecube - nkit rvz [zstd-19-128k]"
            )),
            Some("gamecube")
        );
        for (system, folder) in [
            ("No-Intro - Atari - Atari ST", "atarist"),
            ("No-Intro - Commodore - Commodore 64", "c64"),
            ("No-Intro - Microsoft - MSX2", "msx2"),
            ("No-Intro - Sega - PICO", "pico"),
            ("No-Intro - Philips - Videopac+", "videopacplus"),
            ("Redump - Sony - PlayStation 3", "ps3"),
            ("Redump - Arcade - Sega - Naomi 2", "naomi2"),
            ("Redump - NEC - PC-98 Series", "pc98"),
            ("No-Intro - NEC - PC-98", "pc98"),
        ] {
            assert_eq!(system_folder(&torrent(system)), Some(folder), "{system}");
        }
        // Variants such as aftermarket dumps and unknown systems stay unsorted, and
        // Odyssey² is skipped because RetroBat and Batocera name it differently.
        assert_eq!(
            system_folder(&torrent(
                "No-Intro - Nintendo - Game Boy Advance (Aftermarket)"
            )),
            None
        );
        assert_eq!(
            system_folder(&torrent("No-Intro - Magnavox - Odyssey 2")),
            None
        );
        assert_eq!(system_folder(&torrent("TOSEC - Sharp")), None);
        assert_eq!(system_folder("Other/odd name.torrent"), None);
    }

    #[test]
    fn system_folder_table_has_no_shadowed_or_unsafe_entries() {
        let mut keys = HashSet::new();
        for (system, folder) in SYSTEM_FOLDERS {
            assert!(
                keys.insert(system.to_ascii_lowercase()),
                "duplicate system key: {system}"
            );
            assert!(
                !folder.is_empty()
                    && folder
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "folder name is not a plain lowercase name: {folder}"
            );
        }
    }

    #[test]
    fn every_nintendo_classics_game_lands_in_its_system_folder() {
        let presets = load_presets();
        let expected: &[(&str, &[&str])] = &[
            ("NES", &["nes", "fds"]),
            ("SNES", &["snes"]),
            ("Game Boy / Color", &["gb", "gbc"]),
            ("Nintendo 64", &["n64"]),
            ("Sega Genesis", &["megadrive"]),
            ("Mega Drive", &["megadrive"]),
            ("Game Boy Advance", &["gba"]),
            ("GameCube", &["gamecube"]),
            ("Virtual Boy", &["virtualboy"]),
        ];
        for group in &presets[0].groups {
            let (system, _) = group.name.rsplit_once(" (").unwrap();
            let (_, allowed) = expected
                .iter()
                .find(|(name, _)| *name == system)
                .expect("known group");
            for item in &group.items {
                let folder = system_folder(&item.file.as_ref().unwrap().torrents)
                    .unwrap_or_else(|| panic!("{} has no system folder", item.name));
                assert!(allowed.contains(&folder), "{} -> {folder}", item.name);
            }
        }
    }

    #[test]
    fn bundled_nintendo_classics_preset_is_valid() {
        let presets = load_presets();
        let preset = &presets[0];
        let mut distinct = HashSet::new();
        for item in preset.groups.iter().flat_map(|g| &g.items) {
            distinct.insert(item.id);
            assert!(
                item.id > 0
                    && item.name.ends_with(".zip")
                    && item.full_path.starts_with("Nintendo Classics / ")
                    && item.file.as_ref().is_some_and(|file| {
                        file.file_name == item.name
                            && file.size > 0
                            && file.torrents.ends_with(".torrent")
                    }),
                "{}",
                item.name
            );
        }
        assert_eq!(distinct.len(), 727);
    }

    #[test]
    fn playstation_classic_has_regional_lists_with_every_disc() {
        let presets = load_presets();
        let preset = presets
            .iter()
            .find(|p| p.name == "PlayStation Classic")
            .expect("preset exists");
        assert!(preset.groups.is_empty(), "one system needs no system lists");
        let names = |list: &str| -> Vec<&str> {
            preset
                .top
                .iter()
                .find(|g| g.name == list)
                .unwrap_or_else(|| panic!("{list} exists"))
                .items
                .iter()
                .map(|item| item.name.as_str())
                .collect()
        };
        let (na, eu, jp) = (
            names("All Games (NA)"),
            names("All Games (EU)"),
            names("All Games (JP)"),
        );
        assert_eq!((na.len(), eu.len(), jp.len()), (23, 23, 24));
        for disc in 1..=3 {
            assert!(na.contains(&format!("Final Fantasy VII (USA) (Disc {disc}).zip").as_str()));
            assert!(eu.contains(&format!("Final Fantasy VII (Europe) (Disc {disc}).zip").as_str()));
            assert!(jp.contains(&format!("Final Fantasy VII (Japan) (Disc {disc}).zip").as_str()));
        }
        assert!(na.contains(&"Metal Gear Solid (USA) (Disc 2).zip"));
        assert!(jp.contains(&"Parasite Eve (Japan) (Disc 2).zip"));
        assert!(jp.contains(&"Toushinden (Japan).zip"));
        assert!(!na.contains(&"Devil Dice (USA).zip") && !eu.iter().any(|n| n.contains("Devil")));
        assert!(na.contains(&"Persona (USA).zip") && eu.contains(&"Persona (USA).zip"));
        for item in preset.top.iter().flat_map(|g| &g.items) {
            let file = item.file.as_ref().unwrap();
            assert_eq!(system_folder(&file.torrents), Some("psx"), "{}", item.name);
            assert!(file.size > 100_000_000 && file.file_name == item.name);
        }
    }

    #[test]
    fn every_preset_region_is_shown_by_default_and_can_be_hidden() {
        let all = PresetRegions::default();
        assert!(all.na && all.eu && all.jp && all.any());
        assert!(Settings::default().preset_regions.shows("EU"));
        // Older settings files keep every region on.
        let old: Settings = serde_json::from_str(r#"{"organize_by_system":false}"#).unwrap();
        assert!(old.preset_regions.na && old.preset_regions.eu && old.preset_regions.jp);
        let partial: Settings = serde_json::from_str(r#"{"preset_regions":{"eu":false}}"#).unwrap();
        let regions = partial.preset_regions;
        assert!(regions.shows("NA") && !regions.shows("EU") && regions.shows("JP"));
        let none = PresetRegions {
            na: false,
            eu: false,
            jp: false,
        };
        assert!(!none.any());
        for preset in load_presets() {
            for group in preset.top.iter().chain(&preset.groups) {
                assert!(group.name.ends_with(&format!("({})", group.region)));
            }
        }
    }

    #[test]
    fn nes_classic_has_thirty_games_per_region_in_the_right_folders() {
        let presets = load_presets();
        let preset = presets
            .iter()
            .find(|p| p.name == "NES Classic Edition")
            .expect("preset exists");
        assert!(preset.groups.is_empty());
        let names = |list: &str| -> Vec<&str> {
            preset
                .top
                .iter()
                .find(|g| g.name == list)
                .unwrap_or_else(|| panic!("{list} exists"))
                .items
                .iter()
                .map(|item| item.name.as_str())
                .collect()
        };
        let (na, eu, jp) = (
            names("All Games (NA)"),
            names("All Games (EU)"),
            names("All Games (JP)"),
        );
        assert_eq!((na.len(), eu.len(), jp.len()), (30, 30, 30));
        assert!(na.contains(&"Super C (USA).zip"));
        assert!(eu.contains(&"Shadow Warriors - Ninja Gaiden (Europe).zip"));
        assert!(eu.contains(&"Punch-Out!! (Europe).zip"));
        assert!(jp.contains(&"Akumajou Dracula (Japan).zip"));
        assert!(jp.contains(&"Super Mario USA (Japan) (En).zip"));
        assert!(!jp.contains(&"Bubble Bobble (USA).zip"));
        for item in preset.top.iter().flat_map(|g| &g.items) {
            let file = item.file.as_ref().unwrap();
            let expected = if item.name.starts_with("Metroid (Japan)")
                || item.name.starts_with("Zelda no Densetsu")
                || item.name.starts_with("Link no Bouken")
            {
                "fds"
            } else {
                "nes"
            };
            assert_eq!(
                system_folder(&file.torrents),
                Some(expected),
                "{}",
                item.name
            );
        }
    }

    #[test]
    fn snes_classic_has_twenty_one_games_per_region_using_local_versions() {
        let presets = load_presets();
        let preset = presets
            .iter()
            .find(|p| p.name == "Super NES Classic Edition")
            .expect("preset exists");
        assert!(preset.groups.is_empty());
        let names = |list: &str| -> Vec<&str> {
            preset
                .top
                .iter()
                .find(|g| g.name == list)
                .unwrap_or_else(|| panic!("{list} exists"))
                .items
                .iter()
                .map(|item| item.name.as_str())
                .collect()
        };
        let (na, eu, jp) = (
            names("All Games (NA)"),
            names("All Games (EU)"),
            names("All Games (JP)"),
        );
        assert_eq!((na.len(), eu.len(), jp.len()), (21, 21, 21));
        assert!(na.contains(&"Final Fantasy III (USA).zip"));
        assert!(eu.contains(&"Super Probotector - Alien Rebels (Europe).zip"));
        assert!(eu.contains(&"Kirby's Fun Pak (Europe).zip"));
        assert!(jp.contains(&"Contra Spirits (Japan).zip"));
        assert!(jp.contains(&"Seiken Densetsu 2 (Japan).zip"));
        assert!(jp.contains(&"Fire Emblem - Monshou no Nazo (Japan).zip"));
        assert!(
            !na.contains(&"Panel de Pon (Japan).zip") && !eu.contains(&"EarthBound (Japan).zip")
        );
        for item in preset.top.iter().flat_map(|g| &g.items) {
            let file = item.file.as_ref().unwrap();
            assert_eq!(system_folder(&file.torrents), Some("snes"), "{}", item.name);
        }
    }

    fn mini_preset(name: &str) -> Preset {
        load_presets()
            .into_iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("{name} exists"))
    }

    fn list_names<'a>(preset: &'a Preset, list: &str) -> Vec<&'a str> {
        preset
            .top
            .iter()
            .chain(&preset.groups)
            .find(|g| g.name == list)
            .unwrap_or_else(|| panic!("{list} exists"))
            .items
            .iter()
            .map(|item| item.name.as_str())
            .collect()
    }

    #[test]
    fn genesis_mini_has_forty_two_games_per_region_using_the_shipped_roms() {
        let preset = mini_preset("Sega Genesis Mini");
        assert!(preset.groups.is_empty());
        let (na, eu, jp) = (
            list_names(&preset, "All Games (NA)"),
            list_names(&preset, "All Games (EU)"),
            list_names(&preset, "All Games (JP)"),
        );
        assert_eq!((na.len(), eu.len(), jp.len()), (42, 42, 42));
        assert!(na.contains(&"Contra - Hard Corps (USA) (Genesis Mini).zip"));
        assert!(eu.contains(&"Probotector (Europe) (En,Fr,De) (Mega Drive Mini).zip"));
        assert!(jp.contains(&"Contra - The Hard Corps (Japan) (Mega Drive Mini).zip"));
        assert!(eu.contains(&"Story of Thor, The (Europe) (Mega Drive Mini).zip"));
        assert!(jp.contains(&"Vampire Killer (Japan) (Mega Drive Mini).zip"));
        assert!(!na.contains(&"Alien Soldier (Japan).zip"), "Asia-only game");
        for item in preset.top.iter().flat_map(|g| &g.items) {
            let file = item.file.as_ref().unwrap();
            assert_eq!(
                system_folder(&file.torrents),
                Some("megadrive"),
                "{}",
                item.name
            );
        }
    }

    #[test]
    fn genesis_mini_2_splits_cartridge_and_sega_cd_games_with_every_disc() {
        let preset = mini_preset("Sega Genesis Mini 2");
        let (na, eu, jp) = (
            list_names(&preset, "All Games (NA)"),
            list_names(&preset, "All Games (EU)"),
            list_names(&preset, "All Games (JP)"),
        );
        assert_eq!((na.len(), eu.len(), jp.len()), (61, 61, 60));
        for disc in 1..=2 {
            assert!(na.contains(&format!("Night Trap (USA) (Disc {disc}).zip").as_str()));
            assert!(eu.contains(&format!("Night Trap (Europe) (Disc {disc}).zip").as_str()));
            assert!(jp.contains(&format!("Night Trap (Japan) (Disc {disc}).zip").as_str()));
        }
        assert!(jp.contains(&"Lunar - The Silver Star (Japan).zip"));
        assert!(!na.contains(&"Lunar - The Silver Star (Japan).zip"));
        assert!(eu.contains(&"Soleil (Europe).zip"));
        assert!(jp.contains(&"Shin Souseiki Ragnacenty (Japan).zip"));
        let sega_cd = list_names(&preset, "Mega CD (EU)");
        assert!(sega_cd.contains(&"Sonic CD (Europe).zip"));
        assert!(list_names(&preset, "Sega CD (NA)").contains(&"Sonic CD (USA).zip"));
        assert!(list_names(&preset, "Sega Genesis (NA)").contains(&"Streets of Rage 3 (USA).zip"));
        assert!(list_names(&preset, "Mega Drive (JP)").contains(&"Alien Soldier (Japan).zip"));
        for item in preset.top.iter().flat_map(|g| &g.items) {
            let file = item.file.as_ref().unwrap();
            let expected = if file.torrents.contains("Redump") {
                "megacd"
            } else {
                "megadrive"
            };
            assert_eq!(
                system_folder(&file.torrents),
                Some(expected),
                "{}",
                item.name
            );
        }
    }

    #[test]
    fn an_exported_queue_imports_as_a_preset_with_its_file_details() {
        let presets = load_presets();
        let with_details = presets[0].groups[0].items[0].clone();
        let searched = QueueItem {
            id: 77,
            name: "Searched (USA).zip".to_owned(),
            full_path: "Collection / Searched (USA).zip".to_owned(),
            file: None,
        };
        let queue = vec![with_details.clone(), searched];
        let json = serde_json::to_string(&queue_as_preset_file("My set", &queue)).unwrap();
        let file = parse_preset_file(&json, "fallback").unwrap();
        let preset = preset_from_file(file, None);
        assert_eq!(preset.name, "My set");
        assert!(preset.groups.is_empty());
        assert_eq!(preset.top.len(), 1);
        assert_eq!(preset.top[0].name, "All Games");
        let items = &preset.top[0].items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].file, with_details.file);
        assert_eq!(items[1].id, 77);
        assert!(items[1].file.is_none());
    }

    #[test]
    fn imported_presets_are_validated_and_can_have_regional_lists() {
        let parse = |json: &str| parse_preset_file(json, "from-file");
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"name":"x","items":[]}"#).is_err());
        assert!(parse(r#"{"name":"x","items":[{"id":0,"name":"a.zip"}]}"#).is_err());
        let too_long = "n".repeat(61);
        assert!(
            parse(&format!(
                r#"{{"name":"{too_long}","items":[{{"id":1,"name":"a"}}]}}"#
            ))
            .is_err()
        );
        let file = parse(
            r#"{"name":"  ","items":[
                {"id":1,"name":"a.zip","regions":["NA","XX","JP"]},
                {"id":1,"name":"duplicate.zip"},
                {"id":2,"name":"b.zip","regions":["JP"]},
                {"id":3,"name":"","regions":["NA"]}]}"#,
        )
        .unwrap();
        assert_eq!(file.name, "from-file");
        assert_eq!(file.items.len(), 2);
        assert_eq!(file.items[0].regions, ["NA", "JP"]);
        let preset = preset_from_file(file, None);
        let lists: Vec<_> = preset
            .top
            .iter()
            .map(|g| (g.name.as_str(), g.items.len()))
            .collect();
        assert_eq!(
            lists,
            [
                ("All Games", 2),
                ("All Games (NA)", 1),
                ("All Games (JP)", 2)
            ]
        );
        // Imported lists stay visible whichever regions are hidden.
        let none = PresetRegions {
            na: false,
            eu: false,
            jp: false,
        };
        assert!(none.shows(preset.top[0].region));
    }

    #[test]
    fn importing_a_file_returns_its_games_and_replaces_the_same_preset() {
        let dir = tempfile::tempdir().unwrap();
        let presets_dir = dir.path().join("presets");
        let source = dir.path().join("mine.json");
        fs::write(
            &source,
            r#"{"items":[{"id":5,"name":"a.zip"},{"id":6,"name":"b.zip"}]}"#,
        )
        .unwrap();
        let mut presets = load_presets();
        let built_in = presets.len();
        let (name, items) = import_preset_file(&source, &presets_dir, &mut presets).unwrap();
        assert_eq!(name, "mine");
        assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), [5, 6]);
        assert_eq!(presets.len(), built_in + 1);
        // Importing it again updates the preset instead of listing it twice.
        import_preset_file(&source, &presets_dir, &mut presets).unwrap();
        assert_eq!(presets.len(), built_in + 1);
        // A name that belongs to a built-in preset is refused.
        fs::write(
            &source,
            r#"{"name":"NES Classic Edition","items":[{"id":5,"name":"a.zip"}]}"#,
        )
        .unwrap();
        assert!(import_preset_file(&source, &presets_dir, &mut presets).is_err());
        assert_eq!(presets.len(), built_in + 1);
    }

    #[test]
    fn imported_presets_are_kept_between_runs() {
        let dir = tempfile::tempdir().unwrap();
        let presets_dir = dir.path().join("presets");
        assert!(load_custom_presets(&presets_dir).0.is_empty());
        let file = parse_preset_file(
            r#"{"name":"Best of: SNES!","note":"mine","items":[{"id":5,"name":"a.zip"}]}"#,
            "",
        )
        .unwrap();
        let path = save_custom_preset(&presets_dir, &file).unwrap();
        assert_eq!(path.file_name().unwrap(), "best-of-snes.json");
        fs::write(presets_dir.join("broken.json"), "nope").unwrap();
        let (loaded, errors) = load_custom_presets(&presets_dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "Best of: SNES!");
        assert_eq!(loaded[0].note, "mine");
        assert_eq!(loaded[0].custom_file.as_deref(), Some(path.as_path()));
        assert_eq!(errors.len(), 1);
        assert_eq!(preset_slug("///"), "preset");
    }

    #[test]
    fn tag_dropdown_expands_again_after_filtering() {
        let ctx = egui::Context::default();
        let options: Vec<_> = (0..100).map(|index| format!("System {index:03}")).collect();
        let mut selected = 0;
        let mut query = String::new();
        let mut render = |query: &mut String| {
            let mut popup_id = egui::Id::NULL;
            for _ in 0..4 {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1100.0, 760.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            popup_id = ui
                                .make_persistent_id(egui::Id::new("test-tags"))
                                .with("popup");
                            ui.memory_mut(|memory| memory.open_popup(popup_id));
                            combo(ui, "System", "test-tags", &options, &mut selected, query);
                        });
                    },
                );
            }
            ctx.memory(|memory| memory.area_rect(popup_id).unwrap())
        };
        let full = render(&mut query);
        query = "System 099".to_owned();
        let filtered = render(&mut query);
        query.clear();
        let restored = render(&mut query);
        assert!((filtered.height() - full.height()).abs() < 1.0);
        assert!(
            restored.height() >= full.height() - 1.0,
            "Dropdown did not expand: full={full:?}, restored={restored:?}"
        );
        assert!((restored.width() - full.width()).abs() < 1.0);
    }

    #[test]
    fn matches_tags_case_insensitively_and_prioritizes_prefixes() {
        let options = vec![
            "Super Nintendo".to_owned(),
            "Nintendo Entertainment System".to_owned(),
            "USA".to_owned(),
        ];
        assert_eq!(matching_filters(&options, " NINTENDO "), vec![1, 0]);
        assert_eq!(matching_filters(&options, "entertainment"), vec![1]);
        assert!(matching_filters(&options, "missing").is_empty());
        assert_eq!(matching_filters(&options, ""), vec![0, 1, 2]);
        assert_eq!(selected_filter(&options, 2), Some(&options[1]));
    }

    #[test]
    fn parses_archive_directory_and_breadcrumbs() {
        let html = r#"
            <div class="entry search_back"><a href="/browse/./">../</a></div>
            <div class="entry" data-name="systems">
              <a href="/browse/./No-Intro/Some%20System/"><strong>Some System/</strong></a>
              <span>120.83 MB</span>
            </div>
            <div class="entry" data-name="game.zip">
              <a href="/rom?id=42">game.zip</a><span>1.20 MB</span>
            </div>"#;
        let directory = parse_directory(html, "/browse/No-Intro/").unwrap();
        assert_eq!(directory.entries.len(), 2);
        assert_eq!(directory.entries[0].name, "Some System/");
        assert!(directory.entries[0].is_directory);
        assert_eq!(directory.entries[1].id, Some(42));
        assert_eq!(directory.parent.as_deref(), Some("/browse/"));
        assert_eq!(directory.breadcrumbs.last().unwrap().name, "No-Intro");
    }

    #[test]
    fn rejects_path_like_download_file_names() {
        assert_eq!(safe_file_name("folder/game.zip").unwrap(), "game.zip");
        assert!(safe_file_name("../").is_err());
    }

    #[test]
    fn parses_archive_search_records_and_rom_metadata() {
        let record: Rom = serde_json::from_str(
            r#"{"id":581,"full_path":"./Collection/Game.zip","magnet":"magnet:?xt=urn:btih:abc","so_id":7}"#,
        )
        .unwrap();
        assert_eq!(record.name(), "Game.zip");

        let details = parse_rom_details(
            r#"<script>window.rom = {"id":581,"file_name":"Game.zip","full_path":"./Collection/Game.zip","torrents":"Collection/data.torrent","magnet":"magnet:?xt=urn:btih:abc","so_id":7,"size":1234};</script>"#,
        )
        .unwrap();
        assert_eq!(details.id, record.id);
        assert_eq!(details.so_id, 7);
        assert_eq!(details.size, 1234);
        assert_eq!(details.torrents, "Collection/data.torrent");
    }

    #[tokio::test]
    #[ignore = "requires live MiNERVA access and BitTorrent peers"]
    async fn virtual_console_samples_download_into_their_folders() {
        let presets = load_presets();
        let preset = presets
            .iter()
            .find(|preset| preset.name == "Virtual Console")
            .unwrap();
        let mut items = Vec::new();
        let mut expected = Vec::new();
        for (group, folder) in [
            ("Neo Geo (JP)", "neogeo"),
            ("Commodore 64 (EU)", "c64"),
            ("TurboGrafx-16 / PC Engine (NA)", "pcengine"),
            ("Arcade (JP)", "fbneo"),
            ("Master System (EU)", "mastersystem"),
        ] {
            let smallest = preset
                .groups
                .iter()
                .find(|g| g.name == group)
                .unwrap()
                .items
                .iter()
                .filter(|item| {
                    item.file.as_ref().unwrap().folder.is_some() || !group.starts_with("Neo Geo")
                })
                .min_by_key(|item| item.file.as_ref().unwrap().size)
                .unwrap()
                .clone();
            let file = smallest.file.clone().unwrap();
            expected.push((folder, file.file_name, file.size));
            items.push(smallest);
        }
        let output = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel();
        download_queue(Client::new(), tx, items, output.path().to_owned(), true)
            .await
            .unwrap();
        for (folder, file_name, size) in expected {
            let path = output.path().join(folder).join(&file_name);
            assert_eq!(fs::metadata(&path).unwrap().len(), size, "{path:?}");
        }
    }

    #[tokio::test]
    #[ignore = "requires live MiNERVA access and BitTorrent peers"]
    async fn preset_games_download_without_any_rom_details_lookup() {
        let presets = load_presets();
        let nes = &presets[0].groups[0].items;
        let wanted = ["Balloon Fight (USA).zip", "Baseball (USA, Europe).zip"];
        let items: Vec<_> = nes
            .iter()
            .filter(|item| wanted.contains(&item.name.as_str()))
            .cloned()
            .collect();
        assert_eq!(items.len(), 2);
        let output = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        download_queue(
            Client::new(),
            tx,
            items.clone(),
            output.path().to_owned(),
            true,
        )
        .await
        .unwrap();
        for item in &items {
            let size = item.file.as_ref().unwrap().size;
            assert_eq!(
                fs::metadata(output.path().join("nes").join(&item.name))
                    .unwrap()
                    .len(),
                size
            );
        }
        let looked_up = rx.try_iter().any(|event| {
            matches!(event, UiEvent::DownloadStatus(status) if status.contains("ROM details"))
        });
        assert!(
            !looked_up,
            "preset downloads should not look up ROM details"
        );
    }

    #[tokio::test]
    #[ignore = "requires live MiNERVA access and BitTorrent peers"]
    async fn queue_continues_after_failures_and_skips_finished_files() {
        let client = Client::new();
        let output = tempfile::tempdir().unwrap();
        let item = |id, name: &str| QueueItem {
            id,
            name: name.to_owned(),
            full_path: String::new(),
            file: None,
        };
        let items = vec![
            // Out-of-date saved information must fall back to the live record.
            QueueItem {
                file: Some(ArchiveFile {
                    file_name: "Balloon Fight (USA).zip".to_owned(),
                    torrents: "Minerva_Myrient_v0.3/Missing.torrent".to_owned(),
                    so_id: 0,
                    size: 15_883,
                    folder: None,
                }),
                ..item(2_680_605, "Balloon Fight (USA).zip")
            },
            item(u64::MAX, "Missing record"),
            QueueItem {
                file: Some(ArchiveFile {
                    file_name: "Baseball (USA, Europe).zip".to_owned(),
                    torrents: "Minerva_Myrient_v0.3/Minerva_Myrient - No-Intro - Nintendo - Nintendo Entertainment System (Headered).torrent".to_owned(),
                    so_id: 688,
                    size: 17_046,
                    folder: None,
                }),
                ..item(2_680_404, "Baseball (USA, Europe).zip")
            },
        ];
        let (tx, rx) = mpsc::channel();
        let error = download_queue(
            client.clone(),
            tx,
            items.clone(),
            output.path().to_owned(),
            true,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("1 of 3 downloads failed"), "{error}");
        assert_eq!(
            fs::metadata(output.path().join("nes/Balloon Fight (USA).zip"))
                .unwrap()
                .len(),
            15_883
        );
        assert!(
            output
                .path()
                .join("nes/Baseball (USA, Europe).zip")
                .exists()
        );

        let (tx, rx2) = mpsc::channel();
        let _ = download_queue(client, tx, items, output.path().to_owned(), true).await;
        let statuses: Vec<_> = rx2
            .try_iter()
            .filter_map(|event| match event {
                UiEvent::DownloadStatus(status) => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses
                .iter()
                .filter(|status| status.contains("already downloaded"))
                .count(),
            2,
            "{statuses:?}"
        );
        drop(rx);
    }

    #[tokio::test]
    #[ignore = "requires live MiNERVA access and BitTorrent peers"]
    async fn downloads_known_archive_file() {
        let client = Client::new();
        let output = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel();
        download_queue(
            client,
            tx,
            vec![QueueItem {
                id: 581,
                name: "Super Mario Bros. (19xx)(-)[p].zip".to_owned(),
                full_path: "./TOSEC/Sharp/X68000/Game.zip".to_owned(),
                file: None,
            }],
            output.path().to_owned(),
            true,
        )
        .await
        .unwrap();
        let downloaded = output.path().join("Super Mario Bros. (19xx)(-)[p].zip");
        assert_eq!(fs::metadata(downloaded).unwrap().len(), 651_268);
    }
}
