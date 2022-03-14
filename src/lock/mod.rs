mod clean;
mod file;
mod plugin;
mod script;
mod source;

use std::fs;
use std::path::Path;

use anyhow::{Context as ResultExt, Result};
use indexmap::{indexmap, IndexMap};
use itertools::{Either, Itertools};
use once_cell::sync::Lazy;
use rayon::prelude::*;

use crate::config::{Config, Plugin, Shell, Template};
use crate::context::{LockContext, SettingsExt};
pub use crate::lock::file::LockedConfig;
use crate::lock::file::{LockedExternalPlugin, LockedPlugin};

/// Read a [`LockedConfig`] from the given path.
pub fn from_path<P>(path: P) -> Result<LockedConfig>
where
    P: AsRef<Path>,
{
    let path = path.as_ref();
    let locked: LockedConfig = toml::from_str(&String::from_utf8_lossy(
        &fs::read(&path)
            .with_context(s!("failed to read locked config from `{}`", path.display()))?,
    ))
    .context("failed to deserialize locked config")?;
    Ok(locked)
}

/// Consume the [`Config`] and convert it to a [`LockedConfig`].
///
/// This method installs all necessary remote dependencies of plugins,
/// validates that local plugins are present, and checks that templates
/// can compile.
pub fn config(ctx: &LockContext, config: Config) -> Result<LockedConfig> {
    let Config {
        shell,
        matches,
        apply,
        templates,
        plugins,
    } = config;

    let templates = {
        let mut map = shell.default_templates().clone();
        for (name, template) in templates {
            map.insert(name, template);
        }
        map
    };

    // Partition the plugins into external and inline plugins.
    let (externals, inlines): (Vec<_>, Vec<_>) =
        plugins
            .into_iter()
            .enumerate()
            .partition_map(|(index, plugin)| match plugin {
                Plugin::External(plugin) => Either::Left((index, plugin)),
                Plugin::Inline(plugin) => Either::Right((index, LockedPlugin::Inline(plugin))),
            });

    // Create a map of unique `Source` to `Vec<Plugin>`
    let mut map = IndexMap::new();
    for (index, plugin) in externals {
        map.entry(plugin.source.clone())
            .or_insert_with(|| Vec::with_capacity(1))
            .push((index, plugin));
    }

    let matches = &matches.as_ref().unwrap_or_else(|| shell.default_matches());
    #[allow(clippy::redundant_closure)]
    let apply = apply.as_ref().unwrap_or_else(|| Shell::default_apply());
    let count = map.len();
    let mut errors = Vec::new();

    let plugins = if count == 0 {
        inlines
            .into_iter()
            .map(|(_, locked)| locked)
            .collect::<Vec<_>>()
    } else {
        // Install the sources in parallel.
        map.into_par_iter()
            .map(|(source, plugins)| {
                let source_name = source.to_string();

                let source = source::lock(ctx, source)
                    .with_context(s!("failed to install source `{}`", source_name))?;

                let mut locked = Vec::with_capacity(plugins.len());
                for (index, plugin) in plugins {
                    let name = plugin.name.clone();
                    let plugin =
                        plugin::lock(ctx, &templates, source.clone(), matches, apply, plugin)
                            .with_context(s!("failed to install plugin `{}`", name));
                    locked.push((index, plugin));
                }
                Ok(locked)
            })
            // The result of this is basically an `Iter<Result<Vec<(usize, Result)>, _>>`
            // The first thing we need to do is to filter out the failures and record the
            // errors that occurred while installing the source in our `errors` list.
            // Finally, we flatten the sub lists into a single iterator.
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|result| match result {
                Ok(ok) => Some(ok),
                Err(err) => {
                    errors.push(err);
                    None
                }
            })
            .flatten()
            // The result of this is basically a `Iter<(usize, Result<LockedExternalPlugin>)`.
            // Similar to the above, we filter out the failures that
            // occurred during locking of individual plugins and record the
            // errors. Next, we combine this with the inline plugins which
            // didn't have to be installed. Finally we sort by the original index
            // to end up wih an iterator of `LockedPlugin`s which we can collect into a
            // `Vec<_>`.
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|(index, result)| match result {
                Ok(plugin) => Some((index, LockedPlugin::External(plugin))),
                Err(err) => {
                    errors.push(err);
                    None
                }
            })
            .chain(inlines)
            .sorted_by_key(|(index, _)| *index)
            .map(|(_, locked)| locked)
            .collect::<Vec<_>>()
    };

    Ok(LockedConfig {
        settings: ctx.settings().clone(),
        templates,
        errors,
        plugins,
    })
}

impl Shell {
    /// The default files to match on for this shell.
    fn default_matches(&self) -> &Vec<String> {
        static DEFAULT_MATCHES_BASH: Lazy<Vec<String>> = Lazy::new(|| {
            vec_into![
                "{{ name }}.plugin.bash",
                "{{ name }}.plugin.sh",
                "{{ name }}.bash",
                "{{ name }}.sh",
                "*.plugin.bash",
                "*.plugin.sh",
                "*.bash",
                "*.sh"
            ]
        });
        static DEFAULT_MATCHES_ZSH: Lazy<Vec<String>> = Lazy::new(|| {
            vec_into![
                "{{ name }}.plugin.zsh",
                "{{ name }}.zsh",
                "{{ name }}.sh",
                "{{ name }}.zsh-theme",
                "*.plugin.zsh",
                "*.zsh",
                "*.sh",
                "*.zsh-theme"
            ]
        });
        match self {
            Self::Bash => &DEFAULT_MATCHES_BASH,
            Self::Zsh => &DEFAULT_MATCHES_ZSH,
        }
    }

    /// The default templates for this shell.
    pub fn default_templates(&self) -> &IndexMap<String, Template> {
        static DEFAULT_TEMPLATES_BASH: Lazy<IndexMap<String, Template>> = Lazy::new(|| {
            indexmap_into! {
                "PATH" => "export PATH=\"{{ dir }}:$PATH\"",
                "source" => Template::from("source \"{{ file }}\"").each(true)
            }
        });
        static DEFAULT_TEMPLATES_ZSH: Lazy<IndexMap<String, Template>> = Lazy::new(|| {
            indexmap_into! {
                "PATH" => "export PATH=\"{{ dir }}:$PATH\"",
                "path" => "path=( \"{{ dir }}\" $path )",
                "fpath" => "fpath=( \"{{ dir }}\" $fpath )",
                "source" => Template::from("source \"{{ file }}\"").each(true)
            }
        });
        match self {
            Self::Bash => &DEFAULT_TEMPLATES_BASH,
            Self::Zsh => &DEFAULT_TEMPLATES_ZSH,
        }
    }

    /// The default template names to apply.
    fn default_apply() -> &'static Vec<String> {
        static DEFAULT_APPLY: Lazy<Vec<String>> = Lazy::new(|| vec_into!["source"]);
        &DEFAULT_APPLY
    }
}

impl Template {
    /// Set whether this template should be applied to every file.
    fn each(mut self, each: bool) -> Self {
        self.each = each;
        self
    }
}

impl LockedConfig {
    /// Verify that the `LockedConfig` is okay.
    pub fn verify(&self, ctx: &LockContext) -> bool {
        if &self.settings != ctx.settings() {
            return false;
        }
        for plugin in &self.plugins {
            match plugin {
                LockedPlugin::External(plugin) => {
                    if !plugin.dir().exists() {
                        return false;
                    }
                    for file in &plugin.files {
                        if !file.exists() {
                            return false;
                        }
                    }
                }
                LockedPlugin::Inline(_) => {}
            }
        }
        true
    }
}

impl LockedExternalPlugin {
    /// Return a reference to the plugin directory.
    fn dir(&self) -> &Path {
        self.plugin_dir.as_ref().unwrap_or(&self.source_dir)
    }
}

////////////////////////////////////////////////////////////////////////////////
// Unit tests
////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::prelude::*;

    use url::Url;

    use crate::config::{ExternalPlugin, Source};
    use crate::context::{LockMode, Settings};
    use crate::log::Output;

    impl LockContext {
        pub fn testing(root: &Path) -> Self {
            Self {
                settings: Settings {
                    version: crate::util::build::CRATE_RELEASE.to_string(),
                    home: "/".into(),
                    config_file: root.join("config.toml"),
                    lock_file: root.join("config.lock"),
                    clone_dir: root.join("repos"),
                    download_dir: root.join("downloads"),
                    data_dir: root.to_path_buf(),
                    config_dir: root.to_path_buf(),
                },
                output: Output {
                    verbosity: crate::log::Verbosity::Quiet,
                    no_color: true,
                },
                mode: LockMode::Normal,
            }
        }
    }

    #[test]
    fn lock_config_empty() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let dir = temp.path();
        let ctx = LockContext::testing(dir);
        let cfg = Config {
            shell: Shell::Zsh,
            matches: None,
            apply: None,
            templates: IndexMap::new(),
            plugins: Vec::new(),
        };

        let locked = config(&ctx, cfg).unwrap();

        assert_eq!(&locked.settings, ctx.settings());
        assert_eq!(locked.plugins, Vec::new());
        assert_eq!(
            locked.templates,
            Shell::default().default_templates().clone(),
        );
        assert_eq!(locked.errors.len(), 0);
    }

    #[test]
    fn locked_config_clean() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let ctx = LockContext::testing(temp.path());
        let cfg = Config {
            shell: Shell::Zsh,
            matches: None,
            apply: None,
            templates: IndexMap::new(),
            plugins: vec![Plugin::External(ExternalPlugin {
                name: "test".to_string(),
                source: Source::Git {
                    url: Url::parse("git://github.com/rossmacarthur/sheldon-test").unwrap(),
                    reference: None,
                },
                dir: None,
                uses: None,
                apply: None,
            })],
        };
        let locked = config(&ctx, cfg).unwrap();
        let test_dir = ctx.clone_dir().join("github.com/rossmacarthur/another-dir");
        let test_file = test_dir.join("test.txt");
        fs::create_dir_all(&test_dir).unwrap();
        {
            fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&test_file)
                .unwrap();
        }

        let mut warnings = Vec::new();
        locked.clean(&ctx, &mut warnings);
        assert!(warnings.is_empty());
        assert!(ctx
            .clone_dir()
            .join("github.com/rossmacarthur/sheldon-test")
            .exists());
        assert!(ctx
            .clone_dir()
            .join("github.com/rossmacarthur/sheldon-test/test.plugin.zsh")
            .exists());
        assert!(!test_file.exists());
        assert!(!test_dir.exists());
    }

    #[test]
    fn locked_config_to_and_from_path() {
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"version = "<version>"
home = "<home>"
config_dir = "<config>"
data_dir = "<data>"
config_file = "<config>/plugins.toml"
lock_file = "<data>/plugins.lock"
clone_dir = "<data>/repos"
download_dir = "<data>/downloads"
plugins = []

[templates]
"#;
        temp.write_all(content.as_bytes()).unwrap();
        let locked_config = from_path(temp.into_temp_path()).unwrap();
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.into_temp_path();
        locked_config.to_path(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }
}
