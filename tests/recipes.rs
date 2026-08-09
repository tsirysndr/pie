//! Lints every recipe shipped in this repository.
//!
//! These run in CI before any build does, so a typo'd template variable or a
//! recipe that forgets to verify its output fails in seconds rather than after
//! an hour of compiling.

use pie::build::TEMPLATE_VARIABLES;
use pie::recipe::{self, Recipe};
use pie::template::Vars;
use std::path::{Path, PathBuf};

fn recipes_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("recipes")
}

fn all_recipes() -> Vec<(String, PathBuf, Recipe)> {
    let dir = recipes_dir();
    recipe::list(&dir)
        .expect("recipes directory is readable")
        .into_iter()
        .map(|name| {
            let path = recipe::find(&dir, &name).expect("recipe resolves");
            let recipe = Recipe::load(&path)
                .unwrap_or_else(|err| panic!("{} does not parse: {err:#}", path.display()));
            (name, path, recipe)
        })
        .collect()
}

/// Variables with plausible values, so expansion exercises the real strings.
fn sample_vars(recipe: &Recipe) -> Vars {
    let mut vars = Vars::new();
    for name in TEMPLATE_VARIABLES {
        vars.set(*name, format!("<{name}>"));
    }
    // The resolver-specific candidate placeholder is only used inside `probe`.
    vars.set("candidate", "<candidate>");
    for key in recipe.vars.keys() {
        vars.set(key, format!("<{key}>"));
    }
    vars
}

/// Walks every templated string in a recipe.
fn templated_strings(recipe: &Recipe) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut push = |label: &str, value: &str| out.push((label.to_string(), value.to_string()));

    if let Some(source) = &recipe.source {
        push("source.url", &source.url);
        if let Some(checksum) = &source.checksum {
            if let Some(url) = &checksum.url {
                push("source.checksum.url", url);
            }
        }
    }
    if let Some(probe) = &recipe.version.probe {
        push("version.probe", probe);
    }
    for (key, value) in &recipe.vars {
        push(&format!("vars.{key}"), value);
    }
    for (key, value) in &recipe.env {
        push(&format!("env.{key}"), value);
    }
    push("package.name", &recipe.package.name);
    if let Some(inner) = &recipe.package.inner_dir {
        push("package.inner_dir", inner);
    }
    for target in &recipe.verify.pie {
        push("verify.pie", target);
    }
    for target in &recipe.verify.packaged {
        push("verify.packaged", target);
    }
    for target in &recipe.verify.self_contained {
        push("verify.self_contained", target);
    }

    let step_groups = [
        ("dependencies.extra", &recipe.dependencies.extra),
        ("build", &recipe.build),
        ("package.steps", &recipe.package.steps),
        ("verify.smoke", &recipe.verify.smoke),
    ];
    for (label, steps) in step_groups {
        for step in steps {
            push(&format!("{label}[{}].name", step.name), &step.name);
            push(&format!("{label}[{}].run", step.name), &step.run);
            if let Some(cwd) = &step.cwd {
                push(&format!("{label}[{}].cwd", step.name), cwd);
            }
            for value in step.env.values() {
                push(&format!("{label}[{}].env", step.name), value);
            }
        }
    }
    out
}

/// Pkl is the only committed source; the YAML beside it is generated output and
/// is gitignored, so a recipe with no `.pkl` could not be reproduced.
#[test]
fn every_recipe_has_a_pkl_source() {
    let dir = recipes_dir();
    for (name, _, _) in all_recipes() {
        assert!(
            dir.join(format!("{name}.pkl")).is_file(),
            "{name} has no Pkl source in recipes/"
        );
    }
    assert_eq!(
        recipe::pkl_sources(&dir)
            .expect("sources are listable")
            .len(),
        recipe::list(&dir).expect("recipes are listable").len(),
        "every recipe should have exactly one Pkl source"
    );
}

/// A stale generated `.yaml` sitting next to its `.pkl` must never win, or a
/// build would silently use output that no longer matches its source.
#[test]
fn lookup_prefers_pkl_over_generated_yaml() {
    let dir = std::env::temp_dir().join("pie-test-precedence");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(dir.join("demo.pkl"), "// source").expect("write pkl");
    std::fs::write(dir.join("demo.yaml"), "# generated").expect("write yaml");

    let found = recipe::find(&dir, "demo").expect("recipe resolves");
    assert_eq!(found.extension().and_then(|e| e.to_str()), Some("pkl"));

    // And the pair counts as one recipe, not two.
    assert_eq!(
        recipe::list(&dir).expect("listable"),
        vec!["demo".to_string()]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_recipe_parses() {
    let recipes = all_recipes();
    assert!(!recipes.is_empty(), "no recipes found");
}

#[test]
fn recipe_name_matches_its_filename() {
    for (file_stem, path, recipe) in all_recipes() {
        assert_eq!(
            recipe.name,
            file_stem,
            "{} declares name '{}' but is called '{file_stem}.yaml'",
            path.display(),
            recipe.name
        );
    }
}

/// The important one: a template variable that does not exist would expand to a
/// hard error mid-build, after the source has already been downloaded.
#[test]
fn every_template_variable_is_known() {
    for (name, _, recipe) in all_recipes() {
        let vars = sample_vars(&recipe);
        for (label, value) in templated_strings(&recipe) {
            if let Err(err) = vars.expand(&value) {
                panic!("{name}: {label} uses an unknown variable: {err:#}");
            }
        }
    }
}

#[test]
fn every_recipe_verifies_a_pie_binary() {
    for (name, _, recipe) in all_recipes() {
        assert!(
            !recipe.verify.pie.is_empty(),
            "{name} declares no verify.pie targets, so nothing would enforce that the build is PIE"
        );
    }
}

#[test]
fn every_recipe_has_build_and_package_steps() {
    for (name, _, recipe) in all_recipes() {
        assert!(!recipe.build.is_empty(), "{name} has no build steps");
        assert!(
            !recipe.package.steps.is_empty(),
            "{name} has no package steps, so the staging tree would be empty"
        );
    }
}

/// A source URL that ignores the resolved version would silently build the
/// wrong thing for every requested version.
#[test]
fn source_url_depends_on_the_version() {
    for (name, _, recipe) in all_recipes() {
        let Some(source) = &recipe.source else {
            continue; // fetches its own source; covered by the helper-script test
        };
        assert!(
            source.url.contains("{{version") || source.url.contains("{{upstream_tag}}"),
            "{name}: source.url does not reference the resolved version: {}",
            source.url
        );
        assert!(
            source.url.starts_with("https://"),
            "{name}: source.url must be https, got {}",
            source.url
        );
    }
}

/// Artifacts land in one flat directory per release, so the names have to
/// distinguish version, architecture, and that this is the PIE build.
#[test]
fn artifact_names_are_unambiguous() {
    for (name, _, recipe) in all_recipes() {
        let package = &recipe.package.name;
        for required in ["{{version", "{{arch}}", "pie"] {
            assert!(
                package.contains(required),
                "{name}: package.name '{package}' is missing '{required}'"
            );
        }
    }
}

#[test]
fn self_contained_checks_declare_an_allowlist() {
    for (name, _, recipe) in all_recipes() {
        if !recipe.verify.self_contained.is_empty() {
            assert!(
                !recipe.verify.dynamic_allowlist.is_empty(),
                "{name}: verify.self_contained is set but dynamic_allowlist is empty, \
                 which would reject even libc"
            );
        }
    }
}

/// Recipes reach for helper scripts by `{{repo_root}}`; if one is renamed the
/// build would fail only once it got that far.
#[test]
fn referenced_helper_scripts_exist() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (name, _, recipe) in all_recipes() {
        for (label, value) in templated_strings(&recipe) {
            for fragment in value.split_whitespace() {
                let Some(rest) = fragment.split("{{repo_root}}/").nth(1) else {
                    continue;
                };
                let relative = rest.trim_end_matches(['"', '\'', ';']);
                assert!(
                    root.join(relative).exists(),
                    "{name}: {label} references {relative}, which does not exist"
                );
            }
        }
    }
}
