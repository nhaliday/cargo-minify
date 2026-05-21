//! This module contains the functionality necessary to find which packages the
//! user wants to minify.

// Portions of the below code are inspired by/taken from Rustfmt, https://github.com/rust-lang/rustfmt
// Copyright (c) 2016-2021 The Rust Project Developers

use std::{
    collections::{BTreeSet, HashSet},
    env, io,
    path::{Path, PathBuf},
};

use cargo_metadata::{camino::Utf8PathBuf, Target};

use crate::{error::Result, CrateResolutionOptions};

pub fn get_targets(
    manifest_path: Option<&Path>,
    crate_resolution: &CrateResolutionOptions,
) -> Result<HashSet<Target>> {
    let mut targets = HashSet::new();

    match crate_resolution {
        CrateResolutionOptions::Root => root_targets(manifest_path, &mut targets)?,
        CrateResolutionOptions::Workspace { exclude } => {
            workspace_targets(manifest_path, exclude, &mut targets, &mut BTreeSet::new())?
        }
        CrateResolutionOptions::Package { packages } => {
            package_targets(manifest_path, packages, &mut targets)?
        }
    }

    if targets.is_empty() {
        eprintln!("crate resolution found no targets");
    }

    Ok(targets.into_iter().map(normalize_target).collect())
}

/// Canonicalizes `Target.src_path` so HashSet equality is robust to symlinks.
/// On macOS in particular, `/var/folders/...` is a symlink to
/// `/private/var/folders/...`, and the two subprocess invocations cargo-minify
/// makes (`cargo metadata` in this module, `cargo check` in unused.rs) can end
/// up with different src_path normalizations for the same target. Because
/// `cargo_metadata::Target` derives full-struct equality (including src_path),
/// any mismatch makes `targets.contains(&message.target)` miss and every
/// diagnostic gets silently dropped. Applied on both sides of the comparison.
pub fn normalize_target(mut t: Target) -> Target {
    if let Ok(canonical) = PathBuf::from(t.src_path.as_str()).canonicalize() {
        if let Ok(utf8) = Utf8PathBuf::from_path_buf(canonical) {
            t.src_path = utf8;
        }
    }
    t
}

fn root_targets(manifest_path: Option<&Path>, targets: &mut HashSet<Target>) -> Result<()> {
    let metadata = get_cargo_metadata(manifest_path)?;
    let workspace_root_path = PathBuf::from(&metadata.workspace_root).canonicalize()?;
    let (in_workspace_root, current_dir_manifest) = if let Some(target_manifest) = manifest_path {
        (
            workspace_root_path == target_manifest,
            target_manifest.canonicalize()?,
        )
    } else {
        let current_dir = env::current_dir()?.canonicalize()?;
        (
            workspace_root_path == current_dir,
            current_dir.join("Cargo.toml"),
        )
    };

    let package_targets = match metadata.packages.len() {
        1 => metadata.packages.into_iter().next().unwrap().targets,
        _ => metadata
            .packages
            .into_iter()
            .filter(|p| {
                in_workspace_root
                    || PathBuf::from(&p.manifest_path)
                        .canonicalize()
                        .unwrap_or_default()
                        == current_dir_manifest
            })
            .flat_map(|p| p.targets)
            .collect(),
    };

    for target in package_targets {
        targets.insert(target);
    }

    Ok(())
}

fn workspace_targets(
    manifest_path: Option<&Path>,
    exclude: &[String],
    targets: &mut HashSet<Target>,
    visited: &mut BTreeSet<String>,
) -> Result<()> {
    let metadata = get_cargo_metadata(manifest_path)?;
    for package in &metadata.packages {
        if !exclude
            .iter()
            .any(|name| glob_match::glob_match(name, &package.name))
        {
            for target in &package.targets {
                targets.insert(target.clone());
            }

            for dependency in &package.dependencies {
                if dependency.path.is_none() || visited.contains(&dependency.name) {
                    continue;
                }

                let manifest_path =
                    PathBuf::from(dependency.path.as_ref().unwrap()).join("Cargo.toml");
                if manifest_path.exists()
                    && !metadata
                        .packages
                        .iter()
                        .any(|p| p.manifest_path.eq(&manifest_path))
                {
                    visited.insert(dependency.name.to_owned());
                    workspace_targets(Some(&manifest_path), exclude, targets, visited)?;
                }
            }
        }
    }

    Ok(())
}

fn package_targets(
    manifest_path: Option<&Path>,
    packages: &[String],
    targets: &mut HashSet<Target>,
) -> Result<()> {
    let metadata = get_cargo_metadata(manifest_path)?;
    let mut workspace_hitlist: BTreeSet<&String> = BTreeSet::from_iter(packages);

    for package in metadata.packages {
        if workspace_hitlist.remove(&package.name) {
            for target in package.targets {
                targets.insert(target);
            }
        }
    }

    if workspace_hitlist.is_empty() {
        Ok(())
    } else {
        let package = workspace_hitlist.iter().next().unwrap();
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("package `{}` is not a member of the workspace", package),
        )
        .into())
    }
}

#[cfg(all(test, unix))]
mod test {
    use std::{
        collections::HashSet,
        os::unix::fs::symlink,
        sync::atomic::{AtomicU64, Ordering},
    };

    use cargo_metadata::Target;

    use super::*;

    fn target_with_src_path(src_path: &str) -> Target {
        serde_json::from_value(serde_json::json!({
            "name": "t",
            "kind": ["bin"],
            "src_path": src_path,
        }))
        .unwrap()
    }

    fn temp_subdir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cargo-minify-resolver-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // Two Target objects whose src_paths point at the same file via different
    // symlink resolutions should compare equal after normalization. Before the
    // fix, `targets.contains(&message.target)` missed when cargo metadata and
    // cargo check produced different normalizations (e.g., macOS's
    // `/var/folders/...` vs `/private/var/folders/...`), silently dropping
    // every diagnostic.
    #[test]
    fn normalize_target_resolves_symlinks() {
        let dir = temp_subdir();
        let real = dir.join("real.rs");
        let link = dir.join("link.rs");
        std::fs::write(&real, b"fn main() {}").unwrap();
        symlink(&real, &link).unwrap();

        let via_link = target_with_src_path(link.to_str().unwrap());
        let via_real = target_with_src_path(real.canonicalize().unwrap().to_str().unwrap());
        assert_ne!(via_link, via_real, "raw Targets must differ");

        let mut set = HashSet::new();
        set.insert(normalize_target(via_link));
        assert!(
            set.contains(&normalize_target(via_real)),
            "HashSet lookup should hit after symlink normalization"
        );

        std::fs::remove_file(&link).ok();
        std::fs::remove_file(&real).ok();
        std::fs::remove_dir(&dir).ok();
    }

    // When canonicalize fails (e.g., file doesn't exist), the original path is
    // preserved — equality behavior is unchanged for those targets.
    #[test]
    fn normalize_target_preserves_unresolvable_path() {
        let path = "/this/path/definitely/does/not/exist.rs";
        let t = target_with_src_path(path);
        let n = normalize_target(t.clone());
        assert_eq!(n.src_path.as_str(), path);
        assert_eq!(n, t);
    }
}

pub fn get_cargo_metadata(manifest_path: Option<&Path>) -> Result<cargo_metadata::Metadata> {
    let mut cmd = cargo_metadata::MetadataCommand::new();
    cmd.no_deps();
    if let Some(manifest_path) = manifest_path {
        cmd.manifest_path(manifest_path);
    }
    cmd.other_options(vec![String::from("--offline")]);

    match cmd.exec() {
        Ok(metadata) => Ok(metadata),
        Err(_) => {
            cmd.other_options(vec![]);
            match cmd.exec() {
                Ok(metadata) => Ok(metadata),
                Err(error) => Err(io::Error::new(io::ErrorKind::Other, error.to_string()).into()),
            }
        }
    }
}
