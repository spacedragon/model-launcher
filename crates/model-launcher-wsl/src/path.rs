use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WslPathError {
    #[error("path must be an absolute Windows drive path")]
    InvalidRoot,
    #[error("path traversal is not allowed")]
    Traversal,
}

pub fn windows_to_wsl_path(value: &str) -> Result<String, WslPathError> {
    if value.starts_with(r#"\\"#) || value.len() < 3 {
        return Err(WslPathError::InvalidRoot);
    }
    let bytes = value.as_bytes();
    let drive = bytes[0];
    if !drive.is_ascii_alphabetic() || bytes[1] != b':' || !matches!(bytes[2], b'\\' | b'/') {
        return Err(WslPathError::InvalidRoot);
    }
    let components: Vec<_> = value[3..].split(['\\', '/']).collect();
    if components.contains(&"..") {
        return Err(WslPathError::Traversal);
    }
    if components
        .iter()
        .any(|component| invalid_windows_component(component))
    {
        return Err(WslPathError::InvalidRoot);
    }
    let suffix = components
        .into_iter()
        .filter(|component| !component.is_empty() && *component != ".")
        .collect::<Vec<_>>()
        .join("/");
    let drive = char::from(drive).to_ascii_lowercase();
    Ok(if suffix.is_empty() {
        format!("/mnt/{drive}")
    } else {
        format!("/mnt/{drive}/{suffix}")
    })
}
fn invalid_windows_component(component: &str) -> bool {
    if component.is_empty() {
        return false;
    }
    if component.contains(':') || component.ends_with(['.', ' ']) {
        return true;
    }
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
        || stem
            .strip_prefix("LPT")
            .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_drive_paths_preserving_spaces_unicode_and_normalizing_case() {
        assert_eq!(
            windows_to_wsl_path(r#"c:\Models\有 空格\m.gguf"#).unwrap(),
            "/mnt/c/Models/有 空格/m.gguf"
        );
        assert_eq!(
            windows_to_wsl_path(r#"Z:/Models/m.gguf"#).unwrap(),
            "/mnt/z/Models/m.gguf"
        );
    }

    #[test]
    fn rejects_relative_unc_traversal_and_invalid_roots() {
        for value in [
            "model.gguf",
            r#"\\server\share\m.gguf"#,
            r#"C:\Models\..\m.gguf"#,
            r#"1:\m.gguf"#,
            r#"C:relative"#,
        ] {
            assert!(windows_to_wsl_path(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn rejects_ads_reserved_dos_names_and_trailing_dot_or_space() {
        for value in [
            r#"C:\models\file.gguf:stream"#,
            r#"C:\CON"#,
            r#"C:\con.txt"#,
            r#"C:\aux.GGUF"#,
            r#"C:\COM9.bin"#,
            r#"C:\lpt1"#,
            r#"C:\model. "#,
            r#"C:\model."#,
        ] {
            assert!(windows_to_wsl_path(value).is_err(), "accepted {value:?}");
        }
    }
}
