//! CPU temperature from the hwmon `coretemp` driver.

use std::fs;
use std::path::Path;

/// Returns the hottest `coretemp` input below `hwmon_root` in degrees Celsius.
pub fn cpu_temperature(hwmon_root: &Path) -> Result<f64, String> {
    let entries = fs::read_dir(hwmon_root).map_err(|e| format!("{}: {e}", hwmon_root.display()))?;
    let mut hottest: Option<f64> = None;
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(name) = fs::read_to_string(dir.join("name")) else {
            continue;
        };
        if name.trim() != "coretemp" {
            continue;
        }
        let Ok(inputs) = fs::read_dir(&dir) else {
            continue;
        };
        for input in inputs.flatten() {
            let file_name = input.file_name();
            let file_name = file_name.to_string_lossy();
            if !(file_name.starts_with("temp") && file_name.ends_with("_input")) {
                continue;
            }
            let text = fs::read_to_string(input.path())
                .map_err(|e| format!("{}: {e}", input.path().display()))?;
            let milli: f64 = text
                .trim()
                .parse()
                .map_err(|_| format!("{}: not a number", input.path().display()))?;
            hottest = Some(hottest.map_or(milli / 1000.0, |h| h.max(milli / 1000.0)));
        }
    }
    hottest.ok_or_else(|| "no coretemp sensors found".to_owned())
}
