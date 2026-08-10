//! Discover installed applications and launch them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// A discovered application bundle.
#[derive(Clone, Debug)]
pub struct AppEntry {
    /// Display name (from `Info.plist`, falling back to the bundle file stem).
    pub name: String,
    /// Absolute path to the `.app` bundle.
    pub path: PathBuf,
    pub bundle_id: Option<String>,
}

const SEARCH_DIRS: &[&str] = &[
    "/Applications",
    "/Applications/Utilities",
    "/System/Applications",
    "/System/Applications/Utilities",
];

/// Scan the standard application directories for `.app` bundles.
pub fn scan_apps() -> Vec<AppEntry> {
    let mut dirs: Vec<PathBuf> = SEARCH_DIRS.iter().map(PathBuf::from).collect();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Applications"));
    }

    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for dir in dirs {
        // `max_depth(1)` lists the directory's direct children without
        // descending into the `.app` bundles themselves.
        for entry in WalkDir::new(&dir)
            .max_depth(1)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "app") && seen.insert(path.to_path_buf()) {
                apps.push(make_entry(path));
            }
        }
    }

    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

fn make_entry(path: &Path) -> AppEntry {
    let (name, bundle_id) = read_info_plist(&path.join("Contents/Info.plist"));
    let name = name.unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    AppEntry {
        name,
        path: path.to_path_buf(),
        bundle_id,
    }
}

fn read_info_plist(info: &Path) -> (Option<String>, Option<String>) {
    let Ok(value) = plist::Value::from_file(info) else {
        return (None, None);
    };
    let Some(dict) = value.as_dictionary() else {
        return (None, None);
    };
    let name = dict
        .get("CFBundleDisplayName")
        .or_else(|| dict.get("CFBundleName"))
        .and_then(|v| v.as_string())
        .map(String::from);
    let bundle_id = dict
        .get("CFBundleIdentifier")
        .and_then(|v| v.as_string())
        .map(String::from);
    (name, bundle_id)
}

/// Launch an application (or open any path) via `/usr/bin/open`.
pub fn launch(path: &Path) -> anyhow::Result<()> {
    std::process::Command::new("/usr/bin/open").arg(path).spawn()?;
    Ok(())
}

/// Bundle identifier of the app currently in front, if any.
///
/// Used before synthesizing keystrokes so input only ever lands in the app the
/// user actually switched to.
pub fn frontmost_bundle_id() -> Option<String> {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return None;
        }
        let app: *mut Object = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return None;
        }
        let id: *mut Object = msg_send![app, bundleIdentifier];
        if id.is_null() {
            return None;
        }
        let utf8: *const std::os::raw::c_char = msg_send![id, UTF8String];
        if utf8.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr(utf8).to_str().ok().map(str::to_owned)
    }
}
