//! Version resolution.
//!
//! Every recipe validates the requested version against the project's own
//! release index before a single byte of source is downloaded, so an
//! unofficial or mistyped version fails immediately rather than 40 minutes into
//! a compile.

use crate::recipe::{Resolver, VersionSpec};
use crate::template::Vars;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::time::Duration;

pub struct Resolved {
    /// Canonical version as the project writes it (`v22.11.0`, `3.12.7`, `1.3.14`).
    pub version: String,
    /// Version without any leading `v`.
    pub bare: String,
    /// Upstream git tag, when the project is resolved from GitHub releases.
    pub upstream_tag: Option<String>,
    /// Digest of the source archive, when the release index publishes one.
    pub source_sha256: Option<String>,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .user_agent("pie (+https://github.com/tsiry-sndr/pie)")
        .build()
}

pub fn resolve(spec: &VersionSpec, request: &str) -> Result<Resolved> {
    match spec.resolver {
        Resolver::Nodejs => nodejs(spec, request),
        Resolver::Directory => directory(spec, request),
        Resolver::Github => github(spec, request),
        Resolver::Php => php(spec, request),
    }
}

// -- nodejs.org/dist/index.json ---------------------------------------------

#[derive(Deserialize)]
struct NodeRelease {
    version: String,
    #[serde(default)]
    lts: serde_json::Value,
}

fn nodejs(spec: &VersionSpec, request: &str) -> Result<Resolved> {
    let index = spec
        .index
        .as_deref()
        .context("recipe uses the nodejs resolver but sets no `version.index`")?;

    let releases: Vec<NodeRelease> = agent()
        .get(index)
        .call()
        .with_context(|| format!("fetching {index}"))?
        .into_json()
        .with_context(|| format!("parsing {index}"))?;

    if releases.is_empty() {
        bail!("{index} listed no releases");
    }

    let catalogue: Vec<(String, bool)> = releases
        .into_iter()
        .map(|r| {
            let is_lts = !matches!(r.lts, serde_json::Value::Bool(false));
            (r.version, is_lts)
        })
        .collect();

    let version = select_node(&catalogue, request).with_context(|| format!("see {index}"))?;

    Ok(Resolved {
        bare: version.trim_start_matches('v').to_string(),
        version,
        upstream_tag: None,
        source_sha256: None,
    })
}

/// Picks a Node.js version out of the release index. Split out from the HTTP
/// call so the alias rules are testable without a network.
pub fn select_node(releases: &[(String, bool)], request: &str) -> Result<String> {
    if releases.is_empty() {
        bail!("the release index listed no versions");
    }
    let request = request.trim_start_matches("node-");
    match request {
        "latest" => Ok(releases[0].0.clone()),
        "lts" => releases
            .iter()
            .find(|(_, is_lts)| *is_lts)
            .map(|(version, _)| version.clone())
            .context("no LTS release found in the index"),
        other => {
            let wanted = format!("v{}", other.trim_start_matches('v'));
            if !releases.iter().any(|(version, _)| *version == wanted) {
                bail!("'{wanted}' is not an official Node.js release");
            }
            Ok(wanted)
        }
    }
}

// -- directory listing (python.org, postgresql.org, archive.mariadb.org) ------

fn directory(spec: &VersionSpec, request: &str) -> Result<Resolved> {
    let listing = spec
        .listing
        .as_deref()
        .context("recipe uses the directory resolver but sets no `version.listing`")?;
    let probe = spec
        .probe
        .as_deref()
        .context("recipe uses the directory resolver but sets no `version.probe`")?;
    let prefix = spec.dir_prefix.as_deref().unwrap_or("");

    let html = agent()
        .get(listing)
        .call()
        .with_context(|| format!("fetching {listing}"))?
        .into_string()?;

    let candidates = scrape_version_dirs(&html, prefix);
    if candidates.is_empty() {
        bail!("no version directories found in {listing}");
    }

    let request = request.trim_start_matches(prefix).trim_start_matches('v');
    let shortlist = shortlist(candidates, request);
    if shortlist.is_empty() {
        bail!("'{request}' does not match any published release (see {listing})");
    }

    // A directory can exist while the final source archive does not: release
    // candidates often live in the same directory as the eventual release.
    let http = agent();
    for candidate in shortlist.iter().take(20) {
        let mut vars = Vars::new();
        vars.set("candidate", candidate);
        let url = vars.expand(probe)?;
        if http.head(&url).call().is_ok() {
            return Ok(Resolved {
                bare: candidate.clone(),
                version: candidate.clone(),
                upstream_tag: None,
                source_sha256: None,
            });
        }
    }

    bail!("no published source archive found for '{request}'")
}

/// Sort key for a dotted numeric version, padded so `1.10` sorts above `1.9`.
pub fn version_key(version: &str) -> [u32; 4] {
    let mut key = [0u32; 4];
    for (slot, part) in key.iter_mut().zip(version.split('.')) {
        *slot = part.parse().unwrap_or(0);
    }
    key
}

/// True for versions made only of dotted numbers, which excludes the alpha, rc
/// and `-old` directories that these archives are full of.
fn is_plain_version(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Narrows a descending list of versions to `latest`, a series prefix such as
/// `3.12` or `17`, or an exact version.
pub fn shortlist(candidates: Vec<String>, request: &str) -> Vec<String> {
    if request == "latest" {
        return candidates;
    }
    candidates
        .into_iter()
        .filter(|candidate| candidate == request || candidate.starts_with(&format!("{request}.")))
        .collect()
}

/// Pulls `href="3.12.7/"` (or `href="v17.2/"`, `href="mariadb-11.4.4/"`) out of
/// an autoindex page, newest first.
pub fn scrape_version_dirs(html: &str, prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for chunk in html.split("href=\"").skip(1) {
        let Some(end) = chunk.find('"') else { continue };
        let Some(name) = chunk[..end].strip_suffix('/') else {
            continue;
        };
        // Ignore absolute or parent links; only same-level directories count.
        let Some(name) = name.rsplit('/').next() else {
            continue;
        };
        let Some(version) = name.strip_prefix(prefix) else {
            continue;
        };
        if is_plain_version(version) {
            out.push(version.to_string());
        }
    }
    out.sort_by_key(|version| std::cmp::Reverse(version_key(version)));
    out.dedup();
    out
}

// -- GitHub releases ---------------------------------------------------------

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

fn github(spec: &VersionSpec, request: &str) -> Result<Resolved> {
    let repo = spec
        .repo
        .as_deref()
        .context("recipe uses the github resolver but sets no `version.repo`")?;
    let prefix = spec.tag_prefix.as_deref().unwrap_or("v");
    let http = agent();

    let builder = |url: String| {
        let mut req = http.get(&url).set("Accept", "application/vnd.github+json");
        // Anonymous GitHub API access is rate limited to 60 requests an hour,
        // which CI blows through; use the token when one is present.
        if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
            if !token.is_empty() {
                req = req.set("Authorization", &format!("Bearer {token}"));
            }
        }
        req
    };

    let request = request.trim_start_matches(prefix).trim_start_matches('v');

    if spec.tags {
        let tag = github_tag(repo, prefix, request, &builder)?;
        let bare = tag
            .trim_start_matches(prefix)
            .trim_start_matches('v')
            .to_string();
        return Ok(Resolved {
            version: bare.clone(),
            bare,
            upstream_tag: Some(tag),
            source_sha256: None,
        });
    }

    let tag = if request == "latest" {
        let url = format!("https://api.github.com/repos/{repo}/releases/latest");
        let release: GithubRelease = builder(url.clone())
            .call()
            .with_context(|| format!("fetching {url}"))?
            .into_json()?;
        release.tag_name
    } else {
        let wanted = format!("{prefix}{request}");
        let url = format!("https://api.github.com/repos/{repo}/releases/tags/{wanted}");
        let release: GithubRelease = builder(url)
            .call()
            .with_context(|| {
                format!("'{wanted}' is not an official release of {repo} (see https://github.com/{repo}/releases)")
            })?
            .into_json()?;
        if release.draft {
            bail!("'{wanted}' is a draft release of {repo}");
        }
        if release.prerelease {
            eprintln!("note: {wanted} is marked as a prerelease upstream");
        }
        release.tag_name
    };

    let bare = tag
        .trim_start_matches(prefix)
        .trim_start_matches('v')
        .to_string();

    Ok(Resolved {
        version: bare.clone(),
        bare,
        upstream_tag: Some(tag),
        source_sha256: None,
    })
}

// -- php.net release index ---------------------------------------------------

fn php(spec: &VersionSpec, request: &str) -> Result<Resolved> {
    let listing = spec
        .listing
        .as_deref()
        .context("recipe uses the php resolver but sets no `version.listing`")?;

    // Keyed by version; each release lists its source archives with digests.
    let index: serde_json::Map<String, serde_json::Value> = agent()
        .get(listing)
        .call()
        .with_context(|| format!("fetching {listing}"))?
        .into_json()
        .with_context(|| format!("parsing {listing}"))?;

    let (version, source_sha256) =
        select_php(&index, request).with_context(|| format!("see {listing}"))?;

    Ok(Resolved {
        bare: version.clone(),
        version,
        upstream_tag: None,
        source_sha256,
    })
}

/// Resolves against a repository's tags rather than its releases. Some projects
/// (MongoDB among them) tag every release but publish no GitHub Release object,
/// so `/releases/latest` returns a 404 for them.
fn github_tag<F>(repo: &str, prefix: &str, request: &str, builder: &F) -> Result<String>
where
    F: Fn(String) -> ureq::Request,
{
    if request != "latest" {
        let tag = format!("{prefix}{request}");
        let url = format!("https://api.github.com/repos/{repo}/git/ref/tags/{tag}");
        builder(url).call().with_context(|| {
            format!("'{tag}' is not a tag of {repo} (see https://github.com/{repo}/tags)")
        })?;
        return Ok(tag);
    }

    // The tags endpoint is not ordered by version, so several pages are pulled
    // and sorted rather than trusting the first result.
    let mut names: Vec<String> = Vec::new();
    for page in 1..=3 {
        let url = format!("https://api.github.com/repos/{repo}/tags?per_page=100&page={page}");
        let tags: Vec<serde_json::Value> = builder(url.clone())
            .call()
            .with_context(|| format!("fetching {url}"))?
            .into_json()?;
        if tags.is_empty() {
            break;
        }
        for tag in tags {
            if let Some(name) = tag.get("name").and_then(|n| n.as_str()) {
                if let Some(version) = name.strip_prefix(prefix) {
                    if is_plain_version(version) {
                        names.push(version.to_string());
                    }
                }
            }
        }
    }

    names.sort_by_key(|version| std::cmp::Reverse(version_key(version)));
    names.dedup();

    let newest = names
        .first()
        .with_context(|| format!("no version tags found on {repo} with prefix '{prefix}'"))?;
    Ok(format!("{prefix}{newest}"))
}

/// Picks a PHP version out of the php.net release index and returns the
/// published digest of its `.tar.xz` source archive alongside it.
pub fn select_php(
    index: &serde_json::Map<String, serde_json::Value>,
    request: &str,
) -> Result<(String, Option<String>)> {
    let request = request.trim_start_matches("php-").trim_start_matches('v');

    let mut candidates: Vec<String> = index
        .keys()
        .filter(|key| is_plain_version(key))
        .cloned()
        .collect();
    candidates.sort_by_key(|version| std::cmp::Reverse(version_key(version)));

    let shortlist = shortlist(candidates, request);
    let Some(version) = shortlist.first().cloned() else {
        bail!("'{request}' does not match any official PHP release");
    };

    // php.net publishes a sha256 per source archive, so the download can be
    // verified against the same document that resolved the version.
    let digest = index
        .get(&version)
        .and_then(|release| release.get("source"))
        .and_then(|source| source.as_array())
        .and_then(|files| {
            files.iter().find_map(|file| {
                let name = file.get("filename")?.as_str()?;
                if !name.ends_with(".tar.xz") {
                    return None;
                }
                file.get("sha256")?.as_str().map(str::to_owned)
            })
        });

    Ok((version, digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrapes_version_directories_newest_first() {
        let html = r#"
            <a href="../">../</a>
            <a href="3.12.7/">3.12.7/</a>
            <a href="3.13.0/">3.13.0/</a>
            <a href="3.9.20/">3.9.20/</a>
            <a href="3.12.0a1/">3.12.0a1/</a>
            <a href="README.txt">README</a>
            <a href="doc/">doc/</a>
        "#;
        // Newest first, prereleases excluded, and 3.12 must outrank 3.9.
        assert_eq!(
            scrape_version_dirs(html, ""),
            vec!["3.13.0", "3.12.7", "3.9.20"]
        );
    }

    #[test]
    fn scrapes_prefixed_directories() {
        let html =
            r#"<a href="v17.2/">v17.2/</a><a href="v16.6/">v16.6/</a><a href="README/">x</a>"#;
        assert_eq!(scrape_version_dirs(html, "v"), vec!["17.2", "16.6"]);
        // Without the prefix declared, nothing matches.
        assert!(scrape_version_dirs(html, "").is_empty());
    }

    #[test]
    fn scrapes_name_prefixed_directories() {
        let html = r#"<a href="mariadb-11.4.4/">x</a><a href="mariadb-11.5.0-rc/">x</a>"#;
        assert_eq!(scrape_version_dirs(html, "mariadb-"), vec!["11.4.4"]);
    }

    #[test]
    fn version_key_orders_numerically_not_lexically() {
        assert!(version_key("1.10.0") > version_key("1.9.0"));
        assert!(version_key("17.2") > version_key("9.6.24"));
        assert_eq!(version_key("3.12"), [3, 12, 0, 0]);
    }

    fn node_catalogue() -> Vec<(String, bool)> {
        // Ordered newest first, exactly as nodejs.org/dist/index.json is.
        vec![
            ("v23.3.0".to_string(), false),
            ("v22.11.0".to_string(), true),
            ("v20.18.0".to_string(), true),
        ]
    }

    #[test]
    fn node_latest_takes_the_newest_release() {
        assert_eq!(select_node(&node_catalogue(), "latest").unwrap(), "v23.3.0");
    }

    #[test]
    fn node_lts_skips_current() {
        // v23.3.0 is newer but not LTS, so it must not be chosen.
        assert_eq!(select_node(&node_catalogue(), "lts").unwrap(), "v22.11.0");
    }

    #[test]
    fn node_accepts_every_spelling_of_a_version() {
        for request in ["22.11.0", "v22.11.0", "node-v22.11.0"] {
            assert_eq!(
                select_node(&node_catalogue(), request).unwrap(),
                "v22.11.0",
                "failed for {request}"
            );
        }
    }

    #[test]
    fn node_rejects_an_unpublished_version() {
        let err = select_node(&node_catalogue(), "22.11.1").unwrap_err();
        assert!(err.to_string().contains("not an official"));
    }

    #[test]
    fn node_rejects_an_empty_index() {
        assert!(select_node(&[], "latest").is_err());
    }

    fn php_index() -> serde_json::Map<String, serde_json::Value> {
        let raw = serde_json::json!({
            "8.4.2": {
                "source": [
                    {"filename": "php-8.4.2.tar.gz", "sha256": "gz-digest"},
                    {"filename": "php-8.4.2.tar.xz", "sha256": "xz-digest"}
                ]
            },
            "8.3.15": {
                "source": [{"filename": "php-8.3.15.tar.xz", "sha256": "older-digest"}]
            },
            "8.3.14": {
                "source": [{"filename": "php-8.3.14.tar.xz", "sha256": "oldest-digest"}]
            }
        });
        raw.as_object().unwrap().clone()
    }

    #[test]
    fn php_latest_picks_the_highest_version_not_the_first_key() {
        let (version, digest) = select_php(&php_index(), "latest").unwrap();
        assert_eq!(version, "8.4.2");
        // The .tar.xz digest, not the .tar.gz one that precedes it.
        assert_eq!(digest.as_deref(), Some("xz-digest"));
    }

    #[test]
    fn php_series_picks_the_newest_patch_in_that_series() {
        let (version, digest) = select_php(&php_index(), "8.3").unwrap();
        assert_eq!(version, "8.3.15");
        assert_eq!(digest.as_deref(), Some("older-digest"));
    }

    #[test]
    fn php_exact_version_is_honoured() {
        let (version, _) = select_php(&php_index(), "8.3.14").unwrap();
        assert_eq!(version, "8.3.14");
    }

    #[test]
    fn php_rejects_an_unpublished_version() {
        assert!(select_php(&php_index(), "8.9.9").is_err());
        assert!(select_php(&php_index(), "not-a-version").is_err());
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn shortlist_filters_and_preserves_order() {
        let candidates = strings(&["3.13.1", "3.13.0", "3.12.7", "3.12.1"]);
        assert_eq!(shortlist(candidates.clone(), "latest").len(), 4);
        assert_eq!(
            shortlist(candidates.clone(), "3.12"),
            strings(&["3.12.7", "3.12.1"])
        );
        assert_eq!(
            shortlist(candidates.clone(), "3.13.0"),
            strings(&["3.13.0"])
        );
        assert!(shortlist(candidates, "rubbish").is_empty());
    }

    /// "3.1" must not match "3.12.7"; the series filter is dot-delimited.
    #[test]
    fn shortlist_does_not_match_partial_components() {
        let candidates = strings(&["3.12.7", "3.1.5"]);
        assert_eq!(shortlist(candidates, "3.1"), strings(&["3.1.5"]));
    }

    #[test]
    fn shortlist_handles_two_component_versions() {
        let candidates = strings(&["17.2", "17.1", "16.6"]);
        assert_eq!(
            shortlist(candidates.clone(), "17"),
            strings(&["17.2", "17.1"])
        );
        assert_eq!(shortlist(candidates, "17.1"), strings(&["17.1"]));
    }

    #[test]
    fn recognises_plain_versions_only() {
        assert!(is_plain_version("3.12.7"));
        assert!(is_plain_version("17.2"));
        assert!(is_plain_version("1.2.3.4"));
        assert!(!is_plain_version("8"));
        assert!(!is_plain_version("3.12.0rc1"));
        assert!(!is_plain_version("11.5.0-rc"));
        assert!(!is_plain_version("latest"));
        assert!(!is_plain_version("3..1"));
    }
}
