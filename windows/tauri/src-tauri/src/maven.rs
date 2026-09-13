//! Windows persistence for Maven project and machine-local configuration.
//!
//! Portable selections stay below the workspace `.lithe` directory. Maven,
//! JDK, and settings paths are stored only in the application data directory.

use crate::run::{atomic_write, resolve_java_home, resolve_maven_executable};
use regex::Regex;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tauri::{AppHandle, Manager};

const MAVEN_CONFIGURATION_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MavenPortableConfiguration {
    pub version: u32,
    #[serde(default)]
    pub selected_profiles: Vec<String>,
    #[serde(default)]
    pub custom_profiles: Vec<String>,
    #[serde(default)]
    pub skip_tests: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MavenLocalConfiguration {
    pub version: u32,
    #[serde(default)]
    pub settings_path: Option<String>,
    #[serde(default)]
    pub local_repository_path: Option<String>,
    #[serde(default)]
    pub maven_executable_path: Option<String>,
    #[serde(default)]
    pub java_home_path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MavenStoredConfiguration {
    pub portable: Option<MavenPortableConfiguration>,
    pub local: Option<MavenLocalConfiguration>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteMavenConfigurationArgs {
    pub root: PathBuf,
    pub reactor_path: String,
    pub configuration: MavenStoredConfiguration,
}

#[tauri::command]
pub fn maven_load_configuration(
    app: AppHandle,
    root: PathBuf,
    reactor_path: String,
) -> Result<MavenStoredConfiguration, String> {
    let root = existing_directory(&root)?;
    let portable = read_optional::<MavenPortableConfiguration>(&portable_path(&root))?;
    let local = read_optional::<MavenLocalConfiguration>(&local_path(&app, &root, &reactor_path)?)?;
    validate_versions(portable.as_ref(), local.as_ref())?;
    Ok(MavenStoredConfiguration { portable, local })
}

#[tauri::command]
pub fn maven_write_configuration(
    app: AppHandle,
    args: WriteMavenConfigurationArgs,
) -> Result<(), String> {
    let root = existing_directory(&args.root)?;
    validate_versions(
        args.configuration.portable.as_ref(),
        args.configuration.local.as_ref(),
    )?;
    write_optional(&portable_path(&root), args.configuration.portable.as_ref())?;
    write_optional(
        &local_path(&app, &root, &args.reactor_path)?,
        args.configuration.local.as_ref(),
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveMavenEffectiveArgs {
    pub root: PathBuf,
    #[serde(default)]
    pub working_directory: String,
    #[serde(default)]
    pub settings_path: String,
    #[serde(default)]
    pub local_repository_path: String,
    #[serde(default)]
    pub maven_executable_path: String,
    #[serde(default)]
    pub java_home_path: String,
}

/// The configuration values a Maven launch would actually use, with `None`
/// meaning the machine-level detection found nothing.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MavenEffectiveConfiguration {
    pub settings_path: Option<String>,
    pub local_repository_path: Option<String>,
    pub maven_executable_path: Option<String>,
    pub java_home_path: Option<String>,
}

/// Resolves the effective Maven configuration the same way a launch does, so
/// the settings UI can show which values empty fields will end up using.
#[tauri::command]
pub fn maven_resolve_effective_configuration(
    args: ResolveMavenEffectiveArgs,
) -> Result<MavenEffectiveConfiguration, String> {
    let root = existing_directory(&args.root)?;
    let working_directory = if args.working_directory.trim().is_empty() {
        root.clone()
    } else {
        root.join(&args.working_directory)
    };
    let maven_executable_path =
        resolve_maven_executable(&root, &working_directory, &args.maven_executable_path).ok();
    let java_home_path = resolve_java_home(&root, &args.java_home_path)
        .ok()
        .flatten();
    let settings_path = effective_settings_path(
        &args.settings_path,
        user_home_dir().as_deref(),
        maven_executable_path.as_deref(),
    );
    let local_repository_path = effective_local_repository(
        &args.local_repository_path,
        settings_path.as_deref(),
        user_home_dir().as_deref(),
    );
    Ok(MavenEffectiveConfiguration {
        settings_path,
        local_repository_path,
        maven_executable_path,
        java_home_path,
    })
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var("USERPROFILE").ok().map(PathBuf::from)
}

/// The settings.xml file Maven itself would read: the configured path, then the
/// user-level default, then the detected installation's global settings.
fn effective_settings_path(
    configured: &str,
    home: Option<&Path>,
    maven_executable_path: Option<&str>,
) -> Option<String> {
    if !configured.trim().is_empty() {
        return Some(configured.trim().to_string());
    }
    settings_candidates(home, maven_executable_path)
        .into_iter()
        .find(|candidate| candidate.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

fn settings_candidates(home: Option<&Path>, maven_executable_path: Option<&str>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = home {
        candidates.push(home.join(".m2").join("settings.xml"));
    }
    if let Some(executable) = maven_executable_path {
        // .../bin/mvn.cmd -> .../conf/settings.xml
        if let Some(installation) = Path::new(executable).parent().and_then(Path::parent) {
            candidates.push(installation.join("conf").join("settings.xml"));
        }
    }
    candidates
}

/// Resolves the local repository Maven will actually use: the configured
/// override, then `<localRepository>` in the effective settings.xml, then the
/// `~/.m2/repository` default.
fn effective_local_repository(
    configured: &str,
    settings_path: Option<&str>,
    home: Option<&Path>,
) -> Option<String> {
    if !configured.trim().is_empty() {
        return Some(configured.trim().to_string());
    }
    if let Some(path) = settings_path {
        let contents = fs::read_to_string(path).unwrap_or_default();
        if let Some(repository) = parse_local_repository(&contents) {
            return Some(repository);
        }
    }
    home.map(|home| {
        home.join(".m2")
            .join("repository")
            .to_string_lossy()
            .into_owned()
    })
}

fn parse_local_repository(contents: &str) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        Regex::new(r"<localRepository>([^<]*)</localRepository>").expect("local repository pattern")
    });
    let value = pattern.captures(contents)?.get(1)?.as_str().trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn validate_versions(
    portable: Option<&MavenPortableConfiguration>,
    local: Option<&MavenLocalConfiguration>,
) -> Result<(), String> {
    if portable.is_some_and(|value| value.version != MAVEN_CONFIGURATION_VERSION)
        || local.is_some_and(|value| value.version != MAVEN_CONFIGURATION_VERSION)
    {
        return Err(
            "The Maven configuration was created by an unsupported version of Lithe.".into(),
        );
    }
    Ok(())
}

fn portable_path(root: &Path) -> PathBuf {
    root.join(".lithe").join("maven").join("config.json")
}

fn local_path(app: &AppHandle, root: &Path, reactor_path: &str) -> Result<PathBuf, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    let mut digest = Sha256::new();
    let identity = storage_identity(&root.to_string_lossy(), reactor_path);
    digest.update(identity.as_bytes());
    Ok(app_data
        .join("maven")
        .join(format!("{:x}.json", digest.finalize())))
}

fn storage_identity(workspace_path: &str, reactor_path: &str) -> String {
    format!(
        "{}\0{}",
        workspace_path.to_lowercase(),
        reactor_path.replace('\\', "/")
    )
}

fn existing_directory(path: &Path) -> Result<PathBuf, String> {
    let root = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !root.is_dir() {
        return Err("The project directory is unavailable.".into());
    }
    Ok(root)
}

fn read_optional<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let contents = fs::read(path).map_err(|_| {
        format!(
            "Unable to read Maven configuration {}.",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("file")
        )
    })?;
    serde_json::from_slice(&contents).map(Some).map_err(|_| {
        format!(
            "The Maven configuration in {} is invalid.",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("file")
        )
    })
}

fn write_optional<T: Serialize>(path: &Path, value: Option<&T>) -> Result<(), String> {
    let Some(value) = value else {
        if path.is_file() {
            fs::remove_file(path).map_err(|error| error.to_string())?;
        }
        return Ok(());
    };
    let parent = path
        .parent()
        .ok_or_else(|| "Maven configuration path has no parent directory.".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut contents = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    contents.push('\n');
    atomic_write(path, contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_directory() -> PathBuf {
        static NEXT_DIRECTORY_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("lithe-maven-config-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("temp directory");
        path
    }

    #[test]
    fn parses_local_repository_element() {
        let contents = r#"<settings>
            <localRepository>D:\repo\maven</localRepository>
        </settings>"#;
        assert_eq!(
            parse_local_repository(contents).as_deref(),
            Some(r"D:\repo\maven")
        );
        assert_eq!(
            parse_local_repository("<localRepository>  </localRepository>"),
            None
        );
        assert_eq!(parse_local_repository("<settings></settings>"), None);
    }

    #[test]
    fn local_repository_prefers_configured_then_settings_then_default() {
        let home = temp_directory();
        let settings = home.join("user-settings.xml");
        fs::write(
            &settings,
            "<settings><localRepository>C:\\from-settings</localRepository></settings>",
        )
        .expect("write settings");

        assert_eq!(
            effective_local_repository(
                "D:\\custom",
                Some(&settings.to_string_lossy()),
                Some(&home)
            )
            .as_deref(),
            Some("D:\\custom")
        );
        assert_eq!(
            effective_local_repository("", Some(&settings.to_string_lossy()), Some(&home))
                .as_deref(),
            Some(r"C:\from-settings")
        );
        assert_eq!(
            effective_local_repository("", None, Some(&home)).as_deref(),
            Some(
                home.join(".m2")
                    .join("repository")
                    .to_string_lossy()
                    .to_string()
                    .as_str()
            )
        );
        assert_eq!(effective_local_repository("", None, None), None);
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn settings_path_prefers_configured_then_user_then_installation() {
        let home = temp_directory();
        let installation = temp_directory();
        let configured = home.join("configured.xml");
        fs::write(&configured, "<settings/>").expect("write settings");
        let executable = installation.join("bin").join("mvn.cmd");
        let global = installation.join("conf").join("settings.xml");
        let user_m2 = home.join(".m2");
        fs::create_dir_all(installation.join("conf")).expect("conf directory");
        let executable = executable.to_string_lossy().into_owned();

        // A configured path is reported even when the file is missing: Maven
        // itself would fail the launch, and the UI shows what was requested.
        assert_eq!(
            effective_settings_path(
                &configured.to_string_lossy(),
                Some(&home),
                Some(&executable)
            )
            .as_deref(),
            Some(configured.to_string_lossy().to_string().as_str())
        );

        // Neither default exists yet.
        assert_eq!(
            effective_settings_path("", Some(&home), Some(&executable)),
            None
        );

        // The user-level settings file wins over the installation-level one.
        fs::create_dir_all(&user_m2).expect("user m2");
        fs::write(user_m2.join("settings.xml"), "<settings/>").expect("user settings");
        assert_eq!(
            effective_settings_path("", Some(&home), Some(&executable)).as_deref(),
            Some(
                user_m2
                    .join("settings.xml")
                    .to_string_lossy()
                    .to_string()
                    .as_str()
            )
        );
        fs::write(&global, "<settings/>").expect("global settings");
        assert_eq!(
            effective_settings_path("", Some(&home), Some(&executable)).as_deref(),
            Some(
                user_m2
                    .join("settings.xml")
                    .to_string_lossy()
                    .to_string()
                    .as_str()
            )
        );

        // Once the user-level file is gone, the installation-level one applies.
        fs::remove_file(user_m2.join("settings.xml")).expect("remove user settings");
        assert_eq!(
            effective_settings_path("", Some(&home), Some(&executable)).as_deref(),
            Some(global.to_string_lossy().to_string().as_str())
        );

        fs::remove_dir_all(&home).ok();
        fs::remove_dir_all(&installation).ok();
    }

    #[test]
    fn portable_configuration_round_trips_without_local_paths() {
        let root = temp_directory();
        let path = portable_path(&root);
        let portable = MavenPortableConfiguration {
            version: 1,
            selected_profiles: vec!["dev".into(), "qa".into()],
            custom_profiles: vec!["qa".into()],
            skip_tests: true,
        };
        write_optional(&path, Some(&portable)).expect("write portable configuration");
        let loaded = read_optional::<MavenPortableConfiguration>(&path)
            .expect("read portable configuration")
            .expect("portable configuration");

        assert_eq!(loaded.selected_profiles, ["dev", "qa"]);
        assert!(loaded.skip_tests);
        let text = fs::read_to_string(&path).expect("portable text");
        assert!(!text.contains("settingsPath"));
        assert!(!text.contains("mavenExecutablePath"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rejects_unsupported_configuration_versions() {
        let portable = MavenPortableConfiguration {
            version: 2,
            selected_profiles: Vec::new(),
            custom_profiles: Vec::new(),
            skip_tests: false,
        };
        assert!(validate_versions(Some(&portable), None)
            .unwrap_err()
            .contains("unsupported version"));
    }

    #[test]
    fn windows_storage_identity_matches_shared_contract() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../shared/fixtures/maven/platform-contract-v1.json"
        )))
        .expect("Maven platform contract fixture");
        let cases = fixture["storageIdentityCases"]
            .as_array()
            .expect("storage identity cases");
        let windows_cases: Vec<_> = cases
            .iter()
            .filter(|item| item["platform"] == "windows")
            .collect();

        assert!(
            !windows_cases.is_empty(),
            "Windows fixture case is required"
        );
        for item in windows_cases {
            assert_eq!(
                storage_identity(
                    item["workspacePath"].as_str().expect("workspace path"),
                    item["reactorPath"].as_str().expect("reactor path"),
                ),
                item["expectedIdentity"]
                    .as_str()
                    .expect("expected identity")
            );
        }
    }
}
