//! Portable snapshot bundles: "share it with a colleague" (PRD §7).
//!
//! A snapshot on disk is a sandbox *directory* full of absolute paths, plus —
//! on macOS — a machine identifier the hypervisor refuses to restore without.
//! Exporting turns that into one self-describing file that another machine can
//! import; importing gives it a fresh sandbox id and rewrites what is
//! machine-local.
//!
//! What travels: the saved VM state, the writable overlay, a cloned repo
//! workspace if there is one, the spec, and the machine identifier. What does
//! not: the host directories the sandbox had mounted. Those are the importer's
//! files, and a bundle must never be able to name a path on someone else's
//! computer and have it silently appear inside the restored VM — so import
//! *refuses* unless every mount is satisfied locally, and `--remap` is how the
//! importer points each one at their own copy.
//!
//! Three checks make a restore that would fail into an import that refuses:
//! architecture, VMM backend, and guest protocol version. A saved VM's RAM
//! image is not portable across any of them, and the resulting hypervisor error
//! is unreadable.
//!
//! Format: gzipped tar, entries at a flat prefix, `manifest.json` first.

use std::path::{Path, PathBuf};

use agentos_core::{Error, Result, SandboxId, SandboxSpec};
use serde::{Deserialize, Serialize};

/// Bumped when the on-disk layout changes incompatibly. An importer that
/// doesn't know a version says so instead of unpacking a shape it can't read.
pub const BUNDLE_FORMAT: u32 = 1;

/// What the importing machine needs to know before it unpacks anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    /// The sandbox this was exported from — kept for provenance only; the
    /// importer always mints a new id so two people can hold the same bundle.
    pub source_id: String,
    /// `aarch64` / `x86_64`. A saved guest RAM image is arch-specific.
    pub arch: String,
    /// `vz` or `cloud-hypervisor`: saved state formats are not interchangeable.
    pub backend: String,
    /// Guest agent wire version, so an old bundle meeting a new daemon is
    /// caught here rather than at the handshake.
    pub protocol_version: u32,
    pub exported_unix_secs: u64,
    pub spec: SandboxSpec,
    /// Host paths the source machine had mounted, in spec order. Shown to the
    /// importer so they know what to remap; never used as a path directly.
    pub mounts: Vec<String>,
    pub has_workspace: bool,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn current_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "unknown"
    }
}

/// Files inside a sandbox dir that make up a shareable snapshot.
/// `vmstate` is a file on vz and a directory on Cloud Hypervisor; tar handles
/// both, which is why this is a tar rather than a hand-rolled container.
const MEMBERS: &[&str] = &["vmstate", "spec.json", "machine-id", "overlay.img", "workspace"];

/// Write `sandbox_dir` out as a bundle at `dest`.
pub fn export(
    sandbox_dir: &Path,
    spec: &SandboxSpec,
    source_id: &SandboxId,
    backend: &str,
    dest: &Path,
) -> Result<u64> {
    if !sandbox_dir.join("vmstate").exists() {
        return Err(Error::InvalidSpec(
            "sandbox has no saved VM state; snapshot it first (agentos snapshot <id>)".into(),
        ));
    }

    let manifest = Manifest {
        format: BUNDLE_FORMAT,
        source_id: source_id.to_string(),
        arch: current_arch().to_string(),
        backend: backend.to_string(),
        protocol_version: agentos_core::protocol::PROTOCOL_VERSION,
        exported_unix_secs: now_secs(),
        spec: spec.clone(),
        mounts: spec
            .mounts
            .iter()
            .map(|m| m.host_path.display().to_string())
            .collect(),
        has_workspace: sandbox_dir.join("workspace").exists(),
    };

    let staging = sandbox_dir.join("manifest.json");
    std::fs::write(
        &staging,
        serde_json::to_vec_pretty(&manifest).map_err(|e| Error::Backend(e.to_string()))?,
    )?;

    // tar/gzip via the system tools: the alternative is pulling a tar crate
    // into the daemon to do what every host already has.
    let mut cmd = std::process::Command::new("tar");
    cmd.arg("-czf").arg(dest).arg("-C").arg(sandbox_dir).arg("manifest.json");
    for m in MEMBERS {
        if sandbox_dir.join(m).exists() {
            cmd.arg(m);
        }
    }
    let out = cmd
        .output()
        .map_err(|e| Error::Backend(format!("running tar: {e}")))?;
    let _ = std::fs::remove_file(&staging);
    if !out.status.success() {
        return Err(Error::Backend(format!(
            "tar failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0))
}

/// Read a bundle's manifest without unpacking it, so `import` can refuse
/// before writing anything into the sandbox directory.
pub fn peek(path: &Path) -> Result<Manifest> {
    let out = std::process::Command::new("tar")
        .arg("-xzOf")
        .arg(path)
        .arg("manifest.json")
        .output()
        .map_err(|e| Error::Backend(format!("running tar: {e}")))?;
    if !out.status.success() {
        return Err(Error::InvalidSpec(format!(
            "not an Agent OS bundle: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| Error::InvalidSpec(format!("bundle manifest unreadable: {e}")))
}

/// Why an import was refused. Separated from the message so the CLI can print
/// remediation the user can act on rather than a wall of text.
pub fn check_compatible(manifest: &Manifest, backend: &str) -> Result<()> {
    if manifest.format != BUNDLE_FORMAT {
        return Err(Error::InvalidSpec(format!(
            "bundle format v{}, this daemon speaks v{BUNDLE_FORMAT}",
            manifest.format
        )));
    }
    if manifest.arch != current_arch() {
        return Err(Error::InvalidSpec(format!(
            "bundle was saved on {} but this machine is {} — a saved VM's memory \
             image cannot cross architectures",
            manifest.arch,
            current_arch()
        )));
    }
    if manifest.backend != backend {
        return Err(Error::InvalidSpec(format!(
            "bundle was saved by the '{}' hypervisor but this machine uses '{backend}' — \
             saved state formats are not interchangeable",
            manifest.backend
        )));
    }
    if manifest.protocol_version != agentos_core::protocol::PROTOCOL_VERSION {
        return Err(Error::InvalidSpec(format!(
            "bundle's guest agent speaks protocol v{}, this daemon speaks v{} — \
             the restored guest could not be talked to",
            manifest.protocol_version,
            agentos_core::protocol::PROTOCOL_VERSION
        )));
    }
    Ok(())
}

/// Resolve the bundle's mounts against this machine.
///
/// `remap` entries are `old=new`. Every mount must end up at a directory that
/// exists here; anything else is refused with the full list, because the
/// alternative — creating the paths, or dropping the mount — either invents
/// data the guest expects or changes the device set the saved VM was using.
pub fn remap_mounts(spec: &mut SandboxSpec, remap: &[(String, String)]) -> Result<()> {
    let mut missing = Vec::new();
    for m in &mut spec.mounts {
        let original = m.host_path.display().to_string();
        if let Some((_, new)) = remap.iter().find(|(old, _)| *old == original) {
            m.host_path = PathBuf::from(new);
        }
        match m.host_path.canonicalize() {
            Ok(p) if p.is_dir() => m.host_path = p,
            _ => missing.push(format!(
                "  {} -> {}",
                original,
                m.host_path.display()
            )),
        }
    }
    if !missing.is_empty() {
        return Err(Error::InvalidSpec(format!(
            "these mounts don't exist on this machine:\n{}\n\
             point each at a local directory with --remap '<original>=<local path>'",
            missing.join("\n")
        )));
    }
    Ok(())
}

/// Unpack a bundle into `sandbox_dir`.
pub fn unpack(path: &Path, sandbox_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(sandbox_dir)?;
    let out = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(path)
        .arg("-C")
        .arg(sandbox_dir)
        .output()
        .map_err(|e| Error::Backend(format!("running tar: {e}")))?;
    if !out.status.success() {
        return Err(Error::Backend(format!(
            "unpacking bundle failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let _ = std::fs::remove_file(sandbox_dir.join("manifest.json"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            format: BUNDLE_FORMAT,
            source_id: "x".into(),
            arch: current_arch().into(),
            backend: "vz".into(),
            protocol_version: agentos_core::protocol::PROTOCOL_VERSION,
            exported_unix_secs: 0,
            spec: SandboxSpec::command(["true"]),
            mounts: vec![],
            has_workspace: false,
        }
    }

    #[test]
    fn a_matching_bundle_is_accepted() {
        assert!(check_compatible(&manifest(), "vz").is_ok());
    }

    #[test]
    fn a_bundle_from_another_hypervisor_is_refused_by_name() {
        let e = check_compatible(&manifest(), "cloud-hypervisor").unwrap_err();
        assert!(e.to_string().contains("not interchangeable"), "{e}");
    }

    #[test]
    fn an_arch_mismatch_is_refused() {
        let mut m = manifest();
        m.arch = "sparc".into();
        let e = check_compatible(&m, "vz").unwrap_err();
        assert!(e.to_string().contains("architectures"), "{e}");
    }

    #[test]
    fn an_unknown_format_version_is_refused() {
        let mut m = manifest();
        m.format = BUNDLE_FORMAT + 99;
        assert!(check_compatible(&m, "vz").is_err());
    }

    /// The security property: a bundle naming a path that doesn't exist here
    /// must not silently restore without it.
    #[test]
    fn a_mount_missing_locally_refuses_rather_than_vanishing() {
        let mut spec = SandboxSpec::command(["true"]);
        spec.mounts = vec![agentos_core::MountSpec {
            host_path: "/definitely/not/here/agentos-test".into(),
            guest_path: "/mnt/x".into(),
            mode: agentos_core::MountMode::ReadOnly,
        }];
        let e = remap_mounts(&mut spec, &[]).unwrap_err();
        assert!(e.to_string().contains("--remap"), "{e}");
    }

    #[test]
    fn remap_points_a_mount_at_a_local_directory() {
        let tmp = std::env::temp_dir();
        let mut spec = SandboxSpec::command(["true"]);
        spec.mounts = vec![agentos_core::MountSpec {
            host_path: "/somewhere/else".into(),
            guest_path: "/mnt/x".into(),
            mode: agentos_core::MountMode::ReadOnly,
        }];
        remap_mounts(
            &mut spec,
            &[("/somewhere/else".into(), tmp.display().to_string())],
        )
        .unwrap();
        assert_eq!(spec.mounts[0].host_path, tmp.canonicalize().unwrap());
    }
}
