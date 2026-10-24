pub mod backup;
pub mod check;
pub mod clean;
pub mod completions;
pub mod config;
pub mod history;
pub mod man;
pub mod projects;
pub mod scan;
pub mod space;
pub mod status;
pub mod watch;

use std::path::PathBuf;

use crate::ui;
use crate::util::tildify;

/// Drops roots that do not exist, warns about each one, and refuses to continue with
/// an empty set so a typo never turns into a silent "nothing found".
pub fn usable_roots(roots: Vec<PathBuf>) -> Option<Vec<PathBuf>> {
    let mut usable = Vec::new();
    for root in roots {
        if root.exists() {
            usable.push(root);
        } else {
            ui::say_warn(&format!("ignoring missing root: {}", tildify(&root)));
        }
    }
    if usable.is_empty() {
        ui::say_fail("no valid roots to scan");
        return None;
    }
    Some(usable)
}
