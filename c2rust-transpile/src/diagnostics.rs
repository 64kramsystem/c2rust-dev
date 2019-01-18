use std::collections::HashSet;
use std::io;
use std::str::FromStr;
use fern::colors::ColoredLevelConfig;
use log::Level;

const DEFAULT_WARNINGS: &[Diagnostic] = &[
];

#[derive(PartialEq, Eq, Hash, Debug, Display, EnumString, Clone)]
#[strum(serialize_all = "kebab_case")]
pub enum Diagnostic {
    Comments,
}

macro_rules! diag {
    ($type:path, $($arg:tt)*) => (warn!(target: &$type.to_string(), $($arg)*))
}

pub fn init(mut enabled_warnings: HashSet<Diagnostic>) {
    enabled_warnings.extend(DEFAULT_WARNINGS.iter().cloned());

    let colors = ColoredLevelConfig::new();
    fern::Dispatch::new()
        .format(move |out, message, record| {
            let level_label = match record.level() {
                Level::Error => "error",
                Level::Warn => "warning",
                Level::Info => "info",
                Level::Debug => "debug",
                Level::Trace => "trace",
            };
            out.finish(format_args!(
                "\x1B[{}m{}:\x1B[0m {} [-W{}]",
                colors.get_color(&record.level()).to_fg_str(),
                level_label,
                message,
                record.target(),
            ))
        })
        .level(log::LevelFilter::Warn)
        .filter(move |metadata| {
            enabled_warnings.contains(&Diagnostic::from_str(metadata.target()).unwrap())
        })
        .chain(io::stderr())
        .apply()
        .expect("Could not set up diagnostics");
}
