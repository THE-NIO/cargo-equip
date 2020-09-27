use anyhow::{bail, Context as _};
use cargo_metadata as cm;
use easy_ext::ext;
use itertools::Itertools as _;
use once_cell::sync::Lazy;
use rand::Rng as _;
use regex::Regex;
use serde::{de::Error as _, Deserialize, Deserializer};
use std::collections::HashSet;
use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    path::{Path, PathBuf},
    str::{self, FromStr},
};
use syn::Ident;

use crate::shell::Shell;

pub(crate) fn locate_project(cwd: &Path) -> anyhow::Result<PathBuf> {
    cwd.ancestors()
        .map(|p| p.join("Cargo.toml"))
        .find(|p| p.exists())
        .with_context(|| {
            format!(
                "could not find `Cargo.toml` in `{}` or any parent directory",
                cwd.display(),
            )
        })
}

pub(crate) fn cargo_metadata(manifest_path: &Path, cwd: &Path) -> cm::Result<cm::Metadata> {
    cm::MetadataCommand::new()
        .manifest_path(manifest_path)
        .current_dir(cwd)
        .exec()
}

pub(crate) fn cargo_check_using_current_lockfile_and_cache(
    metadata: &cm::Metadata,
    package: &cm::Package,
    code: &str,
) -> anyhow::Result<()> {
    let name = {
        let mut rng = rand::thread_rng();
        let suf = (0..16)
            .map(|_| match rng.gen_range(0, 26 + 10) {
                n @ 0..=25 => b'a' + n,
                n @ 26..=35 => b'0' + n - 26,
                _ => unreachable!(),
            })
            .collect::<Vec<_>>();
        let suf = str::from_utf8(&suf).expect("should be valid ASCII");
        format!("cargo-equip-check-output-{}", suf)
    };

    let temp_pkg = tempfile::Builder::new()
        .prefix(&name)
        .rand_bytes(0)
        .tempdir()?;

    let cargo_exe = crate::process::cargo_exe()?;

    crate::process::process(&cargo_exe)
        .arg("init")
        .arg("-q")
        .arg("--vcs")
        .arg("none")
        .arg("--bin")
        .arg("--edition")
        .arg(&package.edition)
        .arg("--name")
        .arg(&name)
        .arg(temp_pkg.path())
        .cwd(&metadata.workspace_root)
        .exec()?;

    let orig_manifest =
        std::fs::read_to_string(&package.manifest_path)?.parse::<toml_edit::Document>()?;

    let mut temp_manifest = std::fs::read_to_string(temp_pkg.path().join("Cargo.toml"))?
        .parse::<toml_edit::Document>()?;

    temp_manifest["dependencies"] = orig_manifest["dependencies"].clone();
    if let toml_edit::Item::Table(dependencies) = &mut temp_manifest["dependencies"] {
        let path_deps = dependencies
            .iter()
            .filter(|(_, i)| !i["path"].is_none())
            .map(|(k, _)| k.to_owned())
            .collect::<Vec<_>>();
        for path_dep in path_deps {
            dependencies.remove(&path_dep);
        }
    }

    std::fs::write(
        temp_pkg.path().join("Cargo.toml"),
        temp_manifest.to_string(),
    )?;

    std::fs::create_dir(temp_pkg.path().join("src").join("bin"))?;
    std::fs::write(
        temp_pkg
            .path()
            .join("src")
            .join("bin")
            .join(name)
            .with_extension("rs"),
        code,
    )?;

    std::fs::remove_file(temp_pkg.path().join("src").join("main.rs"))?;

    std::fs::copy(
        metadata.workspace_root.join("Cargo.lock"),
        temp_pkg.path().join("Cargo.lock"),
    )?;

    crate::process::process(cargo_exe)
        .arg("check")
        .arg("--target-dir")
        .arg(&metadata.target_directory)
        .arg("--manifest-path")
        .arg(temp_pkg.path().join("Cargo.toml"))
        .arg("--offline")
        .cwd(&metadata.workspace_root)
        .exec()?;

    temp_pkg.close()?;
    Ok(())
}

#[ext(MetadataExt)]
impl cm::Metadata {
    pub(crate) fn exactly_one_bin_target(&self) -> anyhow::Result<(&cm::Target, &cm::Package)> {
        match &*bin_targets(self).collect::<Vec<_>>() {
            [] => bail!("no bin target in this workspace"),
            [bin] => Ok(*bin),
            [bins @ ..] => bail!(
                "could not determine which binary to choose. Use the `--bin` option or \
                 `--src` option to specify a binary.\n\
                 available binaries: {}\n\
                 note: currently `cargo-equip` does not support the `default-run` manifest key.",
                bins.iter()
                    .map(|(cm::Target { name, .. }, _)| name)
                    .format(", "),
            ),
        }
    }

    pub(crate) fn bin_target_by_name<'a>(
        &'a self,
        name: &str,
    ) -> anyhow::Result<(&'a cm::Target, &'a cm::Package)> {
        match *bin_targets(self)
            .filter(|(t, _)| t.name == name)
            .collect::<Vec<_>>()
        {
            [] => bail!("no bin target named `{}`", name),
            [bin] => Ok(bin),
            [..] => bail!("multiple bin targets named `{}` in this workspace", name),
        }
    }

    pub(crate) fn bin_target_by_src_path<'a>(
        &'a self,
        src_path: &Path,
    ) -> anyhow::Result<(&'a cm::Target, &'a cm::Package)> {
        match *bin_targets(self)
            .filter(|(t, _)| t.src_path == src_path)
            .collect::<Vec<_>>()
        {
            [] => bail!(
                "`{}` is not the main source file of any bin targets in this workspace ",
                src_path.display(),
            ),
            [bin] => Ok(bin),
            [..] => bail!(
                "multiple bin targets which `src_path` is `{}`",
                src_path.display(),
            ),
        }
    }

    pub(crate) fn dep_lib_by_extern_crate_name<'a>(
        &'a self,
        package_id: &cm::PackageId,
        extern_crate_name: &str,
    ) -> anyhow::Result<(&cm::Target, &cm::Package)> {
        // https://docs.rs/cargo/0.47.0/src/cargo/core/resolver/resolve.rs.html#323-352

        let package = &self[package_id];

        let node = self
            .resolve
            .as_ref()
            .into_iter()
            .flat_map(|cm::Resolve { nodes, .. }| nodes)
            .find(|cm::Node { id, .. }| id == package_id)
            .with_context(|| format!("`{}` not found in the dependency graph", package_id))?;

        let found_explicitly_renamed_one = package
            .dependencies
            .iter()
            .flat_map(|cm::Dependency { rename, .. }| rename)
            .any(|rename| rename == extern_crate_name);

        if found_explicitly_renamed_one {
            let package = &self[&node
                .deps
                .iter()
                .find(|cm::NodeDep { name, .. }| name == extern_crate_name)
                .expect("found the dep in `dependencies`, not in `resolve.deps`")
                .pkg];

            let lib = package
                .targets
                .iter()
                .find(|cm::Target { kind, .. }| *kind == ["lib".to_owned()])
                .with_context(|| {
                    format!(
                        "`{}` is resolved as `{}` but it has no `lib` target",
                        extern_crate_name, package.name,
                    )
                })?;

            Ok((lib, package))
        } else {
            node.dependencies
                .iter()
                .map(|dep_id| &self[dep_id])
                .flat_map(|p| p.targets.iter().map(move |t| (t, p)))
                .find(|(t, _)| t.name == extern_crate_name && *t.kind == ["lib".to_owned()])
                .with_context(|| {
                    format!(
                        "no external library found which `extern_crate_name` is `{}`",
                        extern_crate_name,
                    )
                })
        }
    }

    pub(crate) fn extern_crate_name(
        &self,
        from: &cm::PackageId,
        to: &cm::PackageId,
    ) -> Option<String> {
        let from = &self[from];
        let to = &self[to];

        let explicit_names = from
            .dependencies
            .iter()
            .flat_map(|cm::Dependency { rename, .. }| rename)
            .collect::<HashSet<_>>();

        let cm::NodeDep { name, .. } = self
            .resolve
            .as_ref()?
            .nodes
            .iter()
            .find(|cm::Node { id, .. }| *id == from.id)?
            .deps
            .iter()
            .find(|cm::NodeDep { pkg, dep_kinds, .. }| {
                *pkg == to.id
                    && (dep_kinds.is_empty()
                        || matches!(
                            &**dep_kinds,
                            [cm::DepKindInfo {
                                kind: cm::DependencyKind::Normal,
                                ..
                            }]
                        ))
            })?;

        if explicit_names.contains(name) {
            Some(name.clone())
        } else {
            to.targets
                .iter()
                .find(|cm::Target { kind, .. }| *kind == ["lib"])
                .map(|cm::Target { name, .. }| name.replace('-', "_"))
        }
    }
}

fn bin_targets(metadata: &cm::Metadata) -> impl Iterator<Item = (&cm::Target, &cm::Package)> {
    metadata
        .packages
        .iter()
        .filter(move |cm::Package { id, .. }| metadata.workspace_members.contains(id))
        .flat_map(|p| p.targets.iter().map(move |t| (t, p)))
        .filter(|(cm::Target { kind, .. }, _)| *kind == ["bin".to_owned()])
}

#[ext(PackageExt)]
impl cm::Package {
    pub(crate) fn parse_metadata(
        &self,
        shell: &mut Shell,
    ) -> anyhow::Result<PackageMetadataCargoEquip> {
        #[derive(Deserialize)]
        #[serde(rename_all = "kebab-case")]
        struct PackageMetadata {
            cargo_equip: Option<PackageMetadataCargoEquip>,
        }

        let cargo_equip = if self.metadata.is_null() {
            None
        } else {
            let PackageMetadata { cargo_equip } = serde_json::from_value(self.metadata.clone())
                .with_context(|| {
                    format!(
                        "could not parse `package.metadata.cargo-equip` at `{}`",
                        self.manifest_path.display(),
                    )
                })?;
            cargo_equip
        };

        if let Some(cargo_equip) = cargo_equip {
            Ok(cargo_equip)
        } else {
            shell.warn(format!(
                "missing `package.metadata.cargo-equip` in `{}`. including all of the modules",
                self.manifest_path.display(),
            ))?;
            Ok(PackageMetadataCargoEquip::default())
        }
    }
}

#[derive(Default, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct PackageMetadataCargoEquip {
    pub(crate) module_dependencies: HashMap<PseudoModulePath, BTreeSet<PseudoModulePath>>,
}

#[derive(Debug, Clone, Ord, Eq, PartialOrd, PartialEq, Hash)]
pub(crate) struct PseudoModulePath {
    pub(crate) extern_crate_name: String,
    pub(crate) module_name: String,
}

impl PseudoModulePath {
    pub(crate) fn new(extern_crate_name: &Ident, module_name: &Ident) -> Self {
        Self {
            extern_crate_name: extern_crate_name.to_string(),
            module_name: module_name.to_string(),
        }
    }
}

impl<'de> Deserialize<'de> for PseudoModulePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

impl FromStr for PseudoModulePath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        static REGEX: Lazy<Regex> =
            Lazy::new(|| Regex::new(r"\A::([a-zA-Z0-9_]+)::([a-zA-Z0-9_]+)\z").unwrap());

        if let Some(caps) = REGEX.captures(s) {
            Ok(Self {
                extern_crate_name: caps[1].to_owned(),
                module_name: caps[2].to_owned(),
            })
        } else {
            Err(format!("expected `{}`", REGEX.as_str()))
        }
    }
}

impl fmt::Display for PseudoModulePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"::{}::{}\"", self.extern_crate_name, self.module_name)
    }
}

#[cfg(test)]
mod tests {
    use crate::workspace::PseudoModulePath;

    #[test]
    fn parse_pseudo_module_path() {
        fn parse(s: &str) -> Result<(), ()> {
            s.parse::<PseudoModulePath>().map(|_| ()).map_err(|_| ())
        }

        assert!(parse("::library::module").is_ok());
        assert!(parse("::library::module::module").is_err());
        assert!(parse("library::module").is_err());
    }
}
