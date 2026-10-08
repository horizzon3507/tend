use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::util::home;

pub struct Target {
    pub name: &'static str,
    pub label: &'static str,
    /// Safe means only regenerable artifacts: no authored work is lost.
    pub safe: bool,
    pub note: &'static str,
}

pub const TARGETS: &[Target] = &[
    Target {
        name: "node_modules",
        label: "Node dependencies",
        safe: true,
        note: "restored with bun install / npm install",
    },
    Target {
        name: "target",
        label: "Rust build",
        safe: true,
        note: "cargo build recreates it",
    },
    Target {
        name: "__pycache__",
        label: "Python bytecode",
        safe: true,
        note: "Python regenerates it",
    },
    Target {
        name: ".pytest_cache",
        label: "pytest cache",
        safe: true,
        note: "test cache only",
    },
    Target {
        name: ".mypy_cache",
        label: "mypy cache",
        safe: true,
        note: "type cache only",
    },
    Target {
        name: ".ruff_cache",
        label: "ruff cache",
        safe: true,
        note: "lint cache only",
    },
    Target {
        name: ".turbo",
        label: "Turborepo cache",
        safe: true,
        note: "build cache only",
    },
    Target {
        name: ".next",
        label: "Next.js build",
        safe: true,
        note: "next build recreates it",
    },
    Target {
        name: ".svelte-kit",
        label: "SvelteKit cache",
        safe: true,
        note: "recreated on dev/build",
    },
    Target {
        name: ".nuxt",
        label: "Nuxt build",
        safe: true,
        note: "nuxt build recreates it",
    },
    Target {
        name: ".gradle",
        label: "Gradle cache",
        safe: true,
        note: "build cache only",
    },
    Target {
        name: ".parcel-cache",
        label: "Parcel cache",
        safe: true,
        note: "build cache only",
    },
    Target {
        name: ".venv",
        label: "Python virtualenv",
        safe: false,
        note: "recreatable, but requires reinstalling deps",
    },
    Target {
        name: "venv",
        label: "Python virtualenv",
        safe: false,
        note: "recreatable, but requires reinstalling deps",
    },
    Target {
        name: "dist",
        label: "dist directory",
        safe: false,
        note: "may contain published artifacts",
    },
    Target {
        name: "build",
        label: "build directory",
        safe: false,
        note: "may contain published artifacts",
    },
];

pub fn find(name: &str) -> Option<&'static Target> {
    TARGETS.iter().find(|target| target.name == name)
}

/// Install roots: a node_modules here belongs to a tool the user installed, not to a
/// project, so deleting it breaks the tool instead of saving rebuild time.
fn protected_prefixes() -> &'static Vec<PathBuf> {
    static PREFIXES: OnceLock<Vec<PathBuf>> = OnceLock::new();
    PREFIXES.get_or_init(|| {
        [
            ".bun/install",
            ".nvm",
            ".npm",
            ".cargo/registry",
            "go/pkg/mod",
            ".local/opt",
            ".omp/plugins",
            ".local/share/pnpm",
        ]
        .iter()
        .map(|suffix| home().join(suffix))
        .collect()
    })
}

pub fn is_protected(path: &Path) -> bool {
    protected_prefixes()
        .iter()
        .any(|prefix| path == prefix || path.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_installations_are_never_treated_as_cache() {
        let home = home();
        assert!(is_protected(&home.join(".bun/install/global/node_modules")));
        assert!(is_protected(
            &home.join(".local/opt/opencode-1/node_modules")
        ));
        assert!(is_protected(&home.join(".omp/plugins/node_modules")));
        assert!(is_protected(&home.join(".cargo/registry/src/x/target")));
    }

    #[test]
    fn project_build_dirs_are_not_protected() {
        assert!(!is_protected(Path::new("/tmp/projeto/node_modules")));
        assert!(!is_protected(Path::new("/home/outro/target")));
    }

    #[test]
    fn a_sibling_with_a_similar_name_is_not_protected() {
        let home = home();
        assert!(!is_protected(&home.join(".bun/installation")));
        assert!(!is_protected(&home.join(".npms")));
    }

    #[test]
    fn targets_have_unique_names_and_known_safety() {
        let mut names: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        let total = names.len();
        names.dedup();
        assert_eq!(names.len(), total);
        assert!(find("node_modules").unwrap().safe);
        assert!(!find("dist").unwrap().safe);
        assert!(find("inexistente").is_none());
    }
}
