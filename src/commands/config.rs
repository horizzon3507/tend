use anyhow::Result;

use crate::cli::ConfigArgs;
use crate::config;
use crate::ui;
use crate::util::tildify;

pub fn run(args: ConfigArgs) -> Result<i32> {
    let path = config::config_path();

    if args.path {
        println!("{}", path.display());
        return Ok(0);
    }

    if args.init {
        config::write_default()?;
        println!("config written to {}", ui::bold(&tildify(&path)));
        return Ok(0);
    }

    let loaded = config::load();
    ui::section("tend", Some("configuration"));
    ui::kv("file", &tildify(&path));
    ui::kv("data", &tildify(&config::data_dir()));
    ui::kv("roots", &loaded.config.roots.join(", "));
    ui::kv("depth", &loaded.config.max_depth.to_string());
    ui::kv("backups", &loaded.config.backup.keep.to_string());
    ui::kv(
        "target",
        &tildify(&config::backup_dest(&loaded.config, None)),
    );
    if loaded.created {
        ui::say_note("  file just created with default values");
    }
    ui::blank();
    Ok(0)
}
