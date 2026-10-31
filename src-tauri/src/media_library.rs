//! The hub's media library: what the room has been sent to project.
//!
//! Why this exists rather than playing straight off a sender: a queue that points
//! at a phone's HTTP server stops the moment that phone sleeps, and a meeting room
//! cannot have its slideshow break because somebody locked their screen. Media that
//! matters is copied onto the hub and served from here. A remote URL is still
//! accepted — a 4 GB clip should not have to cross the LAN twice — but it is stored
//! as a URL item, so the difference stays visible on the big screen.
//!
//! ## The index is the only path to a file
//!
//! Nothing opens a file by name from the network. A request carries an id, the id is
//! looked up in the index, and the stored file name is checked for separators before
//! it is joined to the root. A hand-edited index claiming `..\..\windows\win.ini` is
//! therefore refused rather than served, which matters because the HTTP endpoint is
//! reachable by anything on the LAN.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// What kind of item this is. Mirrors `horizon_cast::queue::MediaKind` so the
/// library and the playlist never disagree about what is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

impl MediaKind {
    fn from_name(name: &str) -> Self {
        match horizon_cast::queue::MediaKind::from_uri(name) {
            horizon_cast::queue::MediaKind::Image => MediaKind::Image,
            horizon_cast::queue::MediaKind::Audio => MediaKind::Audio,
            horizon_cast::queue::MediaKind::Video => MediaKind::Video,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        }
    }

    /// The playlist's spelling of the same three kinds, so a library entry keeps
    /// its own classification when it becomes a track.
    pub fn to_cast(self) -> horizon_cast::queue::MediaKind {
        match self {
            MediaKind::Image => horizon_cast::queue::MediaKind::Image,
            MediaKind::Audio => horizon_cast::queue::MediaKind::Audio,
            MediaKind::Video => horizon_cast::queue::MediaKind::Video,
        }
    }
}

/// One library entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaItem {
    pub id: String,
    /// Shown on the big screen. A library file keeps its name without the
    /// dedupe suffix, so `照片 (2).jpg` still reads as `照片.jpg`.
    pub title: String,
    pub kind: MediaKind,
    /// File name inside the library directory. `None` for a remote item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The address a remote item plays from. Never stored for a library file:
    /// the hub's own IP changes, so its URL is built at read time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub bytes: u64,
    /// Unix milliseconds. The library's default order, newest last.
    pub added_at: i64,
    /// Who put it here: `browser`, `android`, `controller`, `local`.
    #[serde(default)]
    pub source: String,
}

/// What a delete actually did. `existed` distinguishes "removed" from "there was
/// never such an id", which the HTTP layer turns into 200 versus 404.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Removed {
    pub existed: bool,
    pub file_error: Option<String>,
}

/// The library. One instance, owned by the app state and shared with the HTTP
/// server, so the list survives a panel closing and the hub restarting.
pub struct Library {
    root: PathBuf,
    items: Mutex<Vec<MediaItem>>,
}

impl Library {
    /// The default home: `%APPDATA%\horizon-launcher\media`.
    ///
    /// `HORIZON_MEDIA_LIBRARY` overrides it. The wire suite for this feature
    /// uploads 50 MB of test bytes and names items 「开幕.mp4」, and that must not
    /// land in the folder the operator actually projects from.
    pub fn default_root() -> PathBuf {
        if let Some(root) = std::env::var_os("HORIZON_MEDIA_LIBRARY") {
            let root = PathBuf::from(root);
            if !root.as_os_str().is_empty() {
                return root;
            }
        }
        dirs_next::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("horizon-launcher")
            .join("media")
    }

    /// Open the library, reading the index. A missing or unreadable index is
    /// rebuilt from the directory, because files dropped into the folder by hand
    /// — or left by a crash mid-write — should still be projectable.
    pub fn open(root: PathBuf) -> Self {
        // Created eagerly: the panel offers this folder to the user, and an
        // address that does not exist yet is a dead end to explain later.
        let _ = std::fs::create_dir_all(&root);
        let items = read_index(&root).unwrap_or_else(|| scan(root.as_path()));
        Self {
            root,
            items: Mutex::new(items),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn items(&self) -> Vec<MediaItem> {
        self.lock().clone()
    }

    pub fn get(&self, id: &str) -> Option<MediaItem> {
        self.lock().iter().find(|item| item.id == id).cloned()
    }

    /// Where an incoming file should be written, before it becomes an entry. The
    /// caller streams into this path and then calls `add_file`; a transfer that
    /// dies half-way leaves a file nobody lists, and never an entry pointing at
    /// bytes that never arrived.
    pub fn stage(&self, desired_name: &str) -> (String, PathBuf) {
        let name = unique_name(&self.root, &sanitize_file_name(desired_name));
        let path = self.root.join(&name);
        (name, path)
    }

    /// Register a file that is already written.
    pub fn add_file(&self, file_name: String, title: &str, bytes: u64, source: &str) -> MediaItem {
        let item = MediaItem {
            id: new_id(),
            title: if title.is_empty() {
                stem(&file_name)
            } else {
                title.to_string()
            },
            kind: MediaKind::from_name(&file_name),
            file: Some(file_name),
            url: None,
            bytes,
            added_at: now_millis(),
            source: source.to_string(),
        };
        self.push(item.clone());
        item
    }

    /// Register a remote address. Only `http` and `https` are accepted: the hub
    /// hands this string to a media engine that will follow it, so a `file://`
    /// or a scheme that can reach the local machine is refused here rather than
    /// discovered later.
    pub fn add_url(&self, url: &str, title: &str, source: &str) -> Result<MediaItem, String> {
        let lower = url.trim().to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return Err("只支持 http 或 https 地址".to_string());
        }
        if url.chars().count() > 4096 {
            return Err("地址过长".to_string());
        }
        let name = url
            .split(['?', '#'])
            .next()
            .and_then(|path| path.rsplit('/').next().map(str::to_string))
            .unwrap_or_default();
        let fallback = if name.is_empty() {
            url.to_string()
        } else {
            name.clone()
        };
        let item = MediaItem {
            id: new_id(),
            title: if title.is_empty() { fallback } else { title.to_string() },
            kind: MediaKind::from_name(&name),
            file: None,
            url: Some(url.trim().to_string()),
            bytes: 0,
            added_at: now_millis(),
            source: source.to_string(),
        };
        self.push(item.clone());
        Ok(item)
    }

    /// Drop an entry, and its file with it. Deleting a library entry the room can
    /// still see would leave a tile that plays nothing, so the file goes too.
    /// A file that will not go — held open by the player, or on a disconnected
    /// drive — is reported rather than hidden, because those bytes stay on disk.
    pub fn remove(&self, id: &str) -> Result<Removed, String> {
        let removed = {
            let mut items = self.lock();
            let position = items.iter().position(|item| item.id == id);
            let removed = position.map(|index| items.remove(index));
            if position.is_some() {
                write_index(&self.root, &items)?;
            }
            removed
        };
        let Some(item) = removed else {
            return Ok(Removed::default());
        };
        let file_error = match item
            .file
            .as_deref()
            .and_then(|name| self.path_of(name))
        {
            Some(path) => match std::fs::remove_file(&path) {
                Ok(()) => None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => Some(error.to_string()),
            },
            None => None,
        };
        Ok(Removed {
            existed: true,
            file_error,
        })
    }

    /// Rename an entry's title. The file on disk is not touched: the id is what
    /// the transport plays by, and moving bytes to chase a label would risk the
    /// item currently on screen.
    pub fn rename(&self, id: &str, title: &str) -> Result<MediaItem, String> {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return Err("标题不能为空".to_string());
        }
        let mut items = self.lock();
        let Some(item) = items.iter_mut().find(|item| item.id == id) else {
            return Err("片库里没有这一项".to_string());
        };
        item.title = trimmed.chars().take(200).collect();
        let updated = item.clone();
        write_index(&self.root, &items)?;
        Ok(updated)
    }

    /// The absolute path of a stored file, or `None` when the name is not one this
    /// library would accept. Guards against an index edited outside the app.
    pub fn path_of(&self, file_name: &str) -> Option<PathBuf> {
        name_inside_root(file_name).then(|| self.root.join(file_name))
    }

    /// The path a playing item should be served from, by id.
    pub fn path_for_id(&self, id: &str) -> Option<PathBuf> {
        let item = self.get(id)?;
        self.path_of(item.file.as_deref()?)
    }

    /// The address the transport should be handed for this item: the hub's own
    /// file endpoint for a library file, the sender's address for a remote item.
    ///
    /// The file name rides at the end of the path on purpose. `/media/<id>` alone
    /// is unreadable to everything that only has an address to go on: MediaEngine
    /// cannot tell a still photo from a film and answers an image with
    /// `SRC_NOT_SUPPORTED`, and the stage and the DLNA `res` both label the item
    /// from the last path segment. The id is still the only thing looked up — the
    /// name is a label, so a wrong one changes the caption, never the bytes.
    pub fn play_url(&self, item: &MediaItem, base: &str) -> String {
        match (&item.file, &item.url) {
            (Some(file), _) => format!(
                "{}/media/{}/{}",
                base.trim_end_matches('/'),
                item.id,
                encode_segment(file)
            ),
            (None, Some(url)) => url.clone(),
            (None, None) => String::new(),
        }
    }

    fn push(&self, item: MediaItem) {
        let mut items = self.lock();
        items.push(item.clone());
        if let Err(error) = write_index(&self.root, &items) {
            // The entry is in memory and the index on disk says otherwise. Keep
            // playing it — a room mid-presentation is not the place to fail — but
            // the caller has to be told the library may not survive a restart.
            eprintln!("[media] 片库索引写入失败：{error}");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<MediaItem>> {
        self.items.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Percent-encode one path segment. A library file keeps its real name — spaces,
/// `#`, Chinese characters — and the address that reaches a phone or MediaEngine
/// has to survive being read back as one segment.
fn encode_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// A name is only ever joined to the root, so anything that could escape it — a
/// separator, a drive, a relative segment — is refused before it reaches `join`.
fn name_inside_root(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.contains(['/', '\\']) || name.contains(':') {
        return false;
    }
    !std::path::Path::new(name)
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
}

/// Windows forbids these in a file name, and a name from another machine is not
/// trustworthy anyway: a control character is how a path ends up outside a folder.
pub fn sanitize_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("file").trim();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "file".to_string()
    } else {
        cleaned
    }
}

/// A name that is free in `dir`, so two people sending `照片.jpg` do not overwrite
/// each other.
fn unique_name(dir: &Path, name: &str) -> String {
    if !dir.join(name).exists() {
        return name.to_string();
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let extension = path
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned());
    for index in 2..1000 {
        let candidate = match &extension {
            Some(ext) => format!("{stem} ({index}).{ext}"),
            None => format!("{stem} ({index})"),
        };
        if !dir.join(&candidate).exists() {
            return candidate;
        }
    }
    format!("{stem}-{}", now_millis())
}

fn stem(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string())
}

fn index_path(root: &Path) -> PathBuf {
    root.join("index.json")
}

fn read_index(root: &Path) -> Option<Vec<MediaItem>> {
    let text = std::fs::read_to_string(index_path(root)).ok()?;
    let items: Vec<MediaItem> = serde_json::from_str(&text).ok()?;
    Some(items.into_iter().filter(|item| usable(root, item)).collect())
}

fn write_index(root: &Path, items: &[MediaItem]) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let text = serde_json::to_string_pretty(items).map_err(|error| error.to_string())?;
    std::fs::write(index_path(root), text).map_err(|error| error.to_string())
}

/// An entry the room cannot play is not worth showing: a name that tries to
/// escape the library, or a file somebody deleted behind the app's back, is
/// dropped when the library opens. A tile that fails when pressed is worse than
/// one that is not there.
fn usable(root: &Path, item: &MediaItem) -> bool {
    match (&item.file, &item.url) {
        (Some(name), _) => {
            name_inside_root(name) && root.join(name).is_file()
        }
        (None, Some(url)) => !url.is_empty(),
        (None, None) => false,
    }
}

/// Rebuild the list from the folder itself, oldest first so the order still reads
/// as a history of the room.
fn scan(root: &Path) -> Vec<MediaItem> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<MediaItem> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let metadata = std::fs::metadata(&path).ok()?;
            if !metadata.is_file() {
                return None;
            }
            let name = path.file_name()?.to_string_lossy().into_owned();
            if name == "index.json" || !is_media_name(&name) {
                return None;
            }
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_millis() as i64)
                .unwrap_or_default();
            Some(MediaItem {
                id: new_id(),
                title: stem(&name),
                kind: MediaKind::from_name(&name),
                file: Some(name),
                url: None,
                bytes: metadata.len(),
                added_at: modified,
                source: "local".to_string(),
            })
        })
        .collect();
    found.sort_by_key(|item| item.added_at);
    found
}

/// Only extensions the queue can classify as media. A text file left in the
/// folder is somebody's note, not something to offer for projection.
fn is_media_name(name: &str) -> bool {
    let kind = MediaKind::from_name(name);
    let lower = name.to_ascii_lowercase();
    let extension = Path::new(&lower)
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_default();
    !extension.is_empty()
        && match kind {
            MediaKind::Image => [
                "jpg", "jpeg", "png", "webp", "gif", "bmp", "heic", "heif", "avif",
            ]
            .contains(&extension.as_str()),
            MediaKind::Audio => [
                "mp3", "wav", "aac", "flac", "ogg", "oga", "wma", "m4a", "mka", "opus",
            ]
            .contains(&extension.as_str()),
            MediaKind::Video => [
                "mp4", "mov", "mkv", "avi", "wmv", "flv", "webm", "mpg", "mpeg", "ts", "m4v",
                "3gp",
            ]
            .contains(&extension.as_str()),
        }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder that disappears when the test ends, so the suite leaves nothing
    /// in the real library.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "horizon-library-{label}-{}-{}",
                std::process::id(),
                new_id()
            ));
            std::fs::create_dir_all(&path).expect("create");
            Self(path)
        }

        fn library(&self) -> Library {
            Library::open(self.0.clone())
        }

        fn write(&self, name: &str) {
            std::fs::write(self.0.join(name), b"payload").expect("write");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_index_round_trips_through_disk() {
        let dir = TempDir::new("roundtrip");
        let library = dir.library();
        let (name, path) = library.stage("海 报.jpg");
        std::fs::write(&path, b"x").expect("write");
        let item = library.add_file(name, "海报", 1, "browser");
        assert_eq!(item.kind, MediaKind::Image);
        assert_eq!(item.source, "browser");

        let reopened = dir.library();
        let items = reopened.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, item.id);
        assert_eq!(items[0].title, "海报");
        assert_eq!(items[0].bytes, 1);
    }

    #[test]
    fn a_missing_index_is_rebuilt_from_the_folder() {
        let dir = TempDir::new("scan");
        dir.write("clip.mp4");
        dir.write("photo.jpg");
        dir.write("song.mp3");
        dir.write("notes.txt");
        std::fs::create_dir(dir.0.join("subdir")).expect("mkdir");
        dir.write("subdir/inside.mp4");

        let library = dir.library();
        let items = library.items();
        let titles: Vec<&str> = items.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(titles.len(), 3, "a scan takes only files in the root: {titles:?}");
        assert!(titles.contains(&"clip"));
        assert!(titles.contains(&"photo"));
        assert!(titles.contains(&"song"));
        assert!(!titles.contains(&"notes"), "a text file is not media");
        assert!(items.iter().all(|item| item.source == "local"));
    }

    #[test]
    fn names_are_sanitized_and_deduped_before_they_touch_the_disk() {
        let dir = TempDir::new("names");
        let library = dir.library();
        assert_eq!(sanitize_file_name("..\\..\\windows\\win.ini"), "win.ini");
        assert_eq!(sanitize_file_name("a/b/c.txt"), "c.txt");
        assert_eq!(sanitize_file_name(""), "file");

        let (first, path) = library.stage("报告.pdf");
        std::fs::write(&path, b"one").expect("write");
        library.add_file(first, "", 3, "browser");
        let (second, _) = library.stage("报告.pdf");
        assert_eq!(second, "报告 (2).pdf");
    }

    #[test]
    fn an_entry_is_only_ever_reachable_through_its_id() {
        let dir = TempDir::new("jail");
        let library = dir.library();
        assert!(library.path_for_id("nope").is_none());

        let (name, path) = library.stage("clip.mp4");
        std::fs::write(&path, b"two").expect("write");
        let item = library.add_file(name, "", 3, "android");
        assert_eq!(library.path_for_id(&item.id), Some(path));
    }

    #[test]
    fn an_index_written_by_hand_cannot_reach_outside_the_library() {
        let dir = TempDir::new("escape");
        std::fs::write(
            index_path(&dir.0),
            serde_json::to_vec(&vec![MediaItem {
                id: "evil".into(),
                title: "escape".into(),
                kind: MediaKind::Video,
                file: Some("..\\..\\windows\\system.ini".into()),
                url: None,
                bytes: 1,
                added_at: 1,
                source: "controller".into(),
            }])
            .expect("json"),
        )
        .expect("write index");

        let library = dir.library();
        assert!(
            library.items().is_empty(),
            "the escaping entry is refused at load"
        );
        assert!(library.path_for_id("evil").is_none());
        assert!(!name_inside_root("..\\windows\\system.ini"));
        assert!(!name_inside_root("a/b.mp4"));
        assert!(name_inside_root("a.mp4"));
    }

    #[test]
    fn removing_an_entry_takes_its_file_with_it() {
        let dir = TempDir::new("remove");
        let library = dir.library();
        let (name, path) = library.stage("clip.mp4");
        std::fs::write(&path, b"three").expect("write");
        let item = library.add_file(name, "", 5, "browser");
        assert!(path.exists());
        let removed = library.remove(&item.id).expect("remove");
        assert!(removed.existed);
        assert_eq!(removed.file_error, None);
        assert!(!path.exists(), "the bytes go with the entry");
        assert!(library.items().is_empty());
        assert!(read_index(&dir.0).expect("index").is_empty());
    }

    #[test]
    fn removing_an_id_that_is_not_there_is_not_removal() {
        let dir = TempDir::new("absent");
        let library = dir.library();
        let removed = library.remove("nope").expect("remove");
        assert!(!removed.existed, "the HTTP layer needs the 404 to be real");
    }

    #[test]
    fn removing_the_same_id_twice_reports_only_one_removal() {
        let dir = TempDir::new("twice");
        let library = dir.library();
        let (name, path) = library.stage("clip.mp4");
        std::fs::write(&path, b"four").expect("write");
        let item = library.add_file(name, "", 4, "browser");
        assert!(library.remove(&item.id).expect("first").existed);
        assert!(!library.remove(&item.id).expect("second").existed);
    }

    #[test]
    fn an_entry_whose_file_vanished_off_the_disk_is_not_listed() {
        let dir = TempDir::new("gone");
        let library = dir.library();
        let (name, path) = library.stage("clip.mp4");
        std::fs::write(&path, b"eight").expect("write");
        library.add_file(name, "", 5, "browser");
        assert_eq!(library.items().len(), 1);
        std::fs::remove_file(&path).expect("delete behind the app's back");
        assert!(
            dir.library().items().is_empty(),
            "a tile that cannot play is not offered"
        );
    }

    #[test]
    fn only_http_addresses_are_accepted_as_remote_items() {
        let dir = TempDir::new("url");
        let library = dir.library();
        let item = library
            .add_url("https://host/a/clip.mp4?t=1", "", "browser")
            .expect("url");
        assert_eq!(item.kind, MediaKind::Video);
        assert_eq!(item.title, "clip.mp4");
        assert_eq!(library.add_url("file:///C:/windows/a.mp4", "", "browser").err(), Some("只支持 http 或 https 地址".to_string()));
        assert!(library.add_url("javascript:alert(1)", "", "browser").is_err());
        assert_eq!(library.items().len(), 1, "a refused address adds nothing");
    }

    #[test]
    fn a_library_file_is_served_by_id_and_a_remote_item_is_not() {
        let dir = TempDir::new("playurl");
        let library = dir.library();
        let (name, path) = library.stage("clip.mp4");
        std::fs::write(&path, b"five").expect("write");
        let stored = library.add_file(name, "", 4, "browser");
        let remote = library
            .add_url("http://192.168.0.9:8080/clip.mp4", "", "android")
            .expect("url");
        assert_eq!(
            library.play_url(&stored, "http://192.168.0.24:7878"),
            format!("http://192.168.0.24:7878/media/{}/clip.mp4", stored.id)
        );
        assert_eq!(
            library.play_url(&remote, "http://192.168.0.24:7878"),
            "http://192.168.0.9:8080/clip.mp4"
        );

        // The name at the end is what a client that only holds an address reads the
        // kind from — the native player's still-image check included. A photo has to
        // come back as a photo from its own URL, and a real (spaced, Chinese) name
        // has to survive as one segment.
        let (photo_name, photo_path) = library.stage("现场 照片#1.jpg");
        std::fs::write(&photo_path, b"jpeg").expect("write");
        let photo = library.add_file(photo_name, "", 4, "browser");
        let url = library.play_url(&photo, "http://192.168.0.24:7878");
        assert_eq!(
            url,
            format!(
                "http://192.168.0.24:7878/media/{}/%E7%8E%B0%E5%9C%BA%20%E7%85%A7%E7%89%87%231.jpg",
                photo.id
            )
        );
        assert_eq!(
            horizon_cast::queue::MediaKind::from_uri(&url),
            horizon_cast::queue::MediaKind::Image,
            "the address alone has to say it is a photo: {url}"
        );
    }

    #[test]
    fn the_kind_matches_what_the_queue_will_do_with_it() {
        // The library classifies through the same function the playlist uses, so
        // an item cannot be an image here and timed media there.
        assert_eq!(MediaKind::from_name("a.JPG"), MediaKind::Image);
        assert_eq!(MediaKind::from_name("a.m4a"), MediaKind::Audio);
        assert_eq!(MediaKind::from_name("a.mkv"), MediaKind::Video);
        assert_eq!(MediaKind::from_name("no-extension"), MediaKind::Video);
    }

    #[test]
    fn renaming_leaves_the_bytes_alone() {
        let dir = TempDir::new("rename");
        let library = dir.library();
        let (name, path) = library.stage("clip.mp4");
        std::fs::write(&path, b"six").expect("write");
        let item = library.add_file(name, "", 3, "browser");
        let renamed = library.rename(&item.id, "开幕式视频").expect("rename");
        assert_eq!(renamed.title, "开幕式视频");
        assert!(path.exists(), "the file keeps its name");
        assert_eq!(dir.library().items()[0].title, "开幕式视频", "it is persisted");
        assert!(library.rename(&item.id, "   ").is_err());
    }

    #[test]
    fn a_stored_item_survives_a_reopen_and_keeps_its_order() {
        let dir = TempDir::new("order");
        let library = dir.library();
        let mut ids = Vec::new();
        for name in ["a.mp4", "b.mp4", "c.mp4"] {
            let (stored, path) = library.stage(name);
            std::fs::write(&path, b"seven").expect("write");
            ids.push(library.add_file(stored, "", 5, "browser").id);
        }
        let reopened = dir.library();
        let listed: Vec<String> = reopened.items().iter().map(|item| item.id.clone()).collect();
        assert_eq!(listed, ids, "the library is a history, not a set");
    }
}
