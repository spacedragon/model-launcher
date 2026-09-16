//! Shared helpers for adapter-owned launch arguments.
//!
//! Adapters append `Runtime::fixed_args` last so administrators can add
//! engine-specific tuning. Because later flags win for most engines, a fixed
//! arg that spells an adapter-owned flag would let an administrator override
//! security or identity invariants (host, port, model identity, generated
//! config). [`fixed_arg_collision`] detects that collision so adapters can
//! reject it instead of emitting an unsafe argv.

/// Return the first administrator fixed arg that collides with an
/// adapter-owned flag, if any.
///
/// Each `fixed_args` element is compared as one exact argv element. The text
/// before any `=` is matched, so the space spelling (`--host 0.0.0.0`, two
/// elements) and the equals spelling (`--host=0.0.0.0`, one element) are both
/// caught when `--host` is owned, as are owned aliases such as `-m` for
/// `--model`. A space *inside* one element (`--model /tmp/evil.gguf`) is not a
/// separator: it stays a single, non-matching argument because that is exactly
/// how the child receives it. The original arg is returned so the error can
/// name exactly what the administrator supplied.
///
/// # Examples
///
/// ```
/// use model_serving_runtime::fixed_arg_collision;
///
/// let fixed = vec!["--threads".to_owned(), "--host=0.0.0.0".to_owned()];
/// assert_eq!(fixed_arg_collision(&fixed, &["--host"]), Some("--host=0.0.0.0"));
/// assert_eq!(fixed_arg_collision(&fixed, &["-m"]), None);
/// assert_eq!(fixed_arg_collision(&["--threads".to_owned()], &["--host"]), None);
/// // Embedded spaces are one argv element, not two flags.
/// assert_eq!(fixed_arg_collision(&["--host 0.0.0.0".to_owned()], &["--host"]), None);
/// ```
#[must_use]
pub fn fixed_arg_collision<'a>(fixed_args: &'a [String], owned_flags: &[&str]) -> Option<&'a str> {
    fixed_args.iter().find_map(|arg| {
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        owned_flags.contains(&flag).then_some(arg.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::fixed_arg_collision;

    #[test]
    fn accepts_harmless_fixed_args() {
        let fixed = vec![
            "--threads".to_owned(),
            "8".to_owned(),
            "--admin-extra=1".to_owned(),
        ];
        assert_eq!(fixed_arg_collision(&fixed, &["--host", "--port"]), None);
    }

    #[test]
    fn rejects_the_space_spelling() {
        let fixed = vec!["--host".to_owned(), "0.0.0.0".to_owned()];
        assert_eq!(fixed_arg_collision(&fixed, &["--host"]), Some("--host"));
    }

    #[test]
    fn rejects_the_equals_spelling() {
        let fixed = vec!["--host=0.0.0.0".to_owned()];
        assert_eq!(
            fixed_arg_collision(&fixed, &["--host"]),
            Some("--host=0.0.0.0")
        );
    }

    #[test]
    fn rejects_owned_aliases() {
        // Mirrors the llama.cpp adapter's full alias set: every spelling of an
        // owned flag must be caught, canonical and alias alike.
        let owned = [
            "--model",
            "-m",
            "--host",
            "--port",
            "--alias",
            "-a",
            "--ctx-size",
            "-c",
            "--parallel",
            "-np",
            "--batch-size",
            "-b",
            "--flash-attn",
            "-fa",
            "--no-kv-offload",
            "-nkvo",
            "--n-gpu-layers",
            "--gpu-layers",
            "-ngl",
        ];
        for alias in owned {
            let fixed = vec![alias.to_owned()];
            assert_eq!(
                fixed_arg_collision(&fixed, &owned),
                Some(alias),
                "owned alias {alias} must be caught"
            );
        }
    }

    #[test]
    fn rejects_an_owned_alias_with_the_equals_spelling() {
        let fixed = vec!["-m=/tmp/evil.gguf".to_owned()];
        assert_eq!(
            fixed_arg_collision(&fixed, &["--model", "-m"]),
            Some("-m=/tmp/evil.gguf")
        );
    }

    #[test]
    fn embedded_space_is_one_argument_not_two_flags() {
        // The child receives this as a single argv element, so it cannot
        // override `--model`; the helper must not split on whitespace.
        let fixed = vec!["--model /tmp/evil.gguf".to_owned()];
        assert_eq!(fixed_arg_collision(&fixed, &["--model", "-m"]), None);
    }

    #[test]
    fn embedded_space_before_equals_is_one_argument() {
        let fixed = vec!["--model =/tmp/evil.gguf".to_owned()];
        assert_eq!(fixed_arg_collision(&fixed, &["--model", "-m"]), None);
    }

    #[test]
    fn returns_the_first_collision_in_order() {
        let fixed = vec![
            "--port=9".to_owned(),
            "--host".to_owned(),
            "0.0.0.0".to_owned(),
        ];
        assert_eq!(
            fixed_arg_collision(&fixed, &["--host", "--port"]),
            Some("--port=9")
        );
    }
}
