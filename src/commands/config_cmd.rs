use serde_json::json;

use crate::cli::ConfigCmd;
use crate::config::{self, SETTINGS};
use crate::context::Ctx;
use crate::error::Result;
use crate::output::{to_record, Report};

pub fn run(ctx: &Ctx, cmd: ConfigCmd) -> Result<()> {
    let r = &ctx.resolved;
    match cmd {
        ConfigCmd::Show { effective } => {
            let rows = r
                .ordered()
                .into_iter()
                .filter(|(_, _, src)| effective || *src == config::Source::File)
                .map(|(k, v, src)| to_record(&json!({"key": k, "value": v, "source": src.as_str()})))
                .collect();
            ctx.emit(
                &Report::new("config", rows)
                    .meta("config_path", json!(r.config_path))
                    .meta("config_file_exists", json!(r.config_file_exists)),
            )
        }
        ConfigCmd::Set { key, value } => {
            config::write_setting(&r.config_path, &key, Some(&value))?;
            ctx.info(&format!("set {key} in {}", r.config_path.display()));
            let v = config::normalize(&key, &value)?;
            ctx.emit(&Report::new("config", vec![to_record(&json!({"key": key, "value": v, "source": "file"}))]))
        }
        ConfigCmd::Unset { key } => {
            config::write_setting(&r.config_path, &key, None)?;
            ctx.info(&format!("unset {key} in {}", r.config_path.display()));
            Ok(())
        }
        ConfigCmd::Path => ctx.emit(&Report::new(
            "config_path",
            vec![to_record(
                &json!({"path": r.config_path, "exists": r.config_file_exists, "source": r.config_path_source.as_str()}),
            )],
        )),
        ConfigCmd::Keys => {
            let rows = SETTINGS
                .iter()
                .map(|s| to_record(&json!({"key": s.key, "env": s.env.join(","), "choices": s.choices.join("|"), "help": s.help})))
                .collect();
            ctx.emit(&Report::new("config_keys", rows))
        }
    }
}
