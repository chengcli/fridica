//! The owner's egress deny list: terms that must never be published.
use anyhow::Result;
use fridica_core::egress::DenyList;
use std::path::Path;

/// Read and compile a deny list, which must be a private regular file.
pub fn deny_list(path: &Path) -> Result<DenyList> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.mode() & 0o077 != 0 || meta.uid() != users::get_current_uid() {
        anyhow::bail!("the deny list must be a regular file only its owner can read (mode 0600)");
    }
    DenyList::parse(&super::contract::read(path, "deny list")?)
}
