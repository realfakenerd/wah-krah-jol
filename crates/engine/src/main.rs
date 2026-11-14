use engine::config::{ConfigAction, EngineConfig, HELP_TEXT};

/// Exit code for a command line the engine cannot use.
const EXIT_USAGE: i32 = 2;

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    match EngineConfig::from_env() {
        Ok(ConfigAction::Run(config)) => match engine::run(*config) {
            Err(error) if error.downcast_ref::<engine::app::DeviceLost>().is_some() => {
                eprintln!("error: {error}");
                std::process::exit(i32::from(engine::app::EXIT_DEVICE_LOST));
            }
            result => result,
        },
        Ok(ConfigAction::Help) => {
            print!("{HELP_TEXT}");
            Ok(())
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(EXIT_USAGE);
        }
    }
}
