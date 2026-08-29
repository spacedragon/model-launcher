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
}
