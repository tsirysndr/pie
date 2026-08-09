//! Recipe format: a YAML manifest describing *what* to build.
//!
//! A recipe answers three questions, in order:
//!   1. which versions are official, and how to resolve the one that was asked for
//!   2. what has to be installed before the build, and where the source comes from
//!   3. how to configure, compile, verify and package it

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Extensions a recipe may use, in precedence order: a `.pkl` next to a `.yaml`
/// wins, because the `.pkl` is the source and the `.yaml` is its output.
pub const EXTENSIONS: [&str; 3] = ["pkl", "yaml", "yml"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// Short id used on the command line, e.g. `node`.
    pub name: String,
    /// Human readable name, e.g. `Node.js`.
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,

    /// How `--version` is resolved and validated against the official release index.
    pub version: VersionSpec,

    /// Extra template variables, expanded after the built-ins.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,

    /// Environment applied to every build step.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Packages to install before building, per package manager.
    #[serde(default)]
    pub dependencies: Dependencies,

    /// Where the source archive comes from and how it is verified. Optional:
    /// a project whose build needs a git checkout (submodules, generated files)
    /// fetches its own source in the first build step instead.
    #[serde(default)]
    pub source: Option<Source>,

    /// How to build it.
    pub build: Vec<Step>,

    /// What must be a position independent executable when the build finishes.
    #[serde(default)]
    pub verify: Verify,

    /// How to turn the build tree into a distributable archive.
    pub package: Package,

    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionSpec {
    pub resolver: Resolver,
    /// nodejs: URL of the release index JSON.
    #[serde(default)]
    pub index: Option<String>,
    /// directory/php: URL of the listing of published releases.
    #[serde(default)]
    pub listing: Option<String>,
    /// directory: templated URL (`{{candidate}}`) that must exist for a candidate to count.
    #[serde(default)]
    pub probe: Option<String>,
    /// directory: prefix on each directory name, e.g. `v` or `mariadb-`.
    #[serde(default)]
    pub dir_prefix: Option<String>,
    /// github: `owner/repo`.
    #[serde(default)]
    pub repo: Option<String>,
    /// github: tag prefix to strip and re-add, e.g. `bun-v`.
    #[serde(default)]
    pub tag_prefix: Option<String>,
    /// github: resolve against tags instead of releases, for projects that tag
    /// releases without publishing a GitHub Release object.
    #[serde(default)]
    pub tags: bool,
    /// Aliases accepted in addition to an exact version, e.g. `lts`.
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Resolver {
    /// nodejs.org/dist/index.json
    Nodejs,
    /// An autoindex directory listing (python.org, postgresql.org, mariadb.org)
    Directory,
    /// GitHub releases API
    Github,
    /// php.net release index (which also publishes per-file digests)
    Php,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependencies {
    #[serde(default)]
    pub apt: Vec<String>,
    #[serde(default)]
    pub dnf: Vec<String>,
    /// Free-form steps run after the package manager, e.g. installing a toolchain
    /// that is not packaged.
    #[serde(default)]
    pub extra: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Templated URL of the source archive.
    pub url: String,
    /// Directory (relative to the work dir) to unpack into.
    #[serde(default = "default_src_dir")]
    pub dir: String,
    #[serde(default = "default_strip_components")]
    pub strip_components: u32,
    #[serde(default)]
    pub checksum: Option<Checksum>,
}

fn default_src_dir() -> String {
    "src".into()
}
fn default_strip_components() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checksum {
    pub kind: ChecksumKind,
    /// For `sha256-manifest`: templated URL of a `sha256  filename` list.
    #[serde(default)]
    pub url: Option<String>,
    /// For `sha256`: the expected digest.
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChecksumKind {
    /// A digest given inline in the recipe.
    Sha256,
    /// A `SHASUMS256.txt`-style manifest listing many files.
    Sha256Manifest,
    /// The digest published by the project's own release index.
    FromIndex,
    /// No published checksum: record the digest but do not gate on it.
    Record,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    /// Shell script, run with `bash -euo pipefail`.
    pub run: String,
    /// Working directory, relative to the work dir.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Step-local environment, layered over the recipe environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Skip unless this build was asked for the named option (`--with <opt>`).
    #[serde(default)]
    pub when: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verify {
    /// Paths in the build tree that must be real position independent executables.
    #[serde(default)]
    pub pie: Vec<String>,
    /// Paths checked again after the staging install, when they exist.
    #[serde(default)]
    pub packaged: Vec<String>,
    /// Paths whose only allowed `DT_NEEDED` entries are `dynamic_allowlist`.
    #[serde(default)]
    pub self_contained: Vec<String>,
    #[serde(default)]
    pub dynamic_allowlist: Vec<String>,
    /// Smoke tests, run after the artifact is unpacked to a fresh prefix.
    #[serde(default)]
    pub smoke: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    /// Templated archive name without extension.
    pub name: String,
    /// Steps that populate `{{stage}}`.
    pub steps: Vec<Step>,
    pub format: Format,
    /// Directory created inside the archive. Defaults to the package name.
    #[serde(default)]
    pub inner_dir: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[serde(rename = "tar.xz")]
    TarXz,
    #[serde(rename = "tar.gz")]
    TarGz,
    Zip,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::TarXz => "tar.xz",
            Format::TarGz => "tar.gz",
            Format::Zip => "zip",
        }
    }
}

impl Recipe {
    pub fn load(path: &Path) -> Result<Self> {
        let text = render(path)?;
        let recipe: Recipe = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing recipe {}", path.display()))?;
        Ok(recipe)
    }

    pub fn title(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

/// Reads a recipe as YAML, evaluating it with the `pkl` CLI first when it is a
/// Pkl source. Type and constraint errors in the schema surface here, before
/// anything is downloaded or built.
pub fn render(path: &Path) -> Result<String> {
    if path.extension().and_then(|e| e.to_str()) != Some("pkl") {
        return std::fs::read_to_string(path)
            .with_context(|| format!("reading recipe {}", path.display()));
    }

    let output = Command::new("pkl")
        .args(["eval", "-f", "yaml"])
        .arg(path)
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "{} is a Pkl recipe but the `pkl` CLI is not installed \
                     (see https://pkl-lang.org/main/current/pkl-cli/), or build the \
                     generated {}.yaml instead",
                    path.display(),
                    path.file_stem().unwrap_or_default().to_string_lossy()
                )
            } else {
                anyhow::anyhow!("running pkl: {err}")
            }
        })?;

    if !output.status.success() {
        bail!(
            "pkl rejected {}:\n{}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    String::from_utf8(output.stdout).context("pkl produced invalid UTF-8")
}

/// Finds `<dir>/<name>.pkl`, `.yaml` or `.yml`, in that order.
pub fn find(dir: &Path, name: &str) -> Result<PathBuf> {
    for ext in EXTENSIONS {
        let candidate = dir.join(format!("{name}.{ext}"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let available = list(dir).unwrap_or_default();
    if available.is_empty() {
        bail!("no recipe named '{name}' in {}", dir.display());
    }
    bail!(
        "no recipe named '{name}' in {} (available: {})",
        dir.display(),
        available.join(", ")
    );
}

/// Lists the recipe ids in a directory, sorted and deduplicated: a recipe with
/// both a `.pkl` source and its generated `.yaml` is still one recipe.
pub fn list(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading recipe directory {}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        let known = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| EXTENSIONS.contains(&e))
            .unwrap_or(false);
        if known {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Lists the Pkl sources in a directory, sorted.
pub fn pkl_sources(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading recipe directory {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("pkl"))
        .collect();
    paths.sort();
    Ok(paths)
}
