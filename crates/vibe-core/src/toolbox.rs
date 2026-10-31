//! `~/.config/vibeos/toolbox.toml`, the optional recipe for the Linux
//! template (PRD: The toolbox shell):
//!
//! ```toml
//! packages = ["kubectl", "postgresql-client"]   # apt
//!
//! [[install]]
//! name = "gcloud"
//! run  = "curl -sSL https://sdk.cloud.google.com | bash -s -- --disable-prompts --install-dir=/opt"
//!
//! [update]   # run weekly in a toolbox VM; the result is saved as a new template version
//! run = ["gcloud components update --quiet"]
//! ```
//!
//! **Rebuild template** replays `packages` and `[[install]]` into a fresh
//! template on a new base image; `[update]`'s commands run weekly on the
//! template there is. They build the template, not the Linux home, so they
//! run as root, as `sudo` would run them in the toolbox shell. `vibed`
//! reads the file each time it needs it, so edits take effect at once.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use toml_edit::{DocumentMut, Item, Value};

/// The recipe. An absent file is the empty recipe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recipe {
    /// Debian packages, installed with `apt-get install`.
    pub packages: Vec<String>,
    /// Commands that install something, in order, each a shell command.
    pub install: Vec<Install>,
    /// Shell commands run weekly on the template, in order.
    pub update: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub name: String,
    pub run: String,
}

impl Recipe {
    /// Whether there's anything to replay into a fresh template.
    pub fn installs_anything(&self) -> bool {
        !self.packages.is_empty() || !self.install.is_empty()
    }
}

#[derive(Debug)]
pub enum RecipeError {
    Io(String, io::Error),
    Invalid(String),
}

impl fmt::Display for RecipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecipeError::Io(path, e) => write!(f, "can't read {path}: {e}"),
            RecipeError::Invalid(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for RecipeError {}

/// Reads the recipe at `path`; a missing file is the empty recipe.
pub fn load(path: &Path) -> Result<Recipe, RecipeError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            parse(&text).map_err(|why| RecipeError::Invalid(format!("{}: {why}", path.display())))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Recipe::default()),
        Err(e) => Err(RecipeError::Io(path.display().to_string(), e)),
    }
}

/// Reads a recipe from its text.
pub fn parse(text: &str) -> Result<Recipe, String> {
    let doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| e.to_string().trim_end().to_owned())?;
    let mut recipe = Recipe::default();
    for (key, item) in doc.iter() {
        match key {
            "packages" => recipe.packages = packages(item)?,
            "install" => recipe.install = installs(item)?,
            "update" => recipe.update = update(item)?,
            other => {
                return Err(format!(
                    "unknown setting `{other}`: the recipe has `packages`, `[[install]]` and \
                     `[update]`"
                ));
            }
        }
    }
    Ok(recipe)
}

fn strings(item: &Item, what: &str) -> Result<Vec<String>, String> {
    let array = item
        .as_array()
        .ok_or_else(|| format!("`{what}` is a list of strings"))?;
    array
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("`{what}` is a list of strings"))
        })
        .collect()
}

/// A Debian package as `apt-get install` takes it: a name, maybe with an
/// architecture or a version. Never an option, nor anything a shell would
/// read differently.
fn package_name(p: &str) -> bool {
    p.starts_with(|c: char| c.is_ascii_alphanumeric())
        && p.len() <= 200
        && p.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | ':' | '=' | '~' | '_')
        })
}

fn packages(item: &Item) -> Result<Vec<String>, String> {
    let list = strings(item, "packages")?;
    if let Some(bad) = list.iter().find(|p| !package_name(p)) {
        return Err(format!("`{bad}` isn't a Debian package name"));
    }
    Ok(list)
}

fn text(table: &dyn toml_edit::TableLike, key: &str, what: &str) -> Result<String, String> {
    let value = table
        .get(key)
        .ok_or_else(|| format!("{what} needs `{key}`"))?
        .as_str()
        .ok_or_else(|| format!("{what}: `{key}` is a string"))?;
    if value.trim().is_empty() {
        return Err(format!("{what}: `{key}` is empty"));
    }
    Ok(value.to_owned())
}

fn installs(item: &Item) -> Result<Vec<Install>, String> {
    let tables = item
        .as_array_of_tables()
        .ok_or("`install` is written `[[install]]`, with `name` and `run`")?;
    let mut out = Vec::new();
    for (i, table) in tables.iter().enumerate() {
        let what = format!("[[install]] number {}", i + 1);
        if let Some((key, _)) = table.iter().find(|(k, _)| !matches!(*k, "name" | "run")) {
            return Err(format!("{what}: unknown setting `{key}`"));
        }
        out.push(Install {
            name: text(table, "name", &what)?,
            run: text(table, "run", &what)?,
        });
    }
    Ok(out)
}

fn update(item: &Item) -> Result<Vec<String>, String> {
    let table = item
        .as_table_like()
        .ok_or("`update` is written `[update]`, with `run`")?;
    if let Some((key, _)) = table.iter().find(|(k, _)| *k != "run") {
        return Err(format!("[update]: unknown setting `{key}`"));
    }
    let run = match table.get("run") {
        None => return Ok(Vec::new()),
        Some(run) => run,
    };
    let commands = match run.as_value() {
        Some(Value::String(s)) => vec![s.value().clone()],
        _ => strings(run, "[update] run")?,
    };
    if commands.iter().any(|c| c.trim().is_empty()) {
        return Err("[update]: a command in `run` is empty".into());
    }
    Ok(commands)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recipe the PRD shows.
    const PRD: &str = r#"
packages = ["kubectl", "postgresql-client"]   # apt

[[install]]
name = "gcloud"
run  = "curl -sSL https://sdk.cloud.google.com | bash -s -- --disable-prompts --install-dir=/opt"

[update]   # run weekly in a toolbox VM; the result is saved as a new template version
run = ["gcloud components update --quiet"]
"#;

    #[test]
    fn the_prd_recipe_reads() {
        let r = parse(PRD).unwrap();
        assert_eq!(r.packages, ["kubectl", "postgresql-client"]);
        assert_eq!(r.install.len(), 1);
        assert_eq!(r.install[0].name, "gcloud");
        assert!(r.install[0].run.contains("--install-dir=/opt"));
        assert_eq!(r.update, ["gcloud components update --quiet"]);
        assert!(r.installs_anything());
    }

    #[test]
    fn everything_is_optional() {
        assert_eq!(parse("").unwrap(), Recipe::default());
        assert!(!Recipe::default().installs_anything());
        let r = parse("[update]\nrun = \"apt-get upgrade -y\"\n").unwrap();
        assert_eq!(r.update, ["apt-get upgrade -y"]);
        assert!(!r.installs_anything());
        assert_eq!(parse("[update]\n").unwrap().update, Vec::<String>::new());
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            load(&tmp.path().join("toolbox.toml")).unwrap(),
            Recipe::default()
        );
        std::fs::write(tmp.path().join("toolbox.toml"), PRD).unwrap();
        assert_eq!(
            load(&tmp.path().join("toolbox.toml")).unwrap(),
            parse(PRD).unwrap()
        );
    }

    #[test]
    fn mistakes_are_named() {
        for (text, why) in [
            ("packages = \"kubectl\"", "list of strings"),
            ("packages = [1]", "list of strings"),
            (
                "packages = [\"--allow-unauthenticated\"]",
                "isn't a Debian package",
            ),
            ("packages = [\"a b\"]", "isn't a Debian package"),
            ("packages = [\"x;rm -rf /\"]", "isn't a Debian package"),
            ("pakages = []", "unknown setting `pakages`"),
            ("[install]\nname = \"x\"", "[[install]]"),
            ("[[install]]\nname = \"x\"", "needs `run`"),
            ("[[install]]\nrun = \"x\"", "needs `name`"),
            ("[[install]]\nname = \"x\"\nrun = \" \"", "is empty"),
            (
                "[[install]]\nname = \"x\"\nrun = \"y\"\nwhen = 1",
                "unknown setting `when`",
            ),
            ("[update]\nrun = [\"\"]", "is empty"),
            ("[update]\nevery = \"day\"", "unknown setting `every`"),
            ("update = 3", "[update]"),
            ("packages = [", ""),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(why), "{text:?}: {e}");
        }
        let ok = [
            "kubectl",
            "libc6:arm64",
            "nodejs=26.1.0-1",
            "g++",
            "python3.13-venv",
        ];
        assert!(ok.iter().all(|p| package_name(p)));
    }
}
