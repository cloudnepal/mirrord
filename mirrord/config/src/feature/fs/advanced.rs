use std::collections::HashMap;

use mirrord_analytics::{AnalyticValue, CollectAnalytics};
use mirrord_config_derive::MirrordConfig;
use schemars::JsonSchema;
use serde::Serialize;

use super::{FsModeConfig, FsUserConfig};
use crate::{
    config::{from_env::FromEnv, source::MirrordConfigSource, ConfigContext, ConfigError},
    util::{MirrordToggleableConfig, VecOrSingle},
};

// TODO(alex): We could turn this derive macro (`MirrordConfig`) into an attribute version, which
// would allow us to "capture" the `derive` statement, making it possible to implement the same for
// whatever is generated by `map_to`.

/// Allows the user to specify the default behavior for file operations:
///
/// 1. `"read"` or `true` - Read from the remote file system (default)
/// 2. `"write"` - Read/Write from the remote file system.
/// 3. `"local"` or `false` - Read from the local file system.
/// 4. `"localwithoverrides"` - perform fs operation locally, unless the path matches a pre-defined
///    or user-specified exception.
///
/// > Note: by default, some paths are read locally or remotely, regardless of the selected FS mode.
/// > This is described in further detail below.
///
/// Besides the default behavior, the user can specify behavior for specific regex patterns.
/// Case insensitive.
///
/// 1. `"read_write"` - List of patterns that should be read/write remotely.
/// 2. `"read_only"` - List of patterns that should be read only remotely.
/// 3. `"local"` - List of patterns that should be read locally.
/// 4. `"not_found"` - List of patters that should never be read nor written. These files should be
///    treated as non-existent.
/// 4. `"mapping"` - Map of patterns and their corresponding replacers. The replacement happens before any specific behavior as defined above or mode (uses [`Regex::replace`](https://docs.rs/regex/latest/regex/struct.Regex.html#method.replace))
///
/// The logic for choosing the behavior is as follows:
///
///
/// 1. Check agains "mapping" if path needs to be replaced, if matched then continue to next step
///    with new path after replacements otherwise continue as usual.
/// 2. Check if one of the patterns match the file path, do the corresponding action. There's no
///    specified order if two lists match the same path, we will use the first one (and we do not
///    guarantee what is first).
///
///     **Warning**: Specifying the same path in two lists is unsupported and can lead to undefined
///     behaviour.
///
/// 3. There are pre-defined exceptions to the set FS mode.
///     1. Paths that match [the patterns defined here](https://github.com/metalbear-co/mirrord/tree/latest/mirrord/layer/src/file/filter/read_local_by_default.rs)
///        are read locally by default.
///     2. Paths that match [the patterns defined here](https://github.com/metalbear-co/mirrord/tree/latest/mirrord/layer/src/file/filter/read_remote_by_default.rs)
///        are read remotely by default when the mode is `localwithoverrides`.
///     3. Paths that match [the patterns defined here](https://github.com/metalbear-co/mirrord/tree/latest/mirrord/layer/src/file/filter/not_found_by_default.rs)
///        under the running user's home directory will not be found by the application when the
///        mode is not `local`.
///
///     In order to override that default setting for a path, or a pattern, include it the
///     appropriate pattern set from above. E.g. in order to read files under `/etc/` remotely even
///     though it is covered by [the set of patterns that are read locally by default](https://github.com/metalbear-co/mirrord/tree/latest/mirrord/layer/src/file/filter/read_local_by_default.rs),
///     add `"^/etc/."` to the `read_only` set.
///
/// 4. If none of the above match, use the default behavior (mode).
///
/// For more information, check the file operations
/// [technical reference](https://mirrord.dev/docs/reference/fileops/).
///
/// ```json
/// {
///   "feature": {
///     "fs": {
///       "mode": "write",
///       "read_write": ".+\\.json" ,
///       "read_only": [ ".+\\.yaml", ".+important-file\\.txt" ],
///       "local": [ ".+\\.js", ".+\\.mjs" ],
///       "not_found": [ "\\.config/gcloud" ]
///     }
///   }
/// }
/// ```
#[derive(MirrordConfig, Default, Clone, PartialEq, Eq, Debug, Serialize)]
#[config(
    map_to = "AdvancedFsUserConfig",
    derive = "PartialEq,Eq,JsonSchema",
    generator = "FsUserConfig"
)]
pub struct FsConfig {
    /// ### feature.fs.mode {#feature-fs-mode}
    #[config(nested)]
    pub mode: FsModeConfig,

    /// ### feature.fs.read_write {#feature-fs-read_write}
    ///
    /// Specify file path patterns that if matched will be read and written to the remote.
    #[config(env = "MIRRORD_FILE_READ_WRITE_PATTERN")]
    pub read_write: Option<VecOrSingle<String>>,

    /// ### feature.fs.read_only {#feature-fs-read_only}
    ///
    /// Specify file path patterns that if matched will be read from the remote.
    /// if file matching the pattern is opened for writing or read/write it will be opened locally.
    pub read_only: Option<VecOrSingle<String>>,

    /// ### feature.fs.local {#feature-fs-local}
    ///
    /// Specify file path patterns that if matched will be opened locally.
    #[config(env = "MIRRORD_FILE_LOCAL_PATTERN")]
    pub local: Option<VecOrSingle<String>>,

    /// ### feature.fs.not_found {#feature-fs-not_found}
    ///
    /// Specify file path patterns that if matched will be treated as non-existent.
    pub not_found: Option<VecOrSingle<String>>,

    /// ### feature.fs.mapping {#feature-fs-mapping}
    ///
    /// Specify map of patterns that if matched will replace the path according to specification.
    ///
    /// *Capture groups are allowed.*
    ///
    /// Example:
    /// ```json
    /// {
    ///   "^/home/(?<user>\\S+)/dev/tomcat": "/etc/tomcat"
    ///   "^/home/(?<user>\\S+)/dev/config/(?<app>\\S+)": "/mnt/configs/${user}-$app"
    /// }
    /// ```
    /// Will do the next replacements for any io operaton
    ///
    /// `/home/johndoe/dev/tomcat/context.xml` => `/etc/tomcat/context.xml`
    /// `/home/johndoe/dev/config/api/app.conf` => `/mnt/configs/johndoe-api/app.conf`
    ///
    /// - Relative paths: this feature (currently) does not apply mappings to relative paths, e.g.
    ///   `../dev`.
    pub mapping: Option<HashMap<String, String>>,
}

impl MirrordToggleableConfig for AdvancedFsUserConfig {
    fn disabled_config(context: &mut ConfigContext) -> Result<Self::Generated, ConfigError> {
        let mode = FsModeConfig::disabled_config(context)?;
        let read_write = FromEnv::new("MIRRORD_FILE_READ_WRITE_PATTERN")
            .source_value(context)
            .transpose()?;
        let read_only = FromEnv::new("MIRRORD_FILE_READ_ONLY_PATTERN")
            .source_value(context)
            .transpose()?;
        let local = FromEnv::new("MIRRORD_FILE_LOCAL_PATTERN")
            .source_value(context)
            .transpose()?;

        Ok(Self::Generated {
            mode,
            read_write,
            read_only,
            local,
            not_found: None,
            mapping: None,
        })
    }
}

impl FsConfig {
    pub fn is_read(&self) -> bool {
        self.mode.is_read()
    }

    pub fn is_write(&self) -> bool {
        self.mode.is_write()
    }

    /// Checks if fs operations are active
    pub fn is_active(&self) -> bool {
        !matches!(self.mode, FsModeConfig::Local)
    }
}

impl From<FsModeConfig> for AnalyticValue {
    fn from(mode: FsModeConfig) -> Self {
        match mode {
            FsModeConfig::Local => Self::Number(0),
            FsModeConfig::LocalWithOverrides => Self::Number(1),
            FsModeConfig::Read => Self::Number(2),
            FsModeConfig::Write => Self::Number(3),
        }
    }
}

impl CollectAnalytics for &FsConfig {
    fn collect_analytics(&self, analytics: &mut mirrord_analytics::Analytics) {
        analytics.add("mode", self.mode);
        analytics.add(
            "local_paths",
            self.local.as_deref().map(<[_]>::len).unwrap_or_default(),
        );
        analytics.add(
            "read_write_paths",
            self.read_write
                .as_deref()
                .map(<[_]>::len)
                .unwrap_or_default(),
        );
        analytics.add(
            "read_only_paths",
            self.read_only
                .as_deref()
                .map(<[_]>::len)
                .unwrap_or_default(),
        );
        analytics.add(
            "not_found_paths",
            self.not_found
                .as_deref()
                .map(<[_]>::len)
                .unwrap_or_default(),
        );
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::{config::MirrordConfig, util::testing::with_env_vars};

    #[rstest]
    fn advanced_fs_config_default() {
        let expect = FsConfig {
            mode: FsModeConfig::Read,
            ..Default::default()
        };

        with_env_vars(vec![], || {
            let mut cfg_context = ConfigContext::default();

            let fs_config = AdvancedFsUserConfig::default()
                .generate_config(&mut cfg_context)
                .unwrap();

            assert_eq!(fs_config, expect);
        });
    }
}
