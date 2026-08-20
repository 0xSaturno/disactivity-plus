use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use tauri::Manager;

const DISCORD_GAMES_API_URL: &str = "https://discord.com/api/v9/games/detectable";
const DISCORD_NON_GAMES_API_URL: &str = "https://discord.com/api/v9/applications/non-games/detectable";
const STEAMCMD_INFO_API_URL: &str = "https://api.steamcmd.net/v1/info";

const CACHE_FILE_NAME: &str = "disactivity_games_cache.json";
const FAVORITES_FILE_NAME: &str = "disactivity_favorites.json";
const CACHE_EXPIRY_DAYS: i64 = 2;

// Embedded slave executable bytes (built in release mode)
const SLAVE_EXE: &[u8] = include_bytes!("../slave/target/release/slave.exe");

/// Where a running game's fake executable was placed, and how to clean it up
enum CleanupTarget {
    /// Normal flow: everything lives under the OS temp dir
    TempDir(PathBuf),
    /// Steam-fallback flow: a fake install + appmanifest were written into a real Steam library
    SteamFake {
        manifest_path: PathBuf,
        install_dir: PathBuf,
    },
}

/// Tracks a running game process
struct RunningGame {
    process: Child,
    cleanup: CleanupTarget,
}

/// State to track all running game processes
struct AppState {
    running_games: Mutex<HashMap<String, RunningGame>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Executable {
    pub name: String,
    #[serde(default)]
    pub os: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ThirdPartySku {
    pub distributor: String,
    // Discord sometimes reports this as null (e.g. some battlenet entries)
    pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Game {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon_hash: Option<String>,
    #[serde(default)]
    pub executables: Option<Vec<Executable>>,
    pub aliases: Vec<String>,
    #[serde(default)]
    pub third_party_skus: Option<Vec<ThirdPartySku>>,
}

/// The Steam appid for a game, if Discord's data lists one, regardless of
/// whether Discord also knows a Windows executable for it.
fn steam_app_id(game: &Game) -> Option<&str> {
    game.third_party_skus
        .as_ref()?
        .iter()
        .find(|sku| sku.distributor == "steam")?
        .id
        .as_deref()
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheData {
    timestamp: DateTime<Utc>,
    games: Vec<Game>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FetchGamesResponse {
    pub games: Vec<Game>,
    pub from_cache: bool,
}

fn get_cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|p| p.join(CACHE_FILE_NAME))
}

fn read_cache() -> Option<CacheData> {
    let cache_path = get_cache_path()?;
    let content = fs::read_to_string(&cache_path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_cache(games: &[Game]) -> Result<(), String> {
    let cache_path = get_cache_path().ok_or("Could not determine cache directory")?;
    let cache_data = CacheData {
        timestamp: Utc::now(),
        games: games.to_vec(),
    };
    let content = serde_json::to_string(&cache_data).map_err(|e| e.to_string())?;
    fs::write(&cache_path, content).map_err(|e| e.to_string())?;
    Ok(())
}

fn is_cache_valid(cache: &CacheData) -> bool {
    let now = Utc::now();
    let cache_age = now.signed_duration_since(cache.timestamp);
    cache_age.num_days() < CACHE_EXPIRY_DAYS
}

// Favorites functions
fn get_favorites_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("disactivity").join(FAVORITES_FILE_NAME))
}

fn read_favorites() -> HashSet<String> {
    let Some(path) = get_favorites_path() else {
        return HashSet::new();
    };

    let Ok(content) = fs::read_to_string(&path) else {
        return HashSet::new();
    };

    serde_json::from_str(&content).unwrap_or_default()
}

fn write_favorites(favorites: &HashSet<String>) -> Result<(), String> {
    let path = get_favorites_path().ok_or("Could not determine config directory")?;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let content = serde_json::to_string(favorites).map_err(|e| e.to_string())?;
    fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(())
}

async fn fetch_from_api() -> Result<Vec<Game>, String> {
    let client = reqwest::Client::new();
    let response_games = client
        .get(DISCORD_GAMES_API_URL)
        .header("User-Agent", "Disactivity/0.1.0")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch games: {}", e))?;

    if !response_games.status().is_success() {
        return Err(format!("API returned status: {}", response_games.status()));
    }

    let response_non_games = client
        .get(DISCORD_NON_GAMES_API_URL)
        .header("User-Agent", "Disactivity/0.1.0")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch games: {}", e))?;

    let mut games: Vec<Game> = response_games
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    if response_non_games.status().is_success() {
        let non_games: Vec<Game> = response_non_games
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))?;
        games.extend(non_games);
    }

    Ok(games.iter().filter(|game| {
        let has_windows_executable = game
            .executables
            .as_ref()
            .map_or(false, |execs| !execs.is_empty());
        // Keep games with no known executable if Discord still lists a Steam
        // SKU for them - the Steam-fallback flow can derive the executable itself.
        has_windows_executable || steam_app_id(game).is_some()
    }).cloned().collect())
}

#[tauri::command]
async fn fetch_games(force_refresh: bool) -> Result<FetchGamesResponse, String> {
    // Check cache first if not forcing refresh
    if !force_refresh {
        if let Some(cache) = read_cache() {
            if is_cache_valid(&cache) {
                return Ok(FetchGamesResponse {
                    games: cache.games,
                    from_cache: true,
                });
            }
        }
    }

    // Fetch from API
    let games = fetch_from_api().await?;

    // Write to cache
    if let Err(e) = write_cache(&games) {
        eprintln!("Warning: Failed to write cache: {}", e);
    }

    Ok(FetchGamesResponse {
        games,
        from_cache: false,
    })
}

#[tauri::command]
fn get_cache_info() -> Option<String> {
    let cache = read_cache()?;
    Some(cache.timestamp.to_rfc3339())
}

/// Select the best executable for win32 platform
/// Filters by os == "win32", excludes paths starting with ">", and picks shortest path
fn select_best_executable(executables: &[Executable]) -> Option<String> {
    executables
        .iter()
        .filter(|exe| {
            // Must be win32
            exe.os.as_deref() == Some("win32")
            // Must not start with ">" (which indicates "starts with" pattern)
            && !exe.name.starts_with('>')
        })
        .min_by_key(|exe| {
            // Pick the one with fewest path separators, then shortest length
            let separators = exe.name.matches('/').count() + exe.name.matches('\\').count();
            (separators, exe.name.len())
        })
        .map(|exe| exe.name.clone())
}

/// Create the directory structure and place the slave executable
fn setup_game_executable(game_id: &str, exe_path: &str) -> Result<(PathBuf, PathBuf), String> {
    // Get system temp directory
    let temp_base = std::env::temp_dir().join("disactivity").join(game_id);

    // Parse the executable path and create directory structure
    // exe_path might be something like "path/to/game.exe" or just "game.exe"
    let exe_path_normalized = exe_path.replace('\\', "/");
    let path_parts: Vec<&str> = exe_path_normalized.split('/').collect();

    // Create the full path including directories
    let mut full_dir = temp_base.clone();
    for part in &path_parts[..path_parts.len().saturating_sub(1)] {
        if !part.is_empty() {
            full_dir = full_dir.join(part);
        }
    }

    // Create all directories
    fs::create_dir_all(&full_dir).map_err(|e| format!("Failed to create directories: {}", e))?;

    // Get the executable filename
    let exe_filename = path_parts.last().ok_or("Invalid executable path")?;
    let final_exe_path = full_dir.join(exe_filename);

    // Write the slave executable
    fs::write(&final_exe_path, SLAVE_EXE)
        .map_err(|e| format!("Failed to write executable: {}", e))?;

    Ok((temp_base, final_exe_path))
}

/// Clean up a game's temp directory
fn cleanup_game(temp_dir: &PathBuf) -> Result<(), String> {
    if temp_dir.exists() {
        fs::remove_dir_all(temp_dir).map_err(|e| format!("Failed to cleanup: {}", e))?;
    }
    Ok(())
}

/// Locate the local Steam install directory via the registry
fn find_steam_path() -> Option<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Software\\Valve\\Steam") {
        if let Ok(path) = key.get_value::<String, _>("SteamPath") {
            return Some(PathBuf::from(path));
        }
    }

    if let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey("SOFTWARE\\WOW6432Node\\Valve\\Steam")
    {
        if let Ok(path) = key.get_value::<String, _>("InstallPath") {
            return Some(PathBuf::from(path));
        }
    }

    None
}

#[derive(Debug, Serialize)]
pub struct SteamLaunchInfo {
    pub installdir: String,
    /// Ranked guesses, best first - there isn't one reliable answer, so the
    /// frontend offers all of them instead of committing to a single one.
    pub candidates: Vec<String>,
}

/// Best-effort guesses at a game's install dir and Windows launch executable,
/// exposed to the frontend so the user can review/correct them before use -
/// SteamCMD's launch options don't always match the exe that actually ends up
/// running (e.g. anti-cheat bootstrapper stubs, stale/incomplete data), and
/// the real folder layout can't be verified from this API alone.
#[tauri::command]
async fn resolve_steam_launch_info(steam_app_id: String) -> Result<SteamLaunchInfo, String> {
    let (installdir, candidates) = fetch_steam_launch_info(&steam_app_id).await?;
    Ok(SteamLaunchInfo { installdir, candidates })
}

/// Ask SteamCMD's public app-info mirror for a game's install dir and a
/// ranked list of candidate Windows launch executables. Used as a fallback
/// when Discord's own `executables` list for a game is empty.
async fn fetch_steam_launch_info(app_id: &str) -> Result<(String, Vec<String>), String> {
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/{}", STEAMCMD_INFO_API_URL, app_id))
        .header("User-Agent", "Disactivity/0.1.0")
        .send()
        .await
        .map_err(|e| format!("Failed to reach SteamCMD info API: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("SteamCMD info API returned status: {}", response.status()));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse SteamCMD response: {}", e))?;

    let app = body
        .get("data")
        .and_then(|d| d.get(app_id))
        .ok_or("SteamCMD has no info for this app id")?;

    let config = app
        .get("config")
        .ok_or("SteamCMD info is missing a 'config' section for this app")?;

    let installdir = config
        .get("installdir")
        .and_then(|v| v.as_str())
        .ok_or("SteamCMD info is missing an installdir for this app")?
        .to_string();

    // Unreal Engine games report their cloud-save location under
    // "ufs.savefiles" as "<ProjectName>/Saved/...". That project name is also
    // the prefix on the packaged Windows binary
    // (`<ProjectName>-Win64-Shipping.exe`), which is a far more reliable
    // signal than the "launch" options below - those often point at an
    // anti-cheat bootstrapper rather than the real game binary that actually
    // ends up in the process list. There isn't one canonical folder layout
    // though (Marvel Rivals ships a flat "win64/" folder, others nest a
    // project-named subfolder), so we offer several ranked guesses rather
    // than committing to one.
    let ue_project_name = app
        .get("ufs")
        .and_then(|u| u.get("savefiles"))
        .and_then(|s| s.as_object())
        .and_then(|entries| {
            entries.values().find_map(|entry| {
                let path = entry.get("path")?.as_str()?;
                let (project, rest) = path.split_once('/')?;
                if rest.to_lowercase().starts_with("saved/") || rest.eq_ignore_ascii_case("saved") {
                    Some(project.to_string())
                } else {
                    None
                }
            })
        });

    let mut candidates: Vec<String> = Vec::new();

    if let Some(project) = &ue_project_name {
        // Flat "win64/" layout - confirmed against Marvel Rivals' real Discord executables entry.
        candidates.push(format!("win64/{project}-Win64-Shipping.exe"));
        // No project subfolder, capitalized Win64 (also a common Steam depot layout).
        candidates.push(format!("Binaries/Win64/{project}-Win64-Shipping.exe"));
        // Packaged-output layout: project name preserved as a subfolder.
        candidates.push(format!("{project}/Binaries/Win64/{project}-Win64-Shipping.exe"));
    }

    // Also rank whatever Valve's own "launch" options list, as further fallbacks
    // (useful for non-UE games, or if none of the guesses above are right).
    if let Some(launch_entries) = config.get("launch").and_then(|v| v.as_object()) {
        let mut sorted: Vec<(&String, &serde_json::Value)> = launch_entries.iter().collect();
        sorted.sort_by_key(|(k, _)| k.parse::<u32>().unwrap_or(u32::MAX));

        let pick_executable = |entry: &serde_json::Value| -> Option<String> {
            entry.get("executable").and_then(|v| v.as_str()).map(|s| s.to_string())
        };

        let is_windows_compatible = |entry: &serde_json::Value| -> bool {
            match entry.get("config").and_then(|c| c.get("oslist")).and_then(|v| v.as_str()) {
                Some(oslist) => oslist.split(',').any(|os| os.trim() == "windows"),
                None => true,
            }
        };

        let is_beta_only = |entry: &serde_json::Value| -> bool {
            entry.get("config").and_then(|c| c.get("betakey")).is_some()
        };

        // Anti-cheat bootstrapper stubs (EAC/BattlEye/EOS launchers) are often
        // the default, non-beta launch entry, but they're not the exe that
        // actually ends up running the game - a "direct launch" entry pointing
        // at the real game binary (usually gated behind a debug betakey) is a
        // closer match to what a genuine playthrough's process list looks like.
        let is_anticheat_launcher = |exe_name: &str| -> bool {
            let lower = exe_name.to_lowercase();
            ["protected_game", "anticheat", "battleye", "eos_launcher", "eaclauncher"]
                .iter()
                .any(|pat| lower.contains(pat))
        };

        // Prefer a "Shipping"/release build path over "Test"/"Debug" ones when both exist.
        let build_rank = |exe_name: &str| -> u8 {
            let lower = exe_name.to_lowercase();
            if lower.contains("shipping") {
                0
            } else if lower.contains("test") || lower.contains("debug") || lower.contains("dev") {
                2
            } else {
                1
            }
        };

        let mut scored: Vec<(u8, u8, u8, String)> = sorted
            .iter()
            .filter(|(_, entry)| is_windows_compatible(entry))
            .filter_map(|(_, entry)| {
                pick_executable(entry).map(|exe| {
                    (
                        is_anticheat_launcher(&exe) as u8,
                        is_beta_only(entry) as u8,
                        build_rank(&exe),
                        exe,
                    )
                })
            })
            .collect();
        scored.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));

        for (_, _, _, exe) in scored.into_iter().take(3) {
            if !candidates.iter().any(|c| c.eq_ignore_ascii_case(&exe)) {
                candidates.push(exe);
            }
        }
    }

    if candidates.is_empty() {
        return Err("Could not find a usable launch executable for this app".to_string());
    }

    Ok((installdir, candidates))
}

/// Build a minimal Steam appmanifest that marks an app as installed.
/// Deliberately sparse - just appid/name/installdir - since a fuller
/// manifest (StateFlags, depot/size bookkeeping, etc.) turned out to make
/// Steam treat the fake entry as not-really-installed on at least one setup.
fn build_appmanifest(app_id: &str, name: &str, installdir: &str) -> String {
    let safe_name = name.replace('"', "'");
    format!(
        "\"AppState\"\n{{\n\t\"appid\"\t\t\"{app_id}\"\n\t\"name\"\t\t\"{safe_name}\"\n\t\"installdir\"\t\t\"{installdir}\"\n}}\n",
        app_id = app_id,
        safe_name = safe_name,
        installdir = installdir,
    )
}

#[tauri::command]
fn start_game(
    game_id: String,
    game_name: String,
    executables: Vec<Executable>,
    selected_executable: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    // Check if game is already running
    {
        let running = state.running_games.lock().map_err(|e| e.to_string())?;
        if running.contains_key(&game_id) {
            return Err("Game is already running".to_string());
        }
    }

    // Use the selected executable if provided, otherwise auto-select the best one
    let exe_path = if let Some(selected) = selected_executable {
        selected
    } else {
        select_best_executable(&executables)
            .ok_or("No suitable win32 executable found for this game")?
    };

    // Setup the executable in temp directory
    let (temp_dir, final_exe_path) = setup_game_executable(&game_id, &exe_path)?;

    // Start the process, passing the real game title so the fake window
    // shows it instead of the spoofed exe filename
    let process = Command::new(&final_exe_path)
        .arg(&game_name)
        .spawn()
        .map_err(|e| {
            // Cleanup on failure
            let _ = cleanup_game(&temp_dir);
            format!("Failed to start process: {}", e)
        })?;

    // Store the running game
    let mut running = state.running_games.lock().map_err(|e| e.to_string())?;
    running.insert(
        game_id.clone(),
        RunningGame {
            process,
            cleanup: CleanupTarget::TempDir(temp_dir),
        },
    );

    Ok(final_exe_path.to_string_lossy().to_string())
}

/// Fallback flow for games Discord lists with no `executables` (so the normal
/// flow has no exe name to spoof) but which do have a Steam SKU. Writes a fake
/// appmanifest + install folder into the real Steam library so Steam's own
/// "currently running" detection - and by extension Discord's Steam
/// integration used by Quests - picks it up.
#[tauri::command]
async fn start_game_via_steam(
    game_id: String,
    game_name: String,
    steam_app_id: String,
    installdir: String,
    executable: String,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    // Check if game is already running
    {
        let running = state.running_games.lock().map_err(|e| e.to_string())?;
        if running.contains_key(&game_id) {
            return Err("Game is already running".to_string());
        }
    }

    let steam_path = find_steam_path().ok_or("Could not locate a Steam installation on this PC")?;
    let steamapps = steam_path.join("steamapps");
    fs::create_dir_all(&steamapps).map_err(|e| format!("Failed to access Steam library: {}", e))?;

    let exe_rel_normalized = executable.replace('\\', "/");
    let path_parts: Vec<&str> = exe_rel_normalized.split('/').collect();

    let install_root = steamapps.join("common").join(&installdir);
    let mut full_dir = install_root.clone();
    for part in &path_parts[..path_parts.len().saturating_sub(1)] {
        if !part.is_empty() {
            full_dir = full_dir.join(part);
        }
    }
    fs::create_dir_all(&full_dir).map_err(|e| format!("Failed to create fake install dir: {}", e))?;

    let exe_filename = path_parts.last().ok_or("Invalid executable path from SteamCMD")?;
    let final_exe_path = full_dir.join(exe_filename);
    fs::write(&final_exe_path, SLAVE_EXE)
        .map_err(|e| format!("Failed to write executable: {}", e))?;

    let manifest_path = steamapps.join(format!("appmanifest_{}.acf", steam_app_id));
    let manifest_content = build_appmanifest(&steam_app_id, &game_name, &installdir);
    fs::write(&manifest_path, manifest_content).map_err(|e| {
        let _ = fs::remove_dir_all(&install_root);
        format!("Failed to write fake appmanifest: {}", e)
    })?;

    let process = Command::new(&final_exe_path).arg(&game_name).spawn().map_err(|e| {
        let _ = fs::remove_file(&manifest_path);
        let _ = fs::remove_dir_all(&install_root);
        format!("Failed to start process: {}", e)
    })?;

    let mut running = state.running_games.lock().map_err(|e| e.to_string())?;
    running.insert(
        game_id,
        RunningGame {
            process,
            cleanup: CleanupTarget::SteamFake {
                manifest_path,
                install_dir: install_root,
            },
        },
    );

    Ok(final_exe_path.to_string_lossy().to_string())
}

/// Remove whatever files a running game's fake executable left behind
fn cleanup_running_game(game: &RunningGame) -> Result<(), String> {
    match &game.cleanup {
        CleanupTarget::TempDir(dir) => cleanup_game(dir),
        CleanupTarget::SteamFake {
            manifest_path,
            install_dir,
        } => {
            if manifest_path.exists() {
                fs::remove_file(manifest_path)
                    .map_err(|e| format!("Failed to remove fake appmanifest: {}", e))?;
            }
            if install_dir.exists() {
                fs::remove_dir_all(install_dir)
                    .map_err(|e| format!("Failed to remove fake Steam install: {}", e))?;
            }
            Ok(())
        }
    }
}

#[tauri::command]
fn stop_game(game_id: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut running = state.running_games.lock().map_err(|e| e.to_string())?;

    if let Some(mut game) = running.remove(&game_id) {
        // Kill the process
        let _ = game.process.kill();
        let _ = game.process.wait();

        // Cleanup whatever this game's flow left behind
        cleanup_running_game(&game)?;
    }

    Ok(())
}

#[tauri::command]
fn get_running_games(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    let running = state.running_games.lock().map_err(|e| e.to_string())?;
    Ok(running.keys().cloned().collect())
}

#[tauri::command]
fn get_favorites() -> Vec<String> {
    read_favorites().into_iter().collect()
}

#[tauri::command]
fn add_favorite(game_id: String) -> Result<(), String> {
    let mut favorites = read_favorites();
    favorites.insert(game_id);
    write_favorites(&favorites)
}

#[tauri::command]
fn remove_favorite(game_id: String) -> Result<(), String> {
    let mut favorites = read_favorites();
    favorites.remove(&game_id);
    write_favorites(&favorites)
}

#[tauri::command]
fn toggle_favorite(game_id: String) -> Result<bool, String> {
    let mut favorites = read_favorites();
    let is_favorite = if favorites.contains(&game_id) {
        favorites.remove(&game_id);
        false
    } else {
        favorites.insert(game_id);
        true
    };
    write_favorites(&favorites)?;
    Ok(is_favorite)
}

/// Stop all running games and cleanup
fn cleanup_all_games(state: &AppState) {
    if let Ok(mut running) = state.running_games.lock() {
        for (_, mut game) in running.drain() {
            let _ = game.process.kill();
            let _ = game.process.wait();
            let _ = cleanup_running_game(&game);
        }
    }

    // Also cleanup the base disactivity temp directory if it exists
    let temp_base = std::env::temp_dir().join("disactivity");
    if temp_base.exists() {
        let _ = fs::remove_dir_all(&temp_base);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(AppState {
            running_games: Mutex::new(HashMap::new()),
        })
        .invoke_handler(tauri::generate_handler![
            fetch_games,
            get_cache_info,
            start_game,
            resolve_steam_launch_info,
            start_game_via_steam,
            stop_game,
            get_running_games,
            get_favorites,
            add_favorite,
            remove_favorite,
            toggle_favorite
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                // Cleanup all games when window is closed
                if let Some(state) = window.try_state::<AppState>() {
                    cleanup_all_games(state.inner());
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
