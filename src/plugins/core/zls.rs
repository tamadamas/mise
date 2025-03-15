use std::path::{Path, PathBuf};

use crate::backend::Backend;
use crate::cli::args::BackendArg;
use crate::cli::version::OS;
use crate::cmd::CmdLineRunner;
use crate::config::SETTINGS;
use crate::file::TarOptions;
use crate::http::{HTTP, HTTP_FETCH};
use crate::install_context::InstallContext;
use crate::toolset::{ToolRequest, ToolVersion};
use crate::ui::progress_report::SingleReport;
use crate::{file, github, minisign, plugins};
use contracts::requires;
use eyre::Result;
use itertools::Itertools;
use versions::Versioning;
use xx::regex;

#[derive(Debug)]
pub struct ZlsPlugin {
    ba: BackendArg,
}

const MINISIGN_KEY: &str = "RWR+9B91GBZ0zOjh6Lr17+zKf5BoSuFvrx2xSeDE57uIYvnKBGmMjOex";

impl ZlsPlugin {
    pub fn new() -> Self {
        Self {
            ba: plugins::core::new_backend_arg("zls"),
        }
    }

    fn bin_path(&self, bin_name: &str, tv: &ToolVersion) -> PathBuf {
        if cfg!(windows) {
            tv.install_path().join(bin_name + ".exe")
        } else {
            tv.install_path().join("bin").join(bin_name)
        }
    }
   
    fn bin_version(&self, bin_name: &str, ctx: &InstallContext, tv: &ToolVersion) -> Result<String> {
        ctx.pr.set_message((bin_name.to_owned() + " version").into());
        let output = CmdLineRunner::new(self.bin_path(bin_name, tv))
            .with_pr(&ctx.pr)
            .arg("version")
            .output()?;
            
        // Extract version from output
        let version = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
            
        Ok(version)
    }

    fn download(&self, ctx: &InstallContext, tv: &ToolVersion) -> Result<PathBuf> {
        let zls_version = if tv.version == "ref:zig" {
            // Get the installed Zig version using mise
            let zig_version = ctx.toolset
                .get_tool("zig")?
                .current_version()?
                .ok_or_else(|| eyre::eyre!("Zig is not installed"))?
                .version
                .clone();
            zig_version
        } else {
            tv.version.clone()
        };

        let url = self.fetch_url_from_zigtools(&zls_version)?;

        let filename = url.split('/').last().unwrap();
        let tarball_path = tv.download_path().join(filename);

        ctx.pr.set_message(format!("download {filename}"));
        HTTP.download_file(&url, &tarball_path, Some(&ctx.pr))?;

        ctx.pr.set_message(format!("minisign {filename}"));
        let tarball_data = file::read(&tarball_path)?;
        let sig = HTTP.get_text(format!("{url}.minisig"))?;
        minisign::verify(MINISIGN_KEY, &tarball_data, &sig)?;

        Ok(tarball_path)
    }

    fn install(&self, ctx: &InstallContext, tv: &ToolVersion, tarball_path: &Path) -> Result<()> {
        let filename = tarball_path.file_name().unwrap().to_string_lossy();
        ctx.pr.set_message(format!("extract {filename}"));
        file::remove_all(tv.install_path())?;
        file::untar(
            tarball_path,
            &tv.install_path(),
            &TarOptions {
                strip_components: 1,
                pr: Some(&ctx.pr),
                ..Default::default()
            },
        )?;

        if cfg!(unix) {
            file::create_dir_all(tv.install_path().join("bin"))?;
            file::make_symlink(Path::new("../zls"), &tv.install_path().join("bin/zls"))?;
        }

        Ok(())
    }

    fn verify(&self, ctx: &InstallContext, tv: &ToolVersion) -> Result<()> {
        let version = self.bin_version("zls", ctx, tv)?;
        ctx.pr.set_message(format!("verified zls {}", version));
        Ok(())
    }

    fn fetch_url_from_zigtools(&self, zls_version: &str) -> Result<String> {
        let json_url = format!("https://releases.zigtools.org/v1/zls/select-version?zig_version={}&compatibility=only-runtime", zls_version);

        let version_json: serde_json::Value = HTTP_FETCH.json(json_url)?;
        
        // Check if there's an error code in the response
        if let Some(code) = version_json.get("code") {
            let message = version_json["message"].as_str().unwrap_or("Unknown error");
            return Err(eyre::eyre!("ZLS API error (code {}): {}", code, message));
        }
        
        // Get the appropriate tarball URL based on OS and architecture
        let os_key = format!("{}-{}", os(), arch());
        
        if let Some(platform) = version_json.get(&os_key) {
            if let Some(tarball) = platform.get("tarball") {
                if let Some(url) = tarball.as_str() {
                    return Ok(url.to_string());
                }
            }
        }
        
        Err(eyre::eyre!("No compatible ZLS build found for {} on {}", zls_version, os_key))
    }
}

impl Backend for ZlsPlugin {
    fn ba(&self) -> &BackendArg {
        &self.ba
    }

    fn list_remote_versions(&self) -> Result<Vec<String>> {
        let mut versions: Vec<String> = github::list_releases("zigtools/zls")?
            .into_iter()
            .map(|r| r.tag_name)
            .unique()
            .sorted_by_cached_key(|s| (Versioning::new(s), s.to_string()))
            .collect();
            
        // Add special "ref:zig" version that matches installed Zig version
        versions.push("ref:zig".to_string());
        
        Ok(versions)
    }

    fn list_bin_paths(&self, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        if cfg!(windows) {
            Ok(vec![tv.install_path()])
        } else {
            Ok(vec![tv.install_path().join("bin")])
        }
    }

    fn idiomatic_install_path(&self, _tv: &ToolVersion) -> Result<()> {
        Ok(())
    }

    #[requires(matches!(tv.request, ToolRequest::Version { .. } | ToolRequest::Prefix { .. } | ToolRequest::Ref { .. }), "unsupported tool version request type")]
    fn install_version_(&self, ctx: &InstallContext, tv: ToolVersion) -> Result<ToolVersion> {
        let tarball_path = self.download(ctx, &tv)?;
        self.install(ctx, &tv, &tarball_path)?;
        self.verify(ctx, &tv)?;
        Ok(tv)
    }
}

fn os() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "freebsd") {
        "freebsd"
    } else {
        &OS
    }
}

fn arch() -> &'static str {
    let arch = SETTINGS.arch();
    if arch == "x86_64" {
        "x86_64"
    } else if arch == "aarch64" {
        "aarch64"
    } else if arch == "arm" {
        "armv7a"
    } else if arch == "riscv64" {
        "riscv64"
    } else {
        arch
    }
}
