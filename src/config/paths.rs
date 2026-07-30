//! Which configuration fields hold filesystem paths, and where a relative one
//! is measured from.
//!
//! The enumeration lives here so the two passes that walk it — anchoring, then
//! expansion in [`WorkspaceSettings::try_from_settings`] — cannot disagree.
//! Before, each carried its own list, and a path field added to the schema
//! would have joined one of them silently: expanded but never anchored (issue
//! #732 back for that field alone), or anchored but never expanded (a literal
//! `~` handed to the filesystem). Neither failure is a compile error, and
//! neither is loud at runtime.
//!
//! What the compiler enforces, precisely: [`path_fields_mut`] destructures
//! [`LanguageSettings`] and [`QueryItem`] exhaustively, so a new field on
//! either fails to build until someone classifies it. A new *top-level* path
//! field on `RawWorkspaceSettings` is not covered — `search_paths` reaches this
//! module as a parameter, because the two callers hold different shapes of it —
//! so adding one means adding it here by hand.
//!
//! [`WorkspaceSettings::try_from_settings`]: crate::config::WorkspaceSettings::try_from_settings

use super::settings::{LanguageSettings, QueryItem};
use path_clean::PathClean;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Every path-valued field, as a mutable borrow, in a deterministic order.
///
/// Languages are visited in name order so that a caller collecting errors
/// reports them the same way on every run, whatever the map's iteration order.
///
/// `search_paths` is a parameter rather than being read off a settings struct
/// because the two callers hold different shapes of it — `Option<Vec<String>>`
/// before conversion, `Vec<String>` after.
pub(crate) fn path_fields_mut<'a>(
    search_paths: Option<&'a mut Vec<String>>,
    languages: &'a mut HashMap<String, LanguageSettings>,
) -> impl Iterator<Item = &'a mut String> {
    let mut entries: Vec<(&'a String, &'a mut LanguageSettings)> = languages.iter_mut().collect();
    entries.sort_by_key(|(name, _)| *name);

    search_paths
        .into_iter()
        .flatten()
        .chain(entries.into_iter().flat_map(|(_, language)| {
            // Destructured exhaustively on purpose: adding a field to
            // `LanguageSettings` fails to compile here until someone decides
            // whether it holds a path.
            let LanguageSettings {
                parser,
                queries,
                // Not paths: a language name, a downstream server's own config,
                // injection layer settings, alternate language names, a flag.
                base: _,
                bridge: _,
                layers: _,
                aliases: _,
                auto_install: _,
            } = language;

            parser.iter_mut().chain(
                queries
                    .iter_mut()
                    .flatten()
                    // Exhaustive for the same reason as above.
                    .map(|QueryItem { path, kind: _ }| path),
            )
        }))
}

/// Whether a configured path already says where it lives, so anchoring must
/// leave it alone.
///
/// Three cases, and the last two are why this is a syntactic test rather than
/// `Path::is_absolute` alone — they are not absolute *yet*:
/// - rooted by the platform's own rule (`/usr/share`; on Windows also `C:\lib`
///   and the drive-relative `\lib`, which names a directory on the current
///   drive and so is not `is_absolute` despite already saying where it lives);
/// - `~`-led, which expansion turns into an absolute path — including `~user`,
///   which expansion deliberately passes through unchanged;
/// - `$`-led, which expansion resolves to wherever the variable points. Joining
///   a base onto that syntax would corrupt it before expansion reads it, and
///   deciding otherwise would mean expanding here — which would consume the
///   `${KAKEHASHI_DATA_DIR}` template the defaults layer depends on.
///
/// A bare `$$literal` is skipped too, even though it expands to the *relative*
/// literal `$literal`. Answering otherwise would require distinguishing the
/// escape from a variable reference, i.e. parsing the expansion syntax here.
/// To place any of these under the source directory, lead with `./`.
fn carries_its_own_base(path: &str) -> bool {
    // `has_root` rather than `is_absolute`: on Windows the two differ for a
    // drive-relative `\lib`, which anchoring must leave alone — rebasing it onto
    // a config directory would move it to that directory's drive. On Unix the
    // two agree.
    Path::new(path).has_root()
        || Path::new(path).is_absolute()
        || path.starts_with('~')
        || path.starts_with('$')
}

/// Rewrite every relative path field in `settings` to sit under `base`.
///
/// Called once per configuration layer, while the directory that layer came
/// from is still known — after the layers merge, a surviving `./queries` no
/// longer says which file asked for it, and the only base left is the server's
/// working directory, which belongs to whoever launched the editor.
///
/// This runs *before* expansion and deliberately stays syntactic; see
/// [`carries_its_own_base`] for which values it declines to touch and why.
///
/// Idempotent, and load-bearingly so: anchoring yields an absolute path, which
/// `carries_its_own_base` then skips. That is what lets `didChangeConfiguration`
/// merge a freshly anchored layer onto already-anchored stored settings, and the
/// post-install reload re-derive settings, without re-basing anything.
///
/// `base` is `None` for layers with no source directory (the programmed
/// defaults), where every value is left untouched.
pub(crate) fn anchor_settings_paths(
    settings: &mut crate::config::RawWorkspaceSettings,
    base: Option<&Path>,
) {
    let Some(base) = base else {
        return;
    };

    // Path fields are `String`, so a base the filesystem accepts but UTF-8 does
    // not cannot be represented here at all. `to_string_lossy` would substitute
    // U+FFFD and hand back a path that looks resolved and names a file that
    // does not exist — a silent redirection. Leaving the layer unanchored keeps
    // the pre-#732 meaning, which is at least a path the user can reason about,
    // and says so out loud.
    let Some(base) = base.to_str() else {
        log::warn!(
            target: "kakehashi::config",
            "Configuration directory {} is not valid UTF-8; its relative paths are left as written \
             and resolve against the working directory",
            base.display()
        );
        return;
    };

    // Anchoring must not *introduce* expansion syntax either. The expansion pass
    // reads `$VAR` anywhere in the value, not just at the front, so a base
    // directory that itself contains a `$` — `/data/$USER/proj`, a directory
    // literally named `a$b` — would be read as a variable reference in every
    // path this layer anchors. That fails the whole configuration when the name
    // is undefined, and silently resolves somewhere else when it happens to be
    // defined. `$$` is the documented escape for a literal `$`; it is what the
    // one expansion pass will turn back into the directory's real name, and
    // `clean` leaves it alone because it folds components, not characters.
    let base = PathBuf::from(base.replace('$', "$$"));

    for path in path_fields_mut(settings.search_paths.as_mut(), &mut settings.languages) {
        if carries_its_own_base(path) {
            continue;
        }
        let joined = base.join(&*path);
        // Folding `./` and `../` away keeps the stored value readable, but the
        // fold is lexical: `..` pops whatever component precedes it, and before
        // expansion that component may be a variable. Popping `$VAR` resolves
        // somewhere else the moment the variable holds more than one component,
        // and it takes any undefined-variable error down with it — expansion can
        // only reject a variable it still sees. So a value carrying both is left
        // unfolded for the kernel to resolve. Either alone is safe: with no
        // `..` there is nothing to pop, and with no variable there is nothing
        // whose value the fold could get wrong.
        //
        // The `.` segments that joining introduces are dropped either way —
        // a `.` pops nothing, so removing it cannot move the path.
        *path = if path.contains('$') && path.contains("..") {
            joined
                .components()
                .filter(|component| !matches!(component, std::path::Component::CurDir))
                .collect::<PathBuf>()
                .to_string_lossy()
                .into_owned()
        } else {
            joined.clean().to_string_lossy().into_owned()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::expand::{expand_path, make_env};
    use crate::config::settings::QueryItem;

    /// Build settings whose every path-valued field carries `value`, so one
    /// assertion covers `searchPaths`, `parser`, and `queries[].path` alike.
    fn settings_with_path(value: &str) -> crate::config::RawWorkspaceSettings {
        crate::config::RawWorkspaceSettings {
            search_paths: Some(vec![value.to_string()]),
            languages: [(
                "lua".to_string(),
                LanguageSettings {
                    parser: Some(value.to_string()),
                    queries: Some(vec![QueryItem {
                        path: value.to_string(),
                        kind: None,
                    }]),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        }
    }

    fn anchor(value: &str, base: Option<&Path>) -> String {
        let mut settings = settings_with_path(value);
        anchor_settings_paths(&mut settings, base);
        let paths: Vec<String> =
            path_fields_mut(settings.search_paths.as_mut(), &mut settings.languages)
                .map(|path| path.clone())
                .collect();
        assert_eq!(paths.len(), 3, "every path field should be visited");
        assert!(
            paths.iter().all(|path| path == &paths[0]),
            "every path field should follow the same rule: {paths:?}"
        );
        paths[0].clone()
    }

    /// The visitor is the contract both passes rely on: if it stops reaching a
    /// field, that field silently stops being anchored *and* expanded.
    #[test]
    fn path_fields_mut_reaches_every_path_valued_field() {
        let mut settings = settings_with_path("x");
        settings.languages.insert(
            "zig".to_string(),
            LanguageSettings {
                parser: Some("x".to_string()),
                ..Default::default()
            },
        );

        let count =
            path_fields_mut(settings.search_paths.as_mut(), &mut settings.languages).count();
        assert_eq!(
            count, 4,
            "1 searchPath + lua's parser and query + zig's parser"
        );
    }

    /// Errors are reported per field, so the walk must not depend on the map's
    /// iteration order.
    #[test]
    fn path_fields_mut_visits_languages_in_name_order() {
        let mut settings = crate::config::RawWorkspaceSettings {
            languages: [
                ("zig", "zig-parser"),
                ("lua", "lua-parser"),
                ("markdown", "markdown-parser"),
            ]
            .into_iter()
            .map(|(name, parser)| {
                (
                    name.to_string(),
                    LanguageSettings {
                        parser: Some(parser.to_string()),
                        ..Default::default()
                    },
                )
            })
            .collect(),
            ..Default::default()
        };

        let visited: Vec<String> = path_fields_mut(None, &mut settings.languages)
            .map(|path| path.clone())
            .collect();
        assert_eq!(visited, ["lua-parser", "markdown-parser", "zig-parser"]);
    }

    #[test]
    fn relative_paths_are_anchored_to_the_base() {
        assert_eq!(
            anchor("./queries/highlights.scm", Some(Path::new("/workspace"))),
            "/workspace/queries/highlights.scm"
        );
        assert_eq!(
            anchor("queries/highlights.scm", Some(Path::new("/workspace"))),
            "/workspace/queries/highlights.scm"
        );
    }

    #[test]
    fn parent_traversal_is_normalized_away() {
        assert_eq!(
            anchor("../shared/parsers", Some(Path::new("/workspace/project"))),
            "/workspace/shared/parsers"
        );
    }

    /// Anchoring runs before expansion, so it must not disturb the syntax the
    /// expansion pass reads — including the `$$` escape for a literal dollar.
    #[test]
    fn variables_survive_anchoring_unexpanded() {
        assert_eq!(
            anchor("./queries/$LANG.scm", Some(Path::new("/workspace"))),
            "/workspace/queries/$LANG.scm"
        );
        assert_eq!(
            anchor("./$$literal", Some(Path::new("/workspace"))),
            "/workspace/$$literal"
        );
    }

    /// A base directory may legally contain a `$`, and the expansion pass reads
    /// `$VAR` anywhere in a value — not just at the front. Anchoring therefore
    /// has to escape what it prepends, or every path in that layer would be
    /// rejected as an undefined variable, or worse, silently resolved to
    /// somewhere else when the name happens to be defined.
    #[test]
    fn a_base_containing_a_dollar_survives_the_expansion_pass() {
        let anchored = anchor("./queries", Some(Path::new("/work/a$b")));
        assert_eq!(anchored, "/work/a$$b/queries");

        let env = make_env(&[("b", "SHOULD NOT BE USED")]);
        assert_eq!(
            expand_path(&anchored, None, &env).expect("the escaped base must expand cleanly"),
            "/work/a$b/queries",
            "expansion must give back the directory's real name"
        );
    }

    /// Folding `..` lexically would pop the *unexpanded* component in front of
    /// it, which is not the same path and can be no path at all: the variable
    /// disappears along with the undefined-variable error the expansion pass
    /// owes the user, and a variable holding more than one component resolves
    /// somewhere else entirely. A value carrying a variable is therefore left
    /// unfolded for the filesystem to resolve.
    #[test]
    fn parent_traversal_after_a_variable_is_left_for_the_filesystem() {
        assert_eq!(
            anchor("./a/$VAR/../b", Some(Path::new("/base"))),
            "/base/a/$VAR/../b"
        );
        assert_eq!(
            anchor("./a/$UNDEFINED/../b", Some(Path::new("/base"))),
            "/base/a/$UNDEFINED/../b",
            "the undefined variable must still reach expansion, which rejects it"
        );
    }

    /// A value that expansion turns into an absolute path is left alone: its
    /// author already said where it lives, and joining a base onto a leading
    /// `~` or `$` would corrupt the syntax before expansion ever sees it.
    #[test]
    fn absolute_and_expandable_prefixes_are_not_anchored() {
        let base = Some(Path::new("/workspace"));
        assert_eq!(
            anchor("/opt/kakehashi/runtime", base),
            "/opt/kakehashi/runtime"
        );
        assert_eq!(anchor("~/parsers/lua.so", base), "~/parsers/lua.so");
        assert_eq!(anchor("$KAKEHASHI_DATA_DIR", base), "$KAKEHASHI_DATA_DIR");
        assert_eq!(
            anchor("${KAKEHASHI_DATA_DIR}", base),
            "${KAKEHASHI_DATA_DIR}"
        );
    }

    /// The opt-out rule, stated once so `docs/README.md` has something to match.
    #[test]
    fn carries_its_own_base_covers_every_opt_out_form() {
        for path in [
            "/usr/share/kakehashi",
            "~/parsers",
            "~",
            // Expansion passes `~user` through unchanged, so anchoring it would
            // produce a directory literally named `~bob` under the base.
            "~bob/parsers",
            "$KAKEHASHI_DATA_DIR",
            "${KAKEHASHI_DATA_DIR}/queries",
            // Skipped despite expanding to the relative literal `$literal`:
            // telling it apart from a variable means parsing the syntax here.
            "$$literal",
        ] {
            assert!(carries_its_own_base(path), "{path} should opt out");
        }

        for path in ["queries/x.scm", "./queries/x.scm", "../shared", ".", ""] {
            assert!(!carries_its_own_base(path), "{path} should be anchored");
        }
    }

    /// A Windows-rooted value must opt out on Windows, where neither a drive
    /// prefix nor a bare root starts with `/`. `\rooted` is the reason the
    /// predicate asks `has_root` and not only `is_absolute`: it names the
    /// current drive, so rebasing it would move it to the config file's drive.
    #[cfg(windows)]
    #[test]
    fn windows_rooted_paths_opt_out() {
        for path in [r"C:\parsers\lua.dll", r"\\server\share\lua.dll", r"\rooted"] {
            assert!(carries_its_own_base(path), "{path} should opt out");
        }
    }

    /// A base the filesystem accepts but UTF-8 does not cannot be written into
    /// a `String` path field. Anchoring lossily would name a file that does not
    /// exist while looking resolved, so the layer is left alone instead.
    #[cfg(unix)]
    #[test]
    fn a_non_unicode_base_leaves_paths_as_written() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let base = Path::new(OsStr::from_bytes(b"/tmp/proj-\xFF"));
        assert_eq!(anchor("./queries", Some(base)), "./queries");
    }

    /// One array, three rules: the relative entry moves, the two that carry
    /// their own base do not. The single-value helper above cannot express
    /// this, and a layer's `searchPaths` routinely mixes the forms.
    #[test]
    fn one_array_may_mix_anchored_and_untouched_entries() {
        let mut settings = crate::config::RawWorkspaceSettings {
            search_paths: Some(vec![
                "./runtime".into(),
                "/opt/kakehashi".into(),
                "${KAKEHASHI_DATA_DIR}".into(),
            ]),
            ..Default::default()
        };

        anchor_settings_paths(&mut settings, Some(Path::new("/workspace")));

        assert_eq!(
            settings.search_paths,
            Some(vec![
                "/workspace/runtime".to_string(),
                "/opt/kakehashi".to_string(),
                "${KAKEHASHI_DATA_DIR}".to_string(),
            ])
        );
    }

    /// Degenerate spellings of "here". Both name the source directory, which is
    /// what anchoring resolves them to — worth pinning because `expand_path`
    /// leaves an empty string empty, so the two passes disagree by design.
    #[test]
    fn empty_and_dot_resolve_to_the_base_itself() {
        assert_eq!(anchor("", Some(Path::new("/workspace"))), "/workspace");
        assert_eq!(anchor(".", Some(Path::new("/workspace"))), "/workspace");
    }

    /// The programmed defaults have no source directory. Without a base every
    /// value must survive verbatim, or `${KAKEHASHI_DATA_DIR}` would stop
    /// reaching the expansion pass that gives it a platform default.
    #[test]
    fn no_base_leaves_every_path_untouched() {
        assert_eq!(anchor("./runtime", None), "./runtime");
        assert_eq!(
            anchor("${KAKEHASHI_DATA_DIR}", None),
            "${KAKEHASHI_DATA_DIR}"
        );
    }
}
