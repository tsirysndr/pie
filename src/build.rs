//! The build pipeline: resolve → install dependencies → download source →
//! build → verify → package.

use crate::elf;
use crate::recipe::{Checksum, ChecksumKind, Format, Recipe, Step};
use crate::resolve;
use crate::runner;
use crate::template::Vars;
use crate::ui::{self, Reporter};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Every template variable a recipe may use. Declared next to `base_vars` so
/// the two cannot drift, and asserted against it by the tests below; the recipe
/// lint test uses it to reject typo'd variables before a build ever starts.
pub const TEMPLATE_VARIABLES: &[&str] = &[
    "name",
    "version",
    "version_bare",
    "version_major",
    "version_minor",
    "version_xy",
    "upstream_tag",
    "os",
    "arch",
    "arch_gnu",
    "nproc",
    "workdir",
    "srcdir",
    "dist",
    "deps_prefix",
    "repo_root",
    "sudo",
    // Set once the package name is known, and after the artifact is unpacked.
    "package",
    "stage",
    "extracted",
];

pub struct Options {
    pub recipe_path: PathBuf,
    pub version: String,
    pub arch: String,
    pub output: PathBuf,
    pub work_dir: PathBuf,
    pub with: Vec<String>,
    pub skip_dependencies: bool,
    pub keep_work: bool,
    pub dry_run: bool,
    pub verbose: bool,
}

pub fn run(opts: &Options) -> Result<()> {
    let started = Instant::now();
    let recipe = Recipe::load(&opts.recipe_path)?;
    let reporter = Reporter::new(opts.verbose);

    reporter.heading(
        &format!("pie · {}", recipe.title()),
        recipe.description.as_deref().unwrap_or(""),
    );

    // 1. Resolve the requested version against the project's official index.
    let resolved = {
        let step = reporter.step(&format!("Resolve version '{}'", opts.version));
        match resolve::resolve(&recipe.version, &opts.version) {
            Ok(resolved) => {
                step.say(&format!("resolved to {}", resolved.version));
                step.finish();
                resolved
            }
            Err(err) => {
                step.fail();
                return Err(err);
            }
        }
    };

    let work = opts
        .work_dir
        .join(format!("{}-{}-{}", recipe.name, resolved.bare, opts.arch));
    let dist = opts.output.clone();
    let src = work.join(recipe.source.as_ref().map_or("src", |s| s.dir.as_str()));
    let deps_prefix = work.join("deps-prefix");

    let mut vars = base_vars(&recipe, &resolved, opts, &work, &dist, &src, &deps_prefix)?;
    let package_name = vars.expand(&recipe.package.name)?;
    let inner_dir = match &recipe.package.inner_dir {
        Some(dir) => vars.expand(dir)?,
        None => package_name.clone(),
    };
    let stage = work.join("stage");
    vars.set("package", &package_name);
    vars.set("stage", stage.join(&inner_dir).to_string_lossy());

    reporter.info("recipe", &opts.recipe_path.display().to_string());
    reporter.info("version", &resolved.version);
    reporter.info("target", &format!("linux-{}", opts.arch));
    reporter.info(
        "artifact",
        &format!("{package_name}.{}", recipe.package.format.extension()),
    );
    reporter.info("work dir", &work.display().to_string());
    println!();

    if opts.dry_run {
        return dry_run(&recipe, &vars, opts);
    }

    if !opts.keep_work && work.exists() {
        std::fs::remove_dir_all(&work).with_context(|| format!("clearing {}", work.display()))?;
    }
    std::fs::create_dir_all(&work)?;
    std::fs::create_dir_all(&dist)?;
    std::fs::create_dir_all(stage.join(&inner_dir))?;

    let env = environment(&recipe, &vars)?;

    // 2. Build dependencies.
    if !opts.skip_dependencies {
        install_dependencies(&recipe, &vars, &env, &work, &reporter)?;
    }
    for step in &recipe.dependencies.extra {
        run_step(step, &vars, &env, &work, &reporter, &opts.with)?;
    }

    // 3. Source: where it comes from, and whether it is what it claims to be.
    if let Some(source) = &recipe.source {
        fetch_source(
            source,
            &vars,
            &work,
            &src,
            &reporter,
            resolved.source_sha256.as_deref(),
        )?;
    } else {
        std::fs::create_dir_all(&src)?;
    }

    // 4. Build.
    for step in &recipe.build {
        run_step(step, &vars, &env, &work, &reporter, &opts.with)?;
    }

    // 5. Verify PIE in the build tree, before spending time on packaging.
    verify_pie(&recipe.verify.pie, &vars, &work, &reporter)?;

    // 6. Package.
    for step in &recipe.package.steps {
        run_step(step, &vars, &env, &work, &reporter, &opts.with)?;
    }
    verify_pie(&recipe.verify.packaged, &vars, &work, &reporter)?;
    verify_self_contained(&recipe, &vars, &work, &reporter)?;

    let archive = create_archive(&recipe, &package_name, &inner_dir, &stage, &dist, &reporter)?;

    // 7. Smoke test the artifact from a prefix it was never built in.
    if !recipe.verify.smoke.is_empty() {
        let extracted = extract_for_verification(&recipe, &archive, &work, &inner_dir, &reporter)?;
        vars.set("extracted", extracted.to_string_lossy());
        for step in &recipe.verify.smoke {
            run_step(step, &vars, &env, &work, &reporter, &opts.with)?;
        }
    }

    let size = std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
    reporter.success(&format!(
        "{} {} built in {}",
        recipe.title(),
        resolved.version,
        ui::format_duration(started.elapsed())
    ));
    println!("  {} ({})", archive.display(), ui::format_bytes(size));
    println!("  {}.sha256\n", archive.display());

    for note in &recipe.notes {
        reporter.note(note);
    }

    Ok(())
}

fn dry_run(recipe: &Recipe, vars: &Vars, opts: &Options) -> Result<()> {
    println!("  {}", console::style("plan").cyan().bold());
    let mut n = 0;
    let mut show = |name: &str| {
        n += 1;
        println!("  {n:>3}. {name}");
    };
    if !opts.skip_dependencies {
        show("Install build dependencies");
    }
    for step in &recipe.dependencies.extra {
        show(&step.name);
    }
    if recipe.source.is_some() {
        show("Download source");
    }
    for step in &recipe.build {
        if step_enabled(step, &opts.with) {
            show(&step.name);
        }
    }
    show("Verify PIE");
    for step in &recipe.package.steps {
        if step_enabled(step, &opts.with) {
            show(&step.name);
        }
    }
    show("Create archive");
    for step in &recipe.verify.smoke {
        show(&step.name);
    }
    println!("\n  {}", console::style("source").cyan().bold());
    match &recipe.source {
        Some(source) => println!("  {}", vars.expand(&source.url)?),
        None => println!("  (fetched by the recipe's own build steps)"),
    }
    println!();
    Ok(())
}

fn base_vars(
    recipe: &Recipe,
    resolved: &resolve::Resolved,
    opts: &Options,
    work: &Path,
    dist: &Path,
    src: &Path,
    deps_prefix: &Path,
) -> Result<Vars> {
    let mut vars = Vars::new();

    let numbers: Vec<&str> = resolved.bare.split('.').collect();
    vars.set("name", &recipe.name)
        .set("version", &resolved.version)
        .set("version_bare", &resolved.bare)
        .set("version_major", numbers.first().copied().unwrap_or(""))
        .set("version_minor", numbers.get(1).copied().unwrap_or(""))
        .set(
            "version_xy",
            match (numbers.first(), numbers.get(1)) {
                (Some(a), Some(b)) => format!("{a}.{b}"),
                _ => resolved.bare.clone(),
            },
        )
        .set(
            "upstream_tag",
            resolved.upstream_tag.clone().unwrap_or_default(),
        )
        .set("os", "linux")
        .set("arch", &opts.arch)
        .set("arch_gnu", gnu_arch(&opts.arch))
        .set("nproc", nproc().to_string())
        .set("workdir", work.to_string_lossy())
        .set("srcdir", src.to_string_lossy())
        .set("dist", dist.to_string_lossy())
        .set("deps_prefix", deps_prefix.to_string_lossy())
        .set("repo_root", repo_root().to_string_lossy())
        .set("sudo", if is_root() { "" } else { "sudo " });

    // Recipe variables may build on the built-ins, and on each other in order.
    let snapshot: Vec<(String, String)> = recipe
        .vars
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (key, value) in snapshot {
        let expanded = vars.expand(&value)?;
        vars.set(key, expanded);
    }

    Ok(vars)
}

fn environment(recipe: &Recipe, vars: &Vars) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for (key, value) in &recipe.env {
        env.insert(key.clone(), vars.expand(value)?);
    }
    Ok(env)
}

fn step_enabled(step: &Step, with: &[String]) -> bool {
    match &step.when {
        Some(option) => with.iter().any(|w| w == option),
        None => true,
    }
}

fn run_step(
    step: &Step,
    vars: &Vars,
    env: &BTreeMap<String, String>,
    work: &Path,
    reporter: &Reporter,
    with: &[String],
) -> Result<()> {
    if !step_enabled(step, with) {
        return Ok(());
    }

    let name = vars.expand(&step.name)?;
    let script = vars.expand(&step.run)?;
    let cwd = match &step.cwd {
        Some(dir) => work.join(vars.expand(dir)?),
        None => work.to_path_buf(),
    };

    let mut env = env.clone();
    for (key, value) in &step.env {
        env.insert(key.clone(), vars.expand(value)?);
    }
    // Recipes address paths through variables; exporting them keeps scripts that
    // call out to helper shell files working without re-templating.
    for (key, value) in vars.iter() {
        env.insert(format!("PIE_{}", key.to_uppercase()), value.clone());
    }
    // Optional features are readable from the script itself, so a recipe can
    // vary configure arguments without needing a separate step per combination.
    env.insert("PIE_WITH".to_string(), with.join(" "));

    let mut ui_step = reporter.step(&name);
    match runner::shell(&script, &cwd, &env, &mut ui_step) {
        Ok(()) => {
            ui_step.finish();
            Ok(())
        }
        Err(err) => {
            ui_step.fail();
            Err(err).with_context(|| format!("step '{name}' failed"))
        }
    }
}

fn install_dependencies(
    recipe: &Recipe,
    vars: &Vars,
    env: &BTreeMap<String, String>,
    work: &Path,
    reporter: &Reporter,
) -> Result<()> {
    let deps = &recipe.dependencies;
    let sudo = vars.get("sudo").unwrap_or("");

    let script = if has_binary("apt-get") && !deps.apt.is_empty() {
        format!(
            "{sudo}apt-get update\n{sudo}apt-get install -y --no-install-recommends {}",
            deps.apt.join(" ")
        )
    } else if (has_binary("dnf") || has_binary("yum")) && !deps.dnf.is_empty() {
        let manager = if has_binary("dnf") { "dnf" } else { "yum" };
        format!("{sudo}{manager} install -y {}", deps.dnf.join(" "))
    } else {
        reporter
            .note("no matching package manager or no packages declared — skipping dependencies");
        return Ok(());
    };

    let step = Step {
        name: "Install build dependencies".into(),
        run: script,
        cwd: None,
        env: BTreeMap::new(),
        when: None,
    };
    run_step(&step, vars, env, work, reporter, &[])
}

fn fetch_source(
    source: &crate::recipe::Source,
    vars: &Vars,
    work: &Path,
    src: &Path,
    reporter: &Reporter,
    index_digest: Option<&str>,
) -> Result<()> {
    let url = vars.expand(&source.url)?;
    let filename = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .context("source url has no filename")?
        .to_string();
    let downloads = work.join("downloads");
    std::fs::create_dir_all(&downloads)?;
    let archive = downloads.join(&filename);

    let mut step = reporter.step(&format!("Download {filename}"));
    step.say(&url);
    if let Err(err) = download(&url, &archive, &mut step) {
        step.fail();
        return Err(err).with_context(|| format!("downloading {url}"));
    }

    let digest = match sha256_file(&archive) {
        Ok(digest) => digest,
        Err(err) => {
            step.fail();
            return Err(err);
        }
    };

    match verify_checksum(
        source.checksum.as_ref(),
        vars,
        &filename,
        &digest,
        index_digest,
    ) {
        Ok(message) => step.say(&message),
        Err(err) => {
            step.fail();
            return Err(err);
        }
    }
    step.finish();

    // Extraction streams, so it gets its own step rather than hiding inside the
    // download: for CPython or Node this is tens of thousands of files.
    let strip = source.strip_components;
    std::fs::create_dir_all(src)?;
    let script = if filename.ends_with(".zip") {
        format!(
            "unzip -q {archive:?} -d {src:?}",
            archive = archive.display().to_string(),
            src = src.display().to_string()
        )
    } else {
        format!(
            "tar -xf {archive:?} -C {src:?} --strip-components={strip}",
            archive = archive.display().to_string(),
            src = src.display().to_string()
        )
    };

    let step = Step {
        name: format!("Unpack {filename}"),
        run: script,
        cwd: None,
        env: BTreeMap::new(),
        when: None,
    };
    run_step(&step, vars, &BTreeMap::new(), work, reporter, &[])
}

fn verify_checksum(
    checksum: Option<&Checksum>,
    vars: &Vars,
    filename: &str,
    digest: &str,
    index_digest: Option<&str>,
) -> Result<String> {
    let Some(checksum) = checksum else {
        return Ok(format!("sha256 {digest} (unverified)"));
    };

    match checksum.kind {
        ChecksumKind::Record => Ok(format!("sha256 {digest} (recorded, no published checksum)")),
        ChecksumKind::FromIndex => {
            let expected = index_digest
                .context("checksum kind 'from-index' needs a resolver that publishes digests")?;
            if expected != digest {
                bail!("checksum mismatch against the release index: expected {expected}, got {digest}");
            }
            Ok(format!(
                "sha256 {digest} verified against the release index"
            ))
        }
        ChecksumKind::Sha256 => {
            let expected = checksum
                .value
                .as_deref()
                .context("checksum kind 'sha256' needs a `value`")?;
            if expected != digest {
                bail!("checksum mismatch: expected {expected}, got {digest}");
            }
            Ok(format!("sha256 {digest} verified"))
        }
        ChecksumKind::Sha256Manifest => {
            let url = vars.expand(
                checksum
                    .url
                    .as_deref()
                    .context("checksum kind 'sha256-manifest' needs a `url`")?,
            )?;
            let manifest = ureq::get(&url)
                .call()
                .with_context(|| format!("fetching {url}"))?
                .into_string()?;

            let expected = manifest
                .lines()
                .filter_map(|line| {
                    let mut parts = line.split_whitespace();
                    let hash = parts.next()?;
                    // Manifests spell the name as "file", "*file" (binary mode)
                    // or "./file" depending on how they were generated.
                    let name = parts
                        .next()?
                        .trim_start_matches('*')
                        .trim_start_matches("./");
                    (name == filename).then_some(hash)
                })
                .next()
                .with_context(|| format!("{filename} is not listed in {url}"))?;

            if expected != digest {
                bail!("checksum mismatch against {url}: expected {expected}, got {digest}");
            }
            Ok(format!("sha256 {digest} verified against {url}"))
        }
    }
}

fn download(url: &str, dest: &Path, step: &mut crate::ui::Step) -> Result<()> {
    let response = ureq::get(url).call()?;
    let total: u64 = response
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut reader = response.into_reader();
    let mut file = std::fs::File::create(dest)?;
    let mut buffer = vec![0u8; 128 * 1024];
    let mut written: u64 = 0;
    let mut last_report = 0u64;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        written += read as u64;

        // Report roughly every 4 MiB so the spinner moves without flooding CI logs.
        if written - last_report >= 4 * 1024 * 1024 {
            last_report = written;
            step.line(&match total {
                0 => format!("downloaded {}", ui::format_bytes(written)),
                total => format!(
                    "downloaded {} / {} ({}%)",
                    ui::format_bytes(written),
                    ui::format_bytes(total),
                    written * 100 / total
                ),
            });
        }
    }

    file.flush()?;
    if total > 0 && written != total {
        bail!("truncated download: got {} of {} bytes", written, total);
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn verify_pie(paths: &[String], vars: &Vars, work: &Path, reporter: &Reporter) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let step = reporter.step("Verify PIE");

    for path in paths {
        let path = work.join(vars.expand(path)?);
        if !path.exists() {
            step.fail();
            bail!("{} does not exist", path.display());
        }
        let info = match elf::inspect(&path) {
            Ok(info) => info,
            Err(err) => {
                step.fail();
                return Err(err);
            }
        };
        if !info.is_pie_executable() {
            step.say(&format!("{}: {}", path.display(), info.explain_failure()));
            step.fail();
            bail!(
                "{} is not a position independent executable",
                path.display()
            );
        }
        step.say(&format!(
            "{} — ELF DYN, DT_FLAGS_1 PIE, PT_INTERP present",
            path.display()
        ));
    }

    step.finish();
    Ok(())
}

fn verify_self_contained(
    recipe: &Recipe,
    vars: &Vars,
    work: &Path,
    reporter: &Reporter,
) -> Result<()> {
    let verify = &recipe.verify;
    if verify.self_contained.is_empty() {
        return Ok(());
    }
    let step = reporter.step("Verify no third-party shared libraries");

    for path in &verify.self_contained {
        let path = work.join(vars.expand(path)?);
        let info = match elf::inspect(&path) {
            Ok(info) => info,
            Err(err) => {
                step.fail();
                return Err(err);
            }
        };

        let unexpected: Vec<&String> = info
            .needed
            .iter()
            .filter(|lib| {
                !verify
                    .dynamic_allowlist
                    .iter()
                    .any(|allowed| lib.as_str() == allowed || lib.starts_with(allowed))
            })
            .collect();

        if !unexpected.is_empty() {
            step.say(&format!(
                "{} links {} which should have been static",
                path.display(),
                unexpected
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            step.fail();
            bail!("{} is not self-contained", path.display());
        }

        step.say(&format!(
            "{} needs only {}",
            path.display(),
            info.needed.join(", ")
        ));
    }

    step.finish();
    Ok(())
}

fn create_archive(
    recipe: &Recipe,
    package_name: &str,
    inner_dir: &str,
    stage: &Path,
    dist: &Path,
    reporter: &Reporter,
) -> Result<PathBuf> {
    let extension = recipe.package.format.extension();
    let archive = dist.join(format!("{package_name}.{extension}"));
    let archive_display = archive.display().to_string();
    let stage_display = stage.display().to_string();

    let script = match recipe.package.format {
        Format::TarXz => format!("tar -C '{stage_display}' -cJf '{archive_display}' '{inner_dir}'"),
        Format::TarGz => format!("tar -C '{stage_display}' -czf '{archive_display}' '{inner_dir}'"),
        Format::Zip => format!(
            "cd '{stage_display}' && rm -f '{archive_display}' && zip -qr '{archive_display}' '{inner_dir}'"
        ),
    };

    let step = Step {
        name: format!("Create {package_name}.{extension}"),
        run: script,
        cwd: None,
        env: BTreeMap::new(),
        when: None,
    };
    run_step(&step, &Vars::new(), &BTreeMap::new(), dist, reporter, &[])?;

    let digest = sha256_file(&archive)?;
    std::fs::write(
        dist.join(format!("{package_name}.{extension}.sha256")),
        format!("{digest}  {package_name}.{extension}\n"),
    )?;

    Ok(archive)
}

fn extract_for_verification(
    recipe: &Recipe,
    archive: &Path,
    work: &Path,
    inner_dir: &str,
    reporter: &Reporter,
) -> Result<PathBuf> {
    // Unpacking somewhere unrelated to the build tree is what actually proves
    // the artifact is relocatable rather than pinned to its staging path.
    let target = work.join("verify");
    if target.exists() {
        std::fs::remove_dir_all(&target)?;
    }
    std::fs::create_dir_all(&target)?;

    let archive_display = archive.display().to_string();
    let target_display = target.display().to_string();
    let script = match recipe.package.format {
        Format::Zip => format!("unzip -q '{archive_display}' -d '{target_display}'"),
        _ => format!("tar -C '{target_display}' -xf '{archive_display}'"),
    };

    let step = Step {
        name: "Unpack artifact to a fresh prefix".into(),
        run: script,
        cwd: None,
        env: BTreeMap::new(),
        when: None,
    };
    run_step(&step, &Vars::new(), &BTreeMap::new(), work, reporter, &[])?;

    Ok(target.join(inner_dir))
}

fn gnu_arch(arch: &str) -> &'static str {
    match arch {
        "arm64" | "aarch64" => "aarch64",
        _ => "x86_64",
    }
}

fn nproc() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn is_root() -> bool {
    runner::capture("id -u", Path::new("."), &BTreeMap::new())
        .map(|uid| uid.trim() == "0")
        .unwrap_or(false)
}

fn repo_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn has_binary(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_options() -> Options {
        Options {
            recipe_path: PathBuf::from("recipes/node.pkl"),
            version: "latest".into(),
            arch: "x64".into(),
            output: PathBuf::from("dist"),
            work_dir: PathBuf::from("work"),
            with: vec![],
            skip_dependencies: false,
            keep_work: false,
            dry_run: true,
            verbose: false,
        }
    }

    fn sample_resolved() -> resolve::Resolved {
        resolve::Resolved {
            version: "v22.11.0".into(),
            bare: "22.11.0".into(),
            upstream_tag: None,
            source_sha256: None,
        }
    }

    /// The declared variable list and what `base_vars` actually sets must agree,
    /// or the recipe lint would pass on a variable that does not exist at build
    /// time (or reject one that does).
    #[test]
    fn declared_variables_match_the_ones_provided() {
        let recipe = Recipe::load(Path::new("recipes/node.pkl")).expect("recipe loads");
        let opts = sample_options();
        let vars = base_vars(
            &recipe,
            &sample_resolved(),
            &opts,
            Path::new("/w"),
            Path::new("/d"),
            Path::new("/w/src"),
            Path::new("/w/deps"),
        )
        .expect("vars build");

        // Everything except the three set later in the pipeline.
        let deferred = ["package", "stage", "extracted"];
        for name in TEMPLATE_VARIABLES {
            if deferred.contains(name) {
                continue;
            }
            assert!(
                vars.get(name).is_some(),
                "TEMPLATE_VARIABLES lists '{name}' but base_vars does not set it"
            );
        }

        for (key, _) in vars.iter() {
            // Recipe-declared vars are legitimately extra.
            if recipe.vars.contains_key(key) {
                continue;
            }
            assert!(
                TEMPLATE_VARIABLES.contains(&key.as_str()),
                "base_vars sets '{key}' but TEMPLATE_VARIABLES does not list it"
            );
        }
    }

    #[test]
    fn derives_version_components() {
        let recipe = Recipe::load(Path::new("recipes/node.pkl")).expect("recipe loads");
        let vars = base_vars(
            &recipe,
            &sample_resolved(),
            &sample_options(),
            Path::new("/w"),
            Path::new("/d"),
            Path::new("/w/src"),
            Path::new("/w/deps"),
        )
        .expect("vars build");

        assert_eq!(vars.get("version"), Some("v22.11.0"));
        assert_eq!(vars.get("version_bare"), Some("22.11.0"));
        assert_eq!(vars.get("version_xy"), Some("22.11"));
        assert_eq!(vars.get("version_major"), Some("22"));
        assert_eq!(vars.get("arch_gnu"), Some("x86_64"));
        // Absent upstream tag must be empty, never the literal "None".
        assert_eq!(vars.get("upstream_tag"), Some(""));
    }

    #[test]
    fn maps_arch_names() {
        assert_eq!(gnu_arch("x64"), "x86_64");
        assert_eq!(gnu_arch("arm64"), "aarch64");
    }

    #[test]
    fn hexes_bytes() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn inline_checksum_is_enforced() {
        let checksum = Checksum {
            kind: ChecksumKind::Sha256,
            url: None,
            value: Some("abc".into()),
        };
        assert!(verify_checksum(Some(&checksum), &Vars::new(), "f.tar", "abc", None).is_ok());
        assert!(verify_checksum(Some(&checksum), &Vars::new(), "f.tar", "def", None).is_err());
    }

    #[test]
    fn recorded_checksum_never_gates() {
        let checksum = Checksum {
            kind: ChecksumKind::Record,
            url: None,
            value: None,
        };
        assert!(verify_checksum(Some(&checksum), &Vars::new(), "f.tar", "anything", None).is_ok());
    }

    #[test]
    fn when_gates_steps() {
        let step = Step {
            name: "LTO".into(),
            run: "true".into(),
            cwd: None,
            env: BTreeMap::new(),
            when: Some("lto".into()),
        };
        assert!(!step_enabled(&step, &[]));
        assert!(step_enabled(&step, &["lto".to_string()]));
    }
}
