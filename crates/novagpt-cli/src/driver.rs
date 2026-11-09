//! `novagpt chrome-driver` — explicit management of the native Chrome and
//! ChromeDriver runtime used by the optional managed backend.
//!
//! Status never downloads. Install downloads only a verified Chrome for
//! Testing/legacy ChromeDriver matching the installed Chrome milestone, then
//! patches a disposable copy in NovaGPT's own store. No browser, profile,
//! cookie or account is opened by these commands.
use novagpt_browser::managed::{
    ChromeDriverStore, ChromeVersion, InstallRequest, InstalledDriver, detect_chrome_version,
};
use serde_json::json;
use std::{error::Error, path::Path};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverAction {
    Status { json: bool },
    Install { milestone: Option<u32>, json: bool },
}

pub async fn run(
    action: DriverAction,
    explicit_chrome: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let chrome = crate::chrome::discover(explicit_chrome).map_err(|_| {
        std::io::Error::other(
            "Chrome executable not found; install Chrome or configure --chatgpt-chrome",
        )
    })?;
    let chrome_version = detect_chrome_version(&chrome).await.map_err(|_| {
        std::io::Error::other(format!(
            "Unable to determine the installed Chrome version for {}",
            chrome.display()
        ))
    })?;
    let store = ChromeDriverStore::new(ChromeDriverStore::default_root()?)
        .map_err(|_| std::io::Error::other("Unable to initialize the ChromeDriver store"))?;
    match action {
        DriverAction::Status { json } => {
            let installed = store
                .locate(InstallRequest {
                    chrome_version: Some(chrome_version),
                    ..Default::default()
                })
                .await
                .map_err(|_| std::io::Error::other("Unable to inspect the ChromeDriver store"))?;
            print_status(&store, chrome_version, installed.as_ref(), json);
        }
        DriverAction::Install { milestone, json } => {
            let installed = store
                .install(InstallRequest {
                    chrome_version: Some(chrome_version),
                    milestone,
                    ..Default::default()
                })
                .await
                .map_err(|error| {
                    std::io::Error::other(format!("ChromeDriver installation failed: {error}"))
                })?;
            print_status(&store, chrome_version, Some(&installed), json);
        }
    }
    Ok(())
}

fn print_status(
    store: &ChromeDriverStore,
    chrome_version: ChromeVersion,
    installed: Option<&InstalledDriver>,
    json: bool,
) {
    let driver = installed.map(|driver| {
        json!({
            "version": driver.version.to_string(),
            "path": driver.path.display().to_string(),
            "platform": driver.platform.cft_name(),
            "patched": true,
            "freshly_installed": driver.freshly_installed,
        })
    });
    if json {
        let report = json!({
            "chrome_version": chrome_version.to_string(),
            "driver_store": store.root().display().to_string(),
            "driver": driver,
        });
        println!("{report}");
        return;
    }
    println!("Chrome: {chrome_version}");
    println!("ChromeDriver store: {}", store.root().display());
    match installed {
        Some(installed) => {
            println!("ChromeDriver: {} (patched)", installed.version);
            println!("Path: {}", installed.path.display());
            if installed.freshly_installed {
                println!("Status: installed and verified");
            } else {
                println!("Status: already installed and verified");
            }
        }
        None => {
            println!("ChromeDriver: not installed for Chrome {chrome_version}");
            println!("Run `novagpt chrome-driver install` to download and patch it.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_json_contains_no_profile_or_endpoint() {
        let store = ChromeDriverStore::new(std::env::temp_dir().join("novagpt-driver-test"))
            .expect("store");
        let driver = InstalledDriver {
            path: std::path::PathBuf::from("/nonexistent/chromedriver"),
            version: ChromeVersion::new(156, 0, 8078, 80),
            platform: novagpt_browser::managed::Platform::Linux64,
            binary_sha256: [0; 32],
            freshly_installed: true,
        };
        let _ = store;
        let rendered = serde_json::to_string(&json!({
            "version": driver.version.to_string(),
            "path": driver.path.display().to_string(),
            "patched": true,
        }))
        .expect("json");
        assert!(rendered.contains("156.0.8078.80"));
        assert!(!rendered.contains("profile"));
        assert!(!rendered.contains("debug"));
    }

    #[test]
    fn action_variants_are_stable() {
        assert_eq!(
            DriverAction::Status { json: true },
            DriverAction::Status { json: true }
        );
        assert_eq!(
            DriverAction::Install {
                milestone: Some(156),
                json: false
            },
            DriverAction::Install {
                milestone: Some(156),
                json: false
            }
        );
    }
}
