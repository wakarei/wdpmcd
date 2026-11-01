//! PMC discovery through `/sys/class/tty/ttyS*/device`.

use std::fs;
use std::path::{Path, PathBuf};

use super::PCI_ADDRESS;

/// Returns true for a `DDDD:BB:DD.F` PCI function address.
fn is_pci_address(name: &str) -> bool {
    let b = name.as_bytes();
    if b.len() != 12 || b[4] != b':' || b[7] != b':' || b[10] != b'.' {
        return false;
    }
    b.iter()
        .enumerate()
        .all(|(i, c)| matches!(i, 4 | 7 | 10) || c.is_ascii_hexdigit())
}

/// Walks up from `path` and returns the first component that is a PCI
/// function address. Newer kernels place serial-core port devices
/// (`0000:00:1e.4:0/0000:00:1e.4:0.0`) between the tty and the PCI function.
fn pci_ancestor(path: &Path) -> Option<String> {
    path.ancestors()
        .filter_map(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .find(|n| is_pci_address(n))
}

/// Scans `sys_tty` for a ttyS* whose parent device is the PMC UART and
/// returns the matching node below `dev_root`.
pub fn find(sys_tty: &Path, dev_root: &Path) -> Result<PathBuf, String> {
    let entries = fs::read_dir(sys_tty).map_err(|e| format!("{}: {e}", sys_tty.display()))?;
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("ttyS"))
        .collect();
    names.sort();
    for name in names {
        let Ok(target) = fs::canonicalize(sys_tty.join(&name).join("device")) else {
            continue;
        };
        let is_pmc = pci_ancestor(&target).is_some_and(|addr| addr == PCI_ADDRESS);
        let node = dev_root.join(&name);
        if is_pmc && node.exists() {
            return Ok(node);
        }
    }
    Err(format!("no ttyS device at PCI {PCI_ADDRESS} found"))
}
