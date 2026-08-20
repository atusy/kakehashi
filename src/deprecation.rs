include!(concat!(env!("OUT_DIR"), "/package_version.rs"));

#[derive(Clone, Copy)]
pub(crate) struct Deprecation {
    remove_in_major: u64,
}

impl Deprecation {
    pub(crate) const fn new(remove_in_major: u64) -> Self {
        Self { remove_in_major }
    }

    pub(crate) const fn remove_in_major(self) -> u64 {
        self.remove_in_major
    }
}

pub(crate) const fn removal_is_due(current_major: u64, remove_in_major: u64) -> bool {
    current_major >= remove_in_major
}

macro_rules! declare_deprecation {
    (
        $visibility:vis const $policy:ident;
        name = $name:literal,
        deprecated_in = $deprecated_in:literal,
        remove_in = $remove_in:literal $(,)?
    ) => {
        $visibility const $policy: $crate::deprecation::Deprecation = {
            let remove_in: u64 = $remove_in;
            $crate::deprecation::Deprecation::new(remove_in)
        };
        const _: () = {
            let deprecated_in: u64 = $deprecated_in;
            let remove_in: u64 = $remove_in;
            assert!(
                deprecated_in < remove_in,
                "a deprecation must precede its removal major"
            );
            assert!(
                !$crate::deprecation::removal_is_due(
                    $crate::deprecation::PACKAGE_MAJOR,
                    remove_in,
                ),
                concat!(
                    "`",
                    $name,
                    "` was deprecated in v",
                    stringify!($deprecated_in),
                    " and must be removed before releasing v",
                    stringify!($remove_in),
                )
            );
        };
    };
}

macro_rules! declare_deprecation_notice {
    (
        $(#[$attribute:meta])*
        $visibility:vis const $notice:ident;
        name = $name:literal,
        deprecated_in = $deprecated_in:literal,
        remove_in = $remove_in:literal,
        message = $message:literal $(,)?
    ) => {
        $(#[$attribute])*
        $visibility const $notice: &str = {
            let deprecated_in: u64 = $deprecated_in;
            let remove_in: u64 = $remove_in;
            assert!(
                deprecated_in < remove_in,
                "a deprecation must precede its removal major"
            );
            assert!(
                !$crate::deprecation::removal_is_due(
                    $crate::deprecation::PACKAGE_MAJOR,
                    remove_in,
                ),
                concat!(
                    "`",
                    $name,
                    "` was deprecated in v",
                    stringify!($deprecated_in),
                    " and must be removed before releasing v",
                    stringify!($remove_in),
                )
            );
            concat!($message, stringify!($remove_in), ".")
        };
    };
}

pub(crate) use {declare_deprecation, declare_deprecation_notice};

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    fn compile_fixture(version: &str) -> std::process::Output {
        let fixture = tempfile::tempdir().expect("fixture temp dir");
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let deprecation_module = manifest_dir.join("src/deprecation.rs");
        let module_path = deprecation_module
            .to_str()
            .expect("repository path must be valid UTF-8");

        fs::create_dir(fixture.path().join("src")).expect("fixture src dir");
        fs::copy(
            manifest_dir.join("build.rs"),
            fixture.path().join("build.rs"),
        )
        .expect("copy production build script");
        fs::write(
            fixture.path().join("Cargo.toml"),
            format!(
                "[package]\nname = \"deprecation-deadline-fixture\"\nversion = \"{version}\"\nedition = \"2024\"\n\n[workspace]\n"
            ),
        )
        .expect("write fixture manifest");
        fs::write(
            fixture.path().join("src/lib.rs"),
            format!(
                r#"
#[path = {module_path:?}]
mod deprecation;

mod policies {{
    crate::deprecation::declare_deprecation_notice!(
        #[cfg(any())]
        const DISABLED_EXPIRED_NOTICE;
        name = "disabled expired path",
        deprecated_in = 0,
        remove_in = 1,
        message = "disabled expired path is removed in v"
    );
    crate::deprecation::declare_deprecation_notice!(
        const V0_NOTICE;
        name = "v0 path",
        deprecated_in = 0,
        remove_in = 2,
        message = "v0 path is removed in v"
    );
    crate::deprecation::declare_deprecation!(
        const V1_POLICY;
        name = "v1 path",
        deprecated_in = 1,
        remove_in = 3,
    );
    crate::deprecation::declare_deprecation!(
        const LARGE_MAJOR_POLICY;
        name = "large major path",
        deprecated_in = 4_294_967_296,
        remove_in = 4_294_967_297,
    );
}}
"#
            ),
        )
        .expect("write fixture library");

        Command::new(env!("CARGO"))
            .args(["check", "--offline", "--quiet"])
            .current_dir(fixture.path())
            .env("CARGO_TARGET_DIR", fixture.path().join("target"))
            .output()
            .expect("run cargo check for deadline fixture")
    }

    #[test]
    fn v0_deprecation_remains_valid_through_v1_and_expires_in_v2() {
        assert!(!removal_is_due(0, 2));
        assert!(!removal_is_due(1, 2));
        assert!(removal_is_due(2, 2));
    }

    #[test]
    fn compile_gate_tracks_cargo_major_and_keeps_generations_independent() {
        let v1 = compile_fixture("1.0.0");
        assert!(
            v1.status.success(),
            "enabled policies and cfg-disabled deadlines must compile in v1: {}",
            String::from_utf8_lossy(&v1.stderr)
        );

        let v2 = compile_fixture("2.0.0");
        assert!(!v2.status.success(), "the v0 policy must expire in v2");
        let stderr = String::from_utf8_lossy(&v2.stderr);
        assert!(
            stderr
                .contains("`v0 path` was deprecated in v0 and must be removed before releasing v2"),
            "the failure must identify the expired policy: {stderr}"
        );
        assert!(
            !stderr.contains("`v1 path` was deprecated"),
            "the independently scheduled v1 policy must remain valid in v2: {stderr}"
        );
    }
}
