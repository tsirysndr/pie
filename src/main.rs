//! pie — build official language runtimes from source with PIE enabled.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pie::{build, elf, recipe, ui};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "pie",
    version,
    about = "Build official language runtimes from source as position independent executables",
    long_about = "pie builds official releases of language runtimes from source with PIE \
                  (position independent executable) enabled.\n\n\
                  Each runtime is described by a YAML recipe that declares where its source \
                  is downloaded from, what has to be installed to build it, and how to build, \
                  verify and package it."
)]
struct Cli {
    /// Directory holding the recipe manifests.
    #[arg(long, global = true, default_value = "recipes", env = "PIE_RECIPES")]
    recipes: PathBuf,

    /// Stream every line of build output instead of a live summary.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build a runtime from its recipe.
    Build {
        /// Recipe id, e.g. `node`, `python`, `bun`.
        recipe: String,

        /// Version to build. Accepts an exact version, a series, or an alias
        /// such as `latest` or `lts`, and is always validated against the
        /// project's official release index.
        #[arg(long, default_value = "latest")]
        version: String,

        /// Target architecture (defaults to the host).
        #[arg(long, value_parser = ["x64", "arm64"])]
        arch: Option<String>,

        /// Where finished archives are written.
        #[arg(long, default_value = "dist")]
        output: PathBuf,

        /// Where source is unpacked and compiled.
        #[arg(long, default_value = "work")]
        work_dir: PathBuf,

        /// Enable an optional recipe feature, e.g. `--with lto --with pgo`.
        #[arg(long = "with", value_name = "FEATURE")]
        with: Vec<String>,

        /// Assume build dependencies are already installed.
        #[arg(long)]
        skip_dependencies: bool,

        /// Reuse an existing work directory instead of clearing it.
        #[arg(long)]
        keep_work: bool,

        /// Resolve the version and print the plan without building.
        #[arg(long)]
        dry_run: bool,
    },

    /// Resolve a version against the project's official release index and print
    /// it. Used by CI to name a release before the build starts.
    Resolve {
        /// Recipe id.
        recipe: String,

        /// Version request: an exact version, a series, or an alias.
        #[arg(long, default_value = "latest")]
        version: String,
    },

    /// Render the Pkl recipe sources to the YAML that `pie` reads.
    ///
    /// Pkl type checks the recipe on the way through, so a misspelled resolver
    /// or a source URL that ignores the resolved version fails here.
    Generate {
        /// Recipe ids to render. Defaults to every Pkl source in the directory.
        #[arg(value_name = "RECIPE")]
        names: Vec<String>,

        /// Report stale output instead of rewriting it, for CI.
        #[arg(long)]
        check: bool,
    },

    /// List the available recipes.
    List,

    /// Show what a recipe builds.
    Show {
        /// Recipe id.
        recipe: String,
    },

    /// Check whether binaries are position independent executables.
    Verify {
        /// Paths to ELF executables.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("\n  {} {err:#}\n", ui::brand::error("error"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Build {
            recipe,
            version,
            arch,
            output,
            work_dir,
            with,
            skip_dependencies,
            keep_work,
            dry_run,
        } => {
            let recipe_path = recipe::find(&cli.recipes, &recipe)?;
            let options = build::Options {
                recipe_path,
                version,
                arch: arch.unwrap_or_else(host_arch),
                output,
                work_dir,
                with,
                skip_dependencies,
                keep_work,
                dry_run,
                verbose: cli.verbose,
            };
            build::run(&options)
        }

        Command::Generate { names, check } => generate(&cli.recipes, &names, check),

        Command::Resolve { recipe, version } => {
            let path = recipe::find(&cli.recipes, &recipe)?;
            let recipe = recipe::Recipe::load(&path)?;
            let resolved = pie::resolve::resolve(&recipe.version, &version)?;
            // Bare on stdout so CI can capture it without any parsing.
            println!("{}", resolved.bare);
            Ok(())
        }

        Command::List => {
            let names = recipe::list(&cli.recipes)?;
            if names.is_empty() {
                println!("no recipes in {}", cli.recipes.display());
                return Ok(());
            }
            println!();
            for name in names {
                let path = recipe::find(&cli.recipes, &name)?;
                match recipe::Recipe::load(&path) {
                    Ok(recipe) => println!(
                        "  {} {}",
                        ui::brand::name(&format!("{:<10}", recipe.name)),
                        ui::brand::dim(recipe.description.as_deref().unwrap_or(""))
                    ),
                    Err(_) => println!(
                        "  {} {}",
                        ui::brand::error(&format!("{name:<10}")),
                        ui::brand::dim("(unreadable)")
                    ),
                }
            }
            println!();
            Ok(())
        }

        Command::Show { recipe } => {
            let path = recipe::find(&cli.recipes, &recipe)?;
            let recipe = recipe::Recipe::load(&path)?;
            show(&recipe, &path)
        }

        Command::Verify { paths } => {
            let mut failed = 0;
            println!();
            for path in &paths {
                match elf::inspect(path) {
                    Ok(info) if info.is_pie_executable() => {
                        println!(
                            "  {} {} {}",
                            ui::brand::ok("✔"),
                            path.display(),
                            ui::brand::dim(&format!("needs: {}", info.needed.join(", ")))
                        );
                    }
                    Ok(info) => {
                        failed += 1;
                        println!(
                            "  {} {} {}",
                            ui::brand::error("✖"),
                            path.display(),
                            ui::brand::dim(&info.explain_failure())
                        );
                    }
                    Err(err) => {
                        failed += 1;
                        println!("  {} {} {err:#}", ui::brand::error("✖"), path.display());
                    }
                }
            }
            println!();
            if failed > 0 {
                anyhow::bail!("{failed} of {} binaries are not PIE", paths.len());
            }
            Ok(())
        }
    }
}

/// Renders `<name>.pkl` to `<name>.yaml` beside it.
fn generate(dir: &std::path::Path, wanted: &[String], check: bool) -> Result<()> {
    let sources = if wanted.is_empty() {
        recipe::pkl_sources(dir)?
    } else {
        wanted
            .iter()
            .map(|name| {
                let path = dir.join(format!("{name}.pkl"));
                if path.is_file() {
                    Ok(path)
                } else {
                    anyhow::bail!("no Pkl source at {}", path.display())
                }
            })
            .collect::<Result<Vec<_>>>()?
    };

    if sources.is_empty() {
        anyhow::bail!("no .pkl recipes in {}", dir.display());
    }

    println!();
    let mut stale = Vec::new();

    for source in &sources {
        let stem = source
            .file_stem()
            .and_then(|s| s.to_str())
            .context("recipe path has no file name")?;
        let target = dir.join(format!("{stem}.yaml"));

        // pkl also type checks here: an invalid recipe never reaches the file.
        let body = recipe::render(source)?;
        let rendered = format!(
            "# Generated from {} by `pie generate` — do not edit.\n#\n{body}",
            source.display()
        );

        let current = std::fs::read_to_string(&target).unwrap_or_default();
        if current == rendered {
            println!(
                "  {} {}",
                ui::brand::ok("✔"),
                ui::brand::dim(&format!("{stem} up to date"))
            );
            continue;
        }

        if check {
            stale.push(stem.to_string());
            println!(
                "  {} {} {}",
                ui::brand::error("✖"),
                stem,
                ui::brand::dim("is stale")
            );
        } else {
            std::fs::write(&target, &rendered)
                .with_context(|| format!("writing {}", target.display()))?;
            println!(
                "  {} {}",
                ui::brand::ok("✔"),
                ui::brand::name(&format!("{stem} written"))
            );
        }
    }
    println!();

    if !stale.is_empty() {
        anyhow::bail!(
            "{} out of date with their Pkl sources: run `pie generate`",
            stale.join(", ")
        );
    }
    Ok(())
}

fn show(recipe: &recipe::Recipe, path: &std::path::Path) -> Result<()> {
    println!();
    println!("  {}", ui::brand::title(recipe.title()));
    if let Some(description) = &recipe.description {
        println!("  {}", ui::brand::dim(description));
    }
    println!();
    println!("  {} {}", ui::brand::label("recipe", 14), path.display());
    if let Some(homepage) = &recipe.homepage {
        println!("  {} {homepage}", ui::brand::label("homepage", 14));
    }
    match &recipe.source {
        Some(source) => println!("  {} {}", ui::brand::label("source", 14), source.url),
        None => println!(
            "  {} {}",
            ui::brand::label("source", 14),
            ui::brand::dim("fetched by the recipe's own build steps")
        ),
    }
    if !recipe.version.aliases.is_empty() {
        println!(
            "  {} {}",
            ui::brand::label("aliases", 14),
            recipe.version.aliases.join(", ")
        );
    }
    println!(
        "  {} {}.{}",
        ui::brand::label("artifact", 14),
        recipe.package.name,
        recipe.package.format.extension()
    );

    if !recipe.dependencies.apt.is_empty() {
        println!(
            "  {} {}",
            ui::brand::label("apt", 14),
            recipe.dependencies.apt.join(" ")
        );
    }
    if !recipe.dependencies.dnf.is_empty() {
        println!(
            "  {} {}",
            ui::brand::label("dnf", 14),
            recipe.dependencies.dnf.join(" ")
        );
    }

    println!("\n  {}", ui::brand::title("build"));
    for (index, step) in recipe.build.iter().enumerate() {
        let optional = step
            .when
            .as_ref()
            .map(|w| format!("  (only with --with {w})"))
            .unwrap_or_default();
        println!(
            "  {:>3}. {}{}",
            index + 1,
            ui::brand::name(&step.name),
            ui::brand::dim(&optional)
        );
    }

    if !recipe.verify.pie.is_empty() {
        println!("\n  {}", ui::brand::title("must be PIE"));
        for target in &recipe.verify.pie {
            println!("       {target}");
        }
    }

    if !recipe.notes.is_empty() {
        println!("\n  {}", ui::brand::title("notes"));
        for note in &recipe.notes {
            println!("       {note}");
        }
    }
    println!();
    Ok(())
}

fn host_arch() -> String {
    match std::env::consts::ARCH {
        "aarch64" => "arm64".to_string(),
        _ => "x64".to_string(),
    }
}

/// Present so `pie --help` stays honest if the binary is moved somewhere without
/// a recipe directory next to it.
#[allow(dead_code)]
fn recipes_hint(dir: &std::path::Path) -> Result<()> {
    dir.metadata()
        .map(|_| ())
        .with_context(|| format!("recipe directory {} not found", dir.display()))
}
