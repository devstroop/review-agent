//! Map file paths to human-readable programming language names for prompt context.
//!
//! Uses a static sorted slice with binary search — zero dependencies, `O(log n)`
//! lookups, and fast enough for the ~40 entries we care about (ADR-022). No regex,
//! no file-content sniffing: v1 values simplicity over precision.

use std::path::Path;

/// Sorted `(extension, language)` pairs. Must stay sorted by extension for
/// `binary_search_by_key` to work correctly.
const LANGUAGES: &[(&str, &str)] = &[
    (".bash", "Shell"),
    (".c", "C"),
    (".cpp", "C++"),
    (".cs", "C#"),
    (".css", "CSS"),
    (".dart", "Dart"),
    (".dockerfile", "Dockerfile"),
    (".ex", "Elixir"),
    (".exs", "Elixir"),
    (".go", "Go"),
    (".h", "C"),
    (".hpp", "C++"),
    (".java", "Java"),
    (".js", "JavaScript"),
    (".json", "JSON"),
    (".jsx", "React"),
    (".kt", "Kotlin"),
    (".less", "CSS"),
    (".md", "Markdown"),
    (".php", "PHP"),
    (".py", "Python"),
    (".rb", "Ruby"),
    (".rs", "Rust"),
    (".scss", "CSS"),
    (".sh", "Shell"),
    (".sql", "SQL"),
    (".svelte", "Svelte"),
    (".swift", "Swift"),
    (".tf", "Terraform"),
    (".toml", "TOML"),
    (".ts", "TypeScript"),
    (".tsx", "React"),
    (".vue", "Vue"),
    (".yaml", "YAML"),
    (".yml", "YAML"),
    (".zig", "Zig"),
];

/// Special-case filenames that have no extension but are well-known.
fn basename_language(basename: &str) -> Option<&'static str> {
    match basename {
        "Dockerfile" => Some("Dockerfile"),
        "Makefile" => Some("Makefile"),
        "Cargo.toml" => Some("TOML"),
        "CMakeLists.txt" => Some("CMake"),
        _ => None,
    }
}

/// Detect the programming language for a file path.
///
/// Returns `"Unknown"` if no mapping is found. Extensionless filenames like
/// `Dockerfile` are matched by basename first; otherwise the last extension is
/// used (so `foo.spec.ts` → "TypeScript").
pub fn detect_language(filename: &str) -> &'static str {
    let path = Path::new(filename);
    let basename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);

    // Special-case well-known extensionless files.
    if let Some(lang) = basename_language(basename) {
        return lang;
    }

    let ext = match path.extension().and_then(|s| s.to_str()) {
        Some(e) => format!(".{}", e),
        None => return "Unknown",
    };

    LANGUAGES
        .binary_search_by_key(&ext.as_str(), |(ext, _)| *ext)
        .map(|idx| LANGUAGES[idx].1)
        .unwrap_or("Unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_extensions() {
        assert_eq!(detect_language("src/main.rs"), "Rust");
        assert_eq!(detect_language("src/app.tsx"), "React");
        assert_eq!(detect_language("main.tf"), "Terraform");
        assert_eq!(detect_language("script.py"), "Python");
        assert_eq!(detect_language("styles.css"), "CSS");
    }

    #[test]
    fn unknown_extension() {
        assert_eq!(detect_language("weird.xyz"), "Unknown");
        assert_eq!(detect_language("noext"), "Unknown");
    }

    #[test]
    fn extensionless_basename() {
        assert_eq!(detect_language("Dockerfile"), "Dockerfile");
        assert_eq!(detect_language("Makefile"), "Makefile");
        assert_eq!(detect_language("Cargo.toml"), "TOML");
    }

    #[test]
    fn double_extension_last_wins() {
        assert_eq!(detect_language("component.spec.ts"), "TypeScript");
        assert_eq!(detect_language("test.integration.ts"), "TypeScript");
    }

    #[test]
    fn nested_paths() {
        assert_eq!(detect_language("a/b/c/deep.rs"), "Rust");
        assert_eq!(detect_language("/abs/path/to/file.go"), "Go");
    }
}
