include!(concat!(env!("OUT_DIR"), "/package_version.rs"));

pub(crate) const fn removal_is_due(current_major: u64, remove_in_major: u64) -> bool {
    current_major >= remove_in_major
}

macro_rules! enforce_deprecation_deadline {
    (
        name = $name:literal,
        deprecated_in = $deprecated_in:literal,
        remove_in = $remove_in:literal $(,)?
    ) => {
        const _: () = {
            assert!(
                $deprecated_in < $remove_in,
                "a deprecation must precede its removal major"
            );
            assert!(
                !$crate::deprecation::removal_is_due(
                    $crate::deprecation::PACKAGE_MAJOR,
                    $remove_in,
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

pub(crate) use enforce_deprecation_deadline;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v0_deprecation_remains_valid_through_v1_and_expires_in_v2() {
        assert!(!removal_is_due(0, 2));
        assert!(!removal_is_due(1, 2));
        assert!(removal_is_due(2, 2));
    }
}
