//! Core logic ported from epic_relink.py
//!
//! Scans game folders (`.egstore` binary manifests + `.mancpn` JSON), rebuilds
//! the launcher's `.item` files, EOSH `.egi` records and `LauncherInstalled.dat`.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};

pub const DEFAULT_MANIFESTS_DIR: &str =
    r"C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests";
pub const DEFAULT_INSTALLED_DAT: &str =
    r"C:\ProgramData\Epic\UnrealEngineLauncher\LauncherInstalled.dat";
pub const DEFAULT_CATCACHE: &str =
    r"C:\ProgramData\Epic\EpicGamesLauncher\Data\Catalog\catcache.bin";
pub const DEFAULT_EGI_DIR: &str =
    r"C:\ProgramData\Epic\EpicOnlineServicesShared\InstallHelper\InstalledItems";
pub const DEFAULT_GAMES_DIR: &str = r"C:\Program Files\Epic Games";

const DM_MAGIC: i32 = 0x44BEC00C;
pub const DB_FIELDS: [&str; 6] = [
    "app_name",
    "version",
    "launch_exe",
    "catalog_ns",
    "catalog_item",
    "display_name",
];

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkStatus {
    Linked,
    NeedsRelink,
    NotLinked,
}

#[derive(Clone, Debug)]
pub struct GameInfo {
    pub guid: String,
    pub game_dir: PathBuf,
    pub manifest_hash: String,
    pub app_name: String,
    pub version: String,
    pub launch_exe: String,
    pub catalog_ns: String,
    pub catalog_item: String,
    pub display_name: String,
    pub install_size: u64,
    pub status: LinkStatus,
}

#[derive(Clone, Copy, Debug)]
pub enum DatEnc {
    Utf16Bom,
    Utf16Le,
    Ascii,
}

/// Paths to the various Epic launcher data locations.
#[derive(Clone, Debug)]
pub struct CtxPaths {
    pub manifests_dir: PathBuf,
    pub installed_dat: PathBuf,
    pub catcache: PathBuf,
    pub egi_dir: PathBuf,
    pub db_path: PathBuf,
}

impl Default for CtxPaths {
    fn default() -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_default();
        CtxPaths {
            manifests_dir: PathBuf::from(DEFAULT_MANIFESTS_DIR),
            installed_dat: PathBuf::from(DEFAULT_INSTALLED_DAT),
            catcache: PathBuf::from(DEFAULT_CATCACHE),
            egi_dir: PathBuf::from(DEFAULT_EGI_DIR),
            db_path: exe_dir.join("epic_relink_db.json"),
        }
    }
}

/// Everything needed to scan games and relink them.
pub struct ScanContext {
    pub paths: CtxPaths,
    pub titles: HashMap<(String, String), String>,
    pub db: HashMap<String, Value>,
    pub existing_by_guid: HashMap<String, Value>,
    pub existing_by_app: HashMap<String, Value>,
    pub dat_lookup: HashMap<String, (String, String)>,
    pub dat: Option<(Value, DatEnc)>,
}

impl ScanContext {
    pub fn load(paths: &CtxPaths) -> Self {
        let titles = load_catalog_titles(&paths.catcache);
        let db = load_db(&paths.db_path);
        let (existing_by_guid, existing_by_app) = load_existing_items(&paths.manifests_dir);
        let dat = load_installed_dat(&paths.installed_dat);
        let mut dat_lookup = HashMap::new();
        if let Some((d, _)) = &dat {
            if let Some(list) = d.get("InstallationList").and_then(|v| v.as_array()) {
                for e in list {
                    if let Some(artifact) = e.get("ArtifactId").and_then(|v| v.as_str()) {
                        dat_lookup.insert(
                            artifact.to_string(),
                            (
                                e.get("NamespaceId")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                e.get("ItemId")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                            ),
                        );
                    }
                }
            }
        }
        ScanContext {
            paths: paths.clone(),
            titles,
            db,
            existing_by_guid,
            existing_by_app,
            dat_lookup,
            dat,
        }
    }

    /// Re-reads the `.item` files after a relink operation.
    pub fn reload_items(&mut self) {
        let (g, a) = load_existing_items(&self.paths.manifests_dir);
        self.existing_by_guid = g;
        self.existing_by_app = a;
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn win(path: &str) -> String {
    path.replace('/', "\\")
}

fn dotnet_ticks_now() -> u64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // 62135596800 s between 0001-01-01 and 1970-01-01
    (dur.as_secs() + 62135596800) * 10_000_000 + (dur.subsec_nanos() as u64 / 100)
}

fn build_revision(timestamp: u64, number: u64) -> String {
    format!("{:016X}{:016X}", timestamp, number)
}

pub fn human_size(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.2} GB", bytes as f64 / (1 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
    } else if bytes >= 1 << 10 {
        format!("{:.0} KB", bytes as f64 / (1 << 10) as f64)
    } else {
        format!("{} B", bytes)
    }
}

// ---------------------------------------------------------------------------
// Local metadata DB (epic_relink_db.json) — additive, survives .mancpn loss
// ---------------------------------------------------------------------------

fn load_db(path: &Path) -> HashMap<String, Value> {
    if let Ok(raw) = fs::read_to_string(path) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(obj) = v.as_object() {
                return obj.clone().into_iter().collect();
            }
        }
    }
    HashMap::new()
}

pub fn save_db(ctx: &ScanContext) {
    let tmp = ctx.paths.db_path.with_extension("json.tmp");
    if let Ok(s) = serde_json::to_string_pretty(&ctx.db) {
        if fs::write(&tmp, s).is_ok() {
            let _ = fs::rename(&tmp, &ctx.paths.db_path);
        }
    }
}

// ---------------------------------------------------------------------------
// Binary Epic manifest parsing (.egstore/<GUID>.manifest)
// ---------------------------------------------------------------------------

fn sha1_file(path: &Path) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let mut h = Sha1::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(format!("{:x}", h.finalize()))
}

/// Extracts the meta strings [AppName, Version, LaunchExecutable, ...].
fn meta_strings(manifest_path: &Path) -> Vec<String> {
    let data = match fs::read(manifest_path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    if data.len() < 42 || i32::from_le_bytes([data[0], data[1], data[2], data[3]]) != DM_MAGIC {
        return vec![];
    }
    // offset 41 = start of the zlib stream
    let mut out = Vec::new();
    let mut decoder = flate2::read::ZlibDecoder::new(&data[41..]);
    if decoder.read_to_end(&mut out).is_err() {
        return vec![];
    }
    let mut strings = Vec::new();
    let mut off = 14usize; // meta u32 + flag u8 + comp u32 + 5 unknown bytes
    for _ in 0..8 {
        if off + 4 > out.len() {
            break;
        }
        let n = u32::from_le_bytes([out[off], out[off + 1], out[off + 2], out[off + 3]]) as usize;
        if n == 0 || n > 512 || off + 4 + n > out.len() {
            break;
        }
        let raw = &out[off + 4..off + 4 + n - 1]; // n includes the NUL
        if raw.is_empty() || raw.iter().any(|&c| !(0x20..=0x7E).contains(&c)) {
            break;
        }
        strings.push(String::from_utf8_lossy(raw).into_owned());
        off += 4 + n;
    }
    strings
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.to_string_lossy().to_lowercase().contains(".egstore") {
            continue;
        }
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                if let Ok(ft) = e.file_type() {
                    if ft.is_dir() {
                        if !ft.is_symlink() {
                            stack.push(e.path());
                        }
                    } else if let Ok(md) = e.metadata() {
                        total += md.len();
                    }
                }
            }
        }
    }
    total
}

/// Collects everything recoverable from a game folder (raw scan, before the
/// metadata merges done in `finalize_game`).
pub fn scan_game(game_dir: &Path) -> Option<GameInfo> {
    let egstore = game_dir.join(".egstore");
    if !egstore.is_dir() {
        return None;
    }
    let entries: Vec<_> = fs::read_dir(&egstore).ok()?.flatten().collect();
    let manifest = entries.iter().find(|e| {
        let name = e.file_name().to_string_lossy().to_lowercase();
        name.ends_with(".manifest") && name.len() >= 40
    })?;
    let mancpn = entries.iter().find(|e| {
        e.file_name()
            .to_string_lossy()
            .to_lowercase()
            .ends_with(".mancpn")
    });

    let guid = manifest
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".manifest")
        .trim_end_matches(".MANIFEST")
        .to_uppercase();
    let manifest_hash = sha1_file(&manifest.path()).unwrap_or_default();

    let mut info = GameInfo {
        guid,
        game_dir: game_dir.to_path_buf(),
        manifest_hash,
        app_name: String::new(),
        version: String::new(),
        launch_exe: String::new(),
        catalog_ns: String::new(),
        catalog_item: String::new(),
        display_name: String::new(),
        install_size: dir_size(game_dir),
        status: LinkStatus::NotLinked,
    };

    let strs = meta_strings(&manifest.path());
    if strs.len() >= 3 {
        info.app_name = strs[0].clone();
        info.version = strs[1].clone();
        info.launch_exe = strs[2].clone();
    }

    if let Some(mc_path) = mancpn {
        if let Ok(raw) = fs::read_to_string(mc_path.path()) {
            if let Ok(mc) = serde_json::from_str::<Value>(&raw) {
                if let Some(ns) = mc.get("CatalogNamespace").and_then(|v| v.as_str()) {
                    info.catalog_ns = ns.to_string();
                }
                if let Some(item) = mc.get("CatalogItemId").and_then(|v| v.as_str()) {
                    info.catalog_item = item.to_string();
                }
                if let Some(app) = mc.get("AppName").and_then(|v| v.as_str()) {
                    if !app.is_empty() {
                        info.app_name = app.to_string();
                    }
                }
            }
        }
    }
    Some(info)
}

// ---------------------------------------------------------------------------
// Metadata completion + link status
// ---------------------------------------------------------------------------

fn load_catalog_titles(catcache: &Path) -> HashMap<(String, String), String> {
    let mut out = HashMap::new();
    let raw = match fs::read(catcache) {
        Ok(r) => r,
        Err(_) => return out,
    };
    let decoded = base64::engine::general_purpose::STANDARD.decode(raw).ok();
    if let Some(bytes) = decoded {
        if let Ok(Value::Array(entries)) = serde_json::from_slice::<Value>(&bytes) {
            for e in entries {
                if let Some(obj) = e.as_object() {
                    let key = (
                        obj.get("namespace").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        obj.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    );
                    let title =
                        obj.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    out.insert(key, title);
                }
            }
        }
    }
    out
}

fn guess_display_name(info: &GameInfo, game_dir: &Path) -> String {
    if !info.launch_exe.is_empty() {
        let lower = info.launch_exe.to_lowercase();
        let name = lower.strip_suffix(".exe").unwrap_or(&lower);
        let bad = ["unins", "setup", "unitycrashhandler", "e0sbootstrapper", "eosbootstrapper"];
        if !name.is_empty() && !bad.iter().any(|p| name.starts_with(p)) {
            return name.to_string();
        }
    }
    game_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Completes missing metadata from the local DB / LauncherInstalled.dat and
/// computes the link status. Also merges the info into the DB (additive).
pub fn finalize_game(ctx: &mut ScanContext, info: &mut GameInfo) {
    if let Some(known) = ctx.db.get(&info.guid) {
        for k in DB_FIELDS {
            let cur = match k {
                "app_name" => &info.app_name,
                "version" => &info.version,
                "launch_exe" => &info.launch_exe,
                "catalog_ns" => &info.catalog_ns,
                "catalog_item" => &info.catalog_item,
                "display_name" => &info.display_name,
                _ => "",
            };
            if cur.is_empty() {
                if let Some(v) = known.get(k).and_then(|v| v.as_str()) {
                    match k {
                        "app_name" => info.app_name = v.to_string(),
                        "version" => info.version = v.to_string(),
                        "launch_exe" => info.launch_exe = v.to_string(),
                        "catalog_ns" => info.catalog_ns = v.to_string(),
                        "catalog_item" => info.catalog_item = v.to_string(),
                        "display_name" => info.display_name = v.to_string(),
                        _ => {}
                    }
                }
            }
        }
    }
    if info.catalog_item.is_empty() && !info.app_name.is_empty() {
        if let Some((ns, item)) = ctx.dat_lookup.get(&info.app_name) {
            if info.catalog_ns.is_empty() {
                info.catalog_ns = ns.clone();
            }
            if info.catalog_item.is_empty() {
                info.catalog_item = item.clone();
            }
        }
    }

    let title = ctx
        .titles
        .get(&(info.catalog_ns.clone(), info.catalog_item.clone()))
        .cloned()
        .unwrap_or_default();
    if !title.is_empty() {
        info.display_name = title;
    } else if info.display_name.is_empty() {
        info.display_name = guess_display_name(info, &info.game_dir);
    }

    // additive merge into the local DB
    let entry = ctx.db.entry(info.guid.clone()).or_insert_with(|| json!({}));
    if let Some(obj) = entry.as_object_mut() {
        for (k, v) in [
            ("app_name", &info.app_name),
            ("version", &info.version),
            ("launch_exe", &info.launch_exe),
            ("catalog_ns", &info.catalog_ns),
            ("catalog_item", &info.catalog_item),
            ("display_name", &info.display_name),
        ] {
            if !v.is_empty() {
                obj.insert(k.to_string(), json!(v));
            }
        }
    }

    info.status = compute_status(ctx, info);
}

pub fn compute_status(ctx: &ScanContext, info: &GameInfo) -> LinkStatus {
    let item = ctx
        .existing_by_guid
        .get(&info.guid)
        .or_else(|| ctx.existing_by_app.get(&info.app_name));
    match item {
        None => LinkStatus::NotLinked,
        Some(it) => {
            if item_needs_update(it, info) {
                LinkStatus::NeedsRelink
            } else {
                LinkStatus::Linked
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Existing .item files
// ---------------------------------------------------------------------------

fn load_existing_items(
    manifests_dir: &Path,
) -> (HashMap<String, Value>, HashMap<String, Value>) {
    let mut by_guid = HashMap::new();
    let mut by_app = HashMap::new();
    if let Ok(rd) = fs::read_dir(manifests_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.to_lowercase().ends_with(".item") {
                continue;
            }
            if let Ok(raw) = fs::read_to_string(e.path()) {
                if let Ok(it) = serde_json::from_str::<Value>(&raw) {
                    by_guid.insert(
                        name[..name.len() - 5].to_uppercase(),
                        it.clone(),
                    );
                    if let Some(app) = it.get("AppName").and_then(|v| v.as_str()) {
                        by_app.insert(app.to_string(), it);
                    }
                }
            }
        }
    }
    (by_guid, by_app)
}

fn item_needs_update(item: &Value, info: &GameInfo) -> bool {
    let loc = win(&info.game_dir.to_string_lossy());
    let checks = [
        item.get("InstallLocation").and_then(|v| v.as_str()) == Some(loc.as_str()),
        item.get("ManifestHash").and_then(|v| v.as_str())
            == Some(info.manifest_hash.as_str()),
        item.get("AppVersionString").and_then(|v| v.as_str())
            == Some(info.version.as_str()),
        item.get("InstallSize").and_then(|v| v.as_u64()) == Some(info.install_size),
        item.get("CatalogItemId").and_then(|v| v.as_str())
            == Some(info.catalog_item.as_str()),
        item.get("CatalogNamespace").and_then(|v| v.as_str())
            == Some(info.catalog_ns.as_str()),
        item.get("LaunchExecutable").and_then(|v| v.as_str())
            == Some(info.launch_exe.as_str()),
        // an .item emptied by EOSH reconciliation has these fields blank:
        !info.catalog_item.is_empty(),
        !info.launch_exe.is_empty(),
    ];
    !checks.iter().all(|c| *c)
}

// ---------------------------------------------------------------------------
// .item generation
// ---------------------------------------------------------------------------

fn build_item(info: &GameInfo, existing: Option<&Value>) -> Value {
    let base: Map<String, Value> = match existing {
        Some(Value::Object(m)) => m.clone(),
        _ => serde_json::from_str::<Value>(DEFAULT_ITEM_TEMPLATE)
            .unwrap()
            .as_object()
            .unwrap()
            .clone(),
    };
    let mut item = Value::Object(base);
    let egstore = win(&info.game_dir.join(".egstore").to_string_lossy());
    let loc = win(&info.game_dir.to_string_lossy());
    let dir_name = info
        .game_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let updates = json!({
        "LaunchExecutable": info.launch_exe,
        "ManifestLocation": egstore,
        "CompleteManifestPath": format!("{}\\{}.manifest", egstore, info.guid),
        "PendingManifestPath": format!("{}\\Pending\\{}.manifest", egstore, info.guid),
        "ManifestHash": info.manifest_hash,
        "DisplayName": info.display_name,
        "InstallLocation": loc,
        "StagingLocation": format!("{}/bps", egstore.replace('\\', "/")),
        "InstallSize": info.install_size,
        "MandatoryAppFolderName": dir_name,
        "CatalogNamespace": info.catalog_ns,
        "CatalogItemId": info.catalog_item,
        "AppName": info.app_name,
        "AppVersionString": info.version,
    });
    if let (Some(target), Some(src)) = (item.as_object_mut(), updates.as_object()) {
        for (k, v) in src {
            target.insert(k.clone(), v.clone());
        }
    }
    item
}

const DEFAULT_ITEM_TEMPLATE: &str = r#"{
    "FormatVersion": 0,
    "EoshRevision": "00000000000000000000000000000000",
    "bIsIncompleteInstall": false,
    "LaunchCommand": "",
    "LaunchExecutable": "",
    "ManifestLocation": "",
    "CompleteManifestPath": "",
    "PendingManifestPath": "",
    "ManifestHash": "",
    "SDMetaHash": "",
    "SDMetaLocation": "",
    "bIsApplication": true,
    "bIsExecutable": true,
    "bIsManaged": false,
    "bIsFab": false,
    "bNeedsValidation": false,
    "bSDMetaMigrated": false,
    "bRequiresAuth": true,
    "bAllowMultipleInstances": false,
    "bCanRunOffline": false,
    "bAllowUriCmdArgs": false,
    "bLaunchElevated": false,
    "BaseURLs": [],
    "BuildLabel": "Live",
    "AppCategories": ["games", "applications"],
    "ChunkDbs": [],
    "CompatibleApps": [],
    "DisplayName": "",
    "InstallationGuid": "",
    "InstallLocation": "",
    "InstallSessionId": "00000000000000000000000000000000",
    "InstallTags": [],
    "InstallComponents": [],
    "HostInstallationGuid": "00000000000000000000000000000000",
    "PrereqSHA1Hash": "",
    "LastPrereqSucceededSHA1Hash": "",
    "StagingLocation": "",
    "TechnicalType": "games,applications",
    "VaultThumbnailUrl": "",
    "VaultTitleText": "",
    "InstallSize": 0,
    "MainWindowProcessName": "",
    "ProcessNames": [],
    "BackgroundProcessNames": [],
    "IgnoredProcessNames": [],
    "DlcProcessNames": [],
    "MandatoryAppFolderName": "",
    "OwnershipToken": "false",
    "SidecarConfigRevision": 1,
    "SidecarDeploymentId": "",
    "PreloadState": 0,
    "CatalogNamespace": "",
    "CatalogItemId": "",
    "AppName": "",
    "AppVersionString": "",
    "MainGameCatalogNamespace": "",
    "MainGameCatalogItemId": "",
    "MainGameAppName": "",
    "AllowedUriEnvVars": []
}"#;

/// Pretty-prints JSON with tab indentation and CRLF line endings, like the
/// launcher does.
pub fn format_json_tabs(value: &Value) -> String {
    fn fmt(v: &Value, depth: usize, out: &mut String) {
        let indent = "\t".repeat(depth);
        match v {
            Value::Object(m) => {
                if m.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\r\n");
                let n = m.len();
                for (i, (k, val)) in m.iter().enumerate() {
                    out.push_str(&indent);
                    out.push('\t');
                    out.push_str(&serde_json::to_string(k).unwrap());
                    out.push_str(": ");
                    fmt(val, depth + 1, out);
                    if i + 1 < n {
                        out.push(',');
                    }
                    out.push_str("\r\n");
                }
                out.push_str(&indent);
                out.push('}');
            }
            Value::Array(a) => {
                if a.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\r\n");
                let n = a.len();
                for (i, val) in a.iter().enumerate() {
                    out.push_str(&indent);
                    out.push('\t');
                    fmt(val, depth + 1, out);
                    if i + 1 < n {
                        out.push(',');
                    }
                    out.push_str("\r\n");
                }
                out.push_str(&indent);
                out.push(']');
            }
            other => out.push_str(&serde_json::to_string(other).unwrap()),
        }
    }
    let mut s = String::new();
    fmt(value, 0, &mut s);
    s
}

// ---------------------------------------------------------------------------
// LauncherInstalled.dat (UTF-16LE / ASCII, JSON)
// ---------------------------------------------------------------------------

pub fn load_installed_dat(path: &Path) -> Option<(Value, DatEnc)> {
    let raw = fs::read(path).ok()?;
    let (text, enc) = if raw.starts_with(&[0xFF, 0xFE]) || raw.starts_with(&[0xFE, 0xFF]) {
        let u16s: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        (String::from_utf16_lossy(&u16s), DatEnc::Utf16Bom)
    } else {
        let zeros_head = raw.get(..4).map(|s| s.iter().filter(|&&b| b == 0).count()).unwrap_or(0);
        let zeros_mid = raw
            .get(200..300)
            .map(|s| s.iter().filter(|&&b| b == 0).count())
            .unwrap_or(0);
        if zeros_head >= 2 || zeros_mid > 10 {
            let u16s: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            (String::from_utf16_lossy(&u16s), DatEnc::Utf16Le)
        } else {
            (String::from_utf8_lossy(&raw).into_owned(), DatEnc::Ascii)
        }
    };
    serde_json::from_str(&text).ok().map(|v| (v, enc))
}

fn save_installed_dat(path: &Path, data: &Value, enc: DatEnc) {
    let json = serde_json::to_string(data).unwrap_or_else(|_| "{}".into());
    let bytes = match enc {
        DatEnc::Utf16Bom => {
            let mut b = vec![0xFF, 0xFE];
            for u in json.encode_utf16() {
                b.extend_from_slice(&u.to_le_bytes());
            }
            b
        }
        DatEnc::Utf16Le => {
            let mut b = Vec::new();
            for u in json.encode_utf16() {
                b.extend_from_slice(&u.to_le_bytes());
            }
            b
        }
        DatEnc::Ascii => json.into_bytes(),
    };
    let _ = fs::write(path, bytes);
}

// ---------------------------------------------------------------------------
// EOSH (.egi) records + revision
// ---------------------------------------------------------------------------

fn read_revision(egi_dir: &Path) -> (u64, u64) {
    let path = egi_dir.join("Revision.json");
    if let Ok(raw) = fs::read_to_string(&path) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            let ts = v.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
            let num = v.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
            return (ts, num);
        }
    }
    (0, 0)
}

fn bump_revision(egi_dir: &Path) -> String {
    let (ts, num) = read_revision(egi_dir);
    let mut new_ts = dotnet_ticks_now();
    if new_ts <= ts {
        new_ts = ts + 1; // the revision must always increase
    }
    let new_num = num + 1;
    let path = egi_dir.join("Revision.json");
    if path.is_file() {
        let _ = fs::copy(&path, path.with_extension("json.bak"));
    }
    let _ = fs::create_dir_all(egi_dir);
    let _ = fs::write(
        &path,
        format!(
            "{{\"timestamp\":{},\"number\":{}}}",
            new_ts, new_num
        ),
    );
    build_revision(new_ts, new_num)
}

fn build_egi(info: &GameInfo, revision: &str) -> Value {
    let loc = win(&info.game_dir.to_string_lossy());
    let empty_data = json!({
        "version": "",
        "launchCommand": "",
        "launchExe": "",
        "prereqSHA1Hash": "",
        "buildSize": 0,
    });
    json!({
        "v4": {
            "installationId": info.guid,
            "state": "Installed",
            "revision": revision,
            "dir": loc,
            "metaDir": format!("{}\\.egstore", loc),
            "manifestPath": format!("{}\\.egstore\\{}.manifest", loc, info.guid),
            "pendingManifestPath": format!("{}\\.egstore\\Pending\\{}.manifest", loc, info.guid),
            "platform": "Windows",
            "sandboxId": info.catalog_ns,
            "itemId": info.catalog_item,
            "artifactId": info.app_name,
            "tags": [],
            "pendingTags": [],
            "stagedVersion": "",
            "manifestData": {
                "version": info.version,
                "launchCommand": "",
                "launchExe": info.launch_exe,
                "prereqSHA1Hash": "",
                "buildSize": info.install_size,
            },
            "pendingManifestData": empty_data,
        }
    })
}

/// Writes the .egi record if missing or different. Returns true if changed.
fn write_egi(egi_dir: &Path, info: &GameInfo, revision: &str) -> bool {
    let path = egi_dir.join(format!("{}.egi", info.guid));
    let content = serde_json::to_string(&build_egi(info, revision)).unwrap_or_default();
    if let Ok(existing) = fs::read_to_string(&path) {
        if existing == content {
            return false;
        }
        let _ = fs::copy(&path, path.with_extension("egi.bak"));
    }
    let _ = fs::create_dir_all(egi_dir);
    fs::write(&path, content).is_ok()
}

fn egi_content(info: &GameInfo, revision: &str) -> String {
    serde_json::to_string(&build_egi(info, revision)).unwrap_or_default()
}

/// Restores the game's .mancpn (removed by the launcher on EOSH rejection).
fn write_mancpn(info: &GameInfo) -> bool {
    if info.catalog_ns.is_empty() || info.catalog_item.is_empty() || info.app_name.is_empty() {
        return false;
    }
    let path = info
        .game_dir
        .join(".egstore")
        .join(format!("{}.mancpn", info.guid));
    let content = format!(
        "{{\"FormatVersion\":0,\"CatalogNamespace\":{},\"CatalogItemId\":{},\"AppName\":{}}}",
        serde_json::to_string(&info.catalog_ns).unwrap(),
        serde_json::to_string(&info.catalog_item).unwrap(),
        serde_json::to_string(&info.app_name).unwrap(),
    );
    if let Ok(existing) = fs::read_to_string(&path) {
        if existing.trim() == content.trim() {
            return false;
        }
        let _ = fs::copy(&path, path.with_extension("mancpn.bak"));
    }
    fs::write(&path, content).is_ok()
}

// ---------------------------------------------------------------------------
// LauncherInstalled.dat upsert
// ---------------------------------------------------------------------------

fn upsert_installation(dat: &mut Value, info: &GameInfo) -> bool {
    let loc = win(&info.game_dir.to_string_lossy());
    let list = dat
        .as_object_mut()
        .and_then(|m| m.get_mut("InstallationList"))
        .and_then(|v| v.as_array_mut());
    let Some(entries) = list else {
        return false;
    };

    let merge = |e: &mut Map<String, Value>| -> bool {
        let mut changed = false;
        for (key, val) in [
            ("InstallLocation", loc.clone()),
            ("NamespaceId", info.catalog_ns.clone()),
            ("ItemId", info.catalog_item.clone()),
            ("ArtifactId", info.app_name.clone()),
            ("AppVersion", info.version.clone()),
            ("AppName", info.app_name.clone()),
        ] {
            if !val.is_empty() && e.get(key).and_then(|v| v.as_str()) != Some(val.as_str()) {
                e.insert(key.to_string(), json!(val));
                changed = true;
            }
        }
        changed
    };

    for e in entries.iter_mut() {
        if let Some(obj) = e.as_object_mut() {
            let matches = obj.get("AppName").and_then(|v| v.as_str())
                == Some(info.app_name.as_str())
                || obj.get("ArtifactId").and_then(|v| v.as_str())
                    == Some(info.app_name.as_str());
            if matches {
                return merge(obj);
            }
        }
    }
    entries.push(json!({
        "InstallLocation": loc,
        "NamespaceId": info.catalog_ns,
        "ItemId": info.catalog_item,
        "ArtifactId": info.app_name,
        "AppVersion": info.version,
        "AppName": info.app_name,
    }));
    true
}

// ---------------------------------------------------------------------------
// Relink
// ---------------------------------------------------------------------------

/// Relinks the given games: .item + .egi + .mancpn + LauncherInstalled.dat.
/// Returns log lines describing what was done.
pub fn relink(ctx: &mut ScanContext, games: &[&GameInfo]) -> Vec<String> {
    let mut logs = Vec::new();
    if games.is_empty() {
        return logs;
    }

    // Phase 1: EOSH — single revision for the whole batch
    let (ts, num) = read_revision(&ctx.paths.egi_dir);
    let cur_revision = build_revision(ts, num);
    let egi_would_change = games.iter().any(|g| {
        let path = ctx.paths.egi_dir.join(format!("{}.egi", g.guid));
        match fs::read_to_string(&path) {
            Ok(existing) => existing != egi_content(g, &cur_revision),
            Err(_) => true,
        }
    });
    let revision = if egi_would_change {
        let rev = bump_revision(&ctx.paths.egi_dir);
        logs.push(format!("EOSH revision: {} -> {}", cur_revision, rev));
        rev
    } else {
        cur_revision
    };

    let mut dat_changed = false;

    for info in games {
        let item_path = ctx
            .paths
            .manifests_dir
            .join(format!("{}.item", info.guid));
        let existing = ctx
            .existing_by_guid
            .get(&info.guid)
            .or_else(|| ctx.existing_by_app.get(&info.app_name));

        let mut item = build_item(info, existing);
        if let Some(obj) = item.as_object_mut() {
            obj.insert("EoshRevision".into(), json!(revision));
        }

        let stale = existing
            .map(|it| item_needs_update(it, info))
            .unwrap_or(true);
        let new_file = existing.is_none();

        let res = (|| -> std::io::Result<()> {
            if stale {
                if item_path.exists() {
                    fs::copy(&item_path, item_path.with_extension("item.bak"))?;
                }
                let _ = fs::create_dir_all(&ctx.paths.manifests_dir);
                fs::write(&item_path, format_json_tabs(&item))?;
            }
            if write_egi(&ctx.paths.egi_dir, info, &revision) {
                logs.push(format!("  .egi written for {}", info.display_name));
            }
            if write_mancpn(info) {
                logs.push(format!("  .mancpn restored for {}", info.display_name));
            }
            Ok(())
        })();

        match res {
            Ok(()) => {
                if stale {
                    logs.push(format!(
                        "{} {}",
                        if new_file { "Created" } else { "Relinked" },
                        info.display_name
                    ));
                } else {
                    logs.push(format!("Up to date: {}", info.display_name));
                }
            }
            Err(e) => logs.push(format!("FAILED {} ({})", info.display_name, e)),
        }

        if let Some((dat, _)) = &mut ctx.dat {
            if upsert_installation(dat, info) {
                dat_changed = true;
            }
        }
    }

    if dat_changed {
        if let Some((dat, enc)) = &ctx.dat {
            if let Some(parent) = ctx.paths.installed_dat.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::copy(
                &ctx.paths.installed_dat,
                ctx.paths.installed_dat.with_extension("dat.bak"),
            )
            .ok();
            save_installed_dat(&ctx.paths.installed_dat, dat, *enc);
            logs.push("LauncherInstalled.dat updated".to_string());
        }
    }

    logs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn scan_real_game_folder() {
        let base = Path::new(DEFAULT_GAMES_DIR);
        if !base.is_dir() {
            eprintln!("skip: no Epic games folder on this machine");
            return;
        }
        let mut found = 0;
        for e in std::fs::read_dir(base).unwrap().flatten() {
            if let Some(info) = scan_game(&e.path()) {
                found += 1;
                println!(
                    "{} | guid={} | app={} | version={} | exe={} | size={}",
                    info.display_name, info.guid, info.app_name,
                    info.version, info.launch_exe, info.install_size
                );
                assert!(!info.guid.is_empty());
                assert!(!info.manifest_hash.is_empty());
            }
        }
        assert!(found > 0, "no game with .egstore found");
    }

    #[test]
    fn installed_dat_roundtrip() {
        let p = Path::new(DEFAULT_INSTALLED_DAT);
        if !p.is_file() {
            eprintln!("skip: LauncherInstalled.dat not found");
            return;
        }
        let (v, enc) = load_installed_dat(p).expect("should parse");
        assert!(v.get("InstallationList").is_some());
        println!("encoding: {:?}", enc);
    }
}
