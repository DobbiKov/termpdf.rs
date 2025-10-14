use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use tar::Archive;
use ureq::{AgentBuilder, Error as UreqError};
use walkdir::WalkDir;
use zip::read::ZipArchive;

const DEFAULT_PDFIUM_VERSION: &str = "7350";
const DEFAULT_RELEASE_PREFIX: &str = "chromium";
const DEFAULT_BASE_URL: &str = "https://github.com/bblanchon/pdfium-binaries/releases/download";

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TERMPDF_PDFIUM_SKIP_DOWNLOAD");
    println!("cargo:rerun-if-env-changed=TERMPDF_PDFIUM_ARCHIVE_PATH");
    println!("cargo:rerun-if-env-changed=TERMPDF_PDFIUM_VERSION");
    println!("cargo:rerun-if-env-changed=TERMPDF_PDFIUM_RELEASE_TAG");
    println!("cargo:rerun-if-env-changed=TERMPDF_PDFIUM_PLATFORM");
    println!("cargo:rerun-if-env-changed=TERMPDF_PDFIUM_BASE_URL");
    println!("cargo:rerun-if-env-changed=TERMPDF_FORCE_DOWNLOAD");
    println!("cargo:rerun-if-env-changed=PDFIUM_DYNAMIC_LIB_PATH");
    println!("cargo:rerun-if-env-changed=PDFIUM_STATIC_LIB_PATH");

    if env::var_os("TERMPDF_PDFIUM_SKIP_DOWNLOAD").is_some() {
        return Ok(());
    }

    if env::var_os("PDFIUM_DYNAMIC_LIB_PATH").is_some()
        || env::var_os("PDFIUM_STATIC_LIB_PATH").is_some()
    {
        // Delegate to user-provided library locations.
        return Ok(());
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").context("OUT_DIR env var not set")?);
    let staging_dir = out_dir.join("pdfium");
    fs::create_dir_all(&staging_dir).context("failed to create staging directory")?;

    let target_os =
        env::var("CARGO_CFG_TARGET_OS").context("CARGO_CFG_TARGET_OS env var missing")?;
    let target_arch =
        env::var("CARGO_CFG_TARGET_ARCH").context("CARGO_CFG_TARGET_ARCH env var missing")?;
    let platform = env::var("TERMPDF_PDFIUM_PLATFORM")
        .unwrap_or_else(|_| default_platform(&target_os, &target_arch));

    if let Ok(path) = locate_library(&staging_dir, &target_os) {
        write_rustc_env(&path)?;
        return Ok(());
    }

    let archive_path = if let Some(path) = env::var_os("TERMPDF_PDFIUM_ARCHIVE_PATH") {
        PathBuf::from(path)
    } else {
        download_pdfium(&staging_dir, &platform)?
    };

    extract_archive(&archive_path, &staging_dir)?;

    let library_path = locate_library(&staging_dir, &target_os).with_context(|| {
        format!(
            "Pdfium library not found in {:?} after extraction",
            staging_dir
        )
    })?;

    write_rustc_env(&library_path)?;

    Ok(())
}

fn write_rustc_env(path: &Path) -> Result<()> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("failed to convert library path {:?} to UTF-8", path))?;
    println!("cargo:rustc-env=TERMPDF_PDFIUM_LIBRARY_PATH={}", path_str);
    Ok(())
}

fn default_platform(target_os: &str, target_arch: &str) -> String {
    match (target_os, target_arch) {
        ("macos", "aarch64") => "mac-arm64".to_string(),
        ("macos", "x86_64") => "mac-x64".to_string(),
        ("linux", "aarch64") => "linux-arm64".to_string(),
        ("linux", "arm") => "linux-arm".to_string(),
        ("linux", "x86_64") => "linux-x64".to_string(),
        ("windows", "aarch64") => "windows-arm64".to_string(),
        ("windows", "x86_64") => "windows-x64".to_string(),
        ("windows", "x86") => "windows-x86".to_string(),
        (other_os, other_arch) => format!("{}-{}", other_os, other_arch),
    }
}

fn library_filenames(target_os: &str) -> &'static [&'static str] {
    match target_os {
        "windows" => &["pdfium.dll"],
        "macos" => &["libpdfium.dylib"],
        _ => &["libpdfium.so"],
    }
}

fn locate_library(root: &Path, target_os: &str) -> Result<PathBuf> {
    let candidates = library_filenames(target_os);

    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if entry.metadata().map(|m| m.is_file()).unwrap_or(false) {
            let file_name = entry.file_name().to_string_lossy();
            if candidates
                .iter()
                .any(|candidate| candidate == &file_name.as_ref())
            {
                return Ok(entry.into_path());
            }
        }
    }

    Err(anyhow!("Pdfium library not found for target {target_os}"))
}

fn download_pdfium(staging_dir: &Path, platform: &str) -> Result<PathBuf> {
    let version =
        env::var("TERMPDF_PDFIUM_VERSION").unwrap_or_else(|_| DEFAULT_PDFIUM_VERSION.to_string());
    let release_tag = env::var("TERMPDF_PDFIUM_RELEASE_TAG")
        .unwrap_or_else(|_| format!("{}/{}", DEFAULT_RELEASE_PREFIX, version));
    let base_url =
        env::var("TERMPDF_PDFIUM_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());

    let download_dir = staging_dir.join("downloads");
    fs::create_dir_all(&download_dir).context("failed to create download cache directory")?;

    let file_candidates = candidate_filenames(&version, platform);
    let mut last_error = None;

    for filename in file_candidates {
        let archive_path = download_dir.join(&filename);

        if archive_path.exists() && env::var_os("TERMPDF_FORCE_DOWNLOAD").is_none() {
            return Ok(archive_path);
        }

        let url = format!(
            "{}/{}/{}",
            base_url.trim_end_matches('/'),
            release_tag.trim_matches('/'),
            filename
        );
        match try_download(&url, &archive_path) {
            Ok(_) => return Ok(archive_path),
            Err(err) => {
                last_error = Some(err);
            }
        }
    }

    Err(anyhow!(
        "failed to download Pdfium for platform {platform} (version {version}); last error: {}",
        last_error
            .map(|err| err.to_string())
            .unwrap_or_else(|| "no candidates succeeded".to_string())
    ))
}

fn candidate_filenames(version: &str, platform: &str) -> Vec<String> {
    vec![
        format!("pdfium-{}.tgz", platform),
        format!("pdfium-{}-{}.tgz", version, platform),
        format!("pdfium-{}.zip", platform),
        format!("pdfium-{}-{}.zip", version, platform),
    ]
}

fn try_download(url: &str, destination: &Path) -> Result<()> {
    let agent = AgentBuilder::new()
        .timeout_read(Duration::from_secs(120))
        .timeout_write(Duration::from_secs(120))
        .build();

    let response = match agent.get(url).call() {
        Ok(response) => response,
        Err(UreqError::Status(code, _)) => {
            return Err(anyhow!("GET {} failed with HTTP status {}", url, code));
        }
        Err(err) => {
            return Err(anyhow!("GET {} failed: {}", url, err));
        }
    };

    let mut reader = response.into_reader();
    let mut file =
        File::create(destination).with_context(|| format!("failed to create {:?}", destination))?;
    io::copy(&mut reader, &mut file)
        .with_context(|| format!("failed to write downloaded data to {:?}", destination))?;
    file.flush().ok();

    Ok(())
}

fn extract_archive(archive: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        // Remove previous extraction while keeping the downloads cache folder intact.
        for entry in fs::read_dir(destination)? {
            let entry = entry?;
            if entry.file_name() == "downloads" {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                fs::remove_dir_all(&path).with_context(|| {
                    format!("failed to remove old extracted directory {:?}", path)
                })?;
            } else {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove old extracted file {:?}", path))?;
            }
        }
    } else {
        fs::create_dir_all(destination)
            .with_context(|| format!("failed to create extraction directory {:?}", destination))?;
    }

    let extension = archive
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if extension == "tgz" || extension == "gz" {
        let file =
            File::open(archive).with_context(|| format!("failed to open archive {:?}", archive))?;
        let decoder = GzDecoder::new(file);
        let mut tar = Archive::new(decoder);
        tar.unpack(destination)
            .with_context(|| format!("failed to unpack {:?}", archive))?;
    } else if extension == "zip" {
        let file =
            File::open(archive).with_context(|| format!("failed to open archive {:?}", archive))?;
        let mut archive = ZipArchive::new(file)
            .with_context(|| format!("failed to read zip archive {:?}", archive))?;
        archive
            .extract(destination)
            .with_context(|| format!("failed to extract {:?}", archive))?;
    } else {
        return Err(anyhow!("unsupported archive format for {:?}", archive));
    }

    Ok(())
}
