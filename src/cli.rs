//! Strict startup-mode selection, without config reads or process side effects.
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupMode {
    Gameplay,
    TestZone,
    Preview(PathBuf),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliArgs {
    pub mode: StartupMode,
    pub profile: bool,
    pub config: Option<String>,
    pub api_port: Option<u16>,
    pub agent_socket: Option<PathBuf>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedCli {
    Help,
    Run(CliArgs),
}

/// Parse an argv slice including its program name, without loading any files.
pub fn parse_args(args: &[String]) -> Result<ParsedCli, String> {
    let mut testzone = false;
    let mut preview = None;
    let mut cli = CliArgs {
        mode: StartupMode::Gameplay,
        profile: false,
        config: None,
        api_port: None,
        agent_socket: None,
    };
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "-h" | "--help" => return Ok(ParsedCli::Help),
            "--testzone" => testzone = true,
            "--profile" => cli.profile = true,
            _ if arg == "--config" || arg.starts_with("--config=") => {
                cli.config = Some(value(args, &mut index, "--config")?);
            }
            _ if arg == "--api-port" || arg.starts_with("--api-port=") => {
                let value = value(args, &mut index, "--api-port")?;
                let port = value
                    .parse::<u16>()
                    .ok()
                    .filter(|&port| port > 0)
                    .ok_or_else(|| format!("--api-port must be a number 1-65535, got '{value}'"))?;
                cli.api_port = Some(port);
            }
            _ if arg == "--agent-socket" || arg.starts_with("--agent-socket=") => {
                cli.agent_socket = Some(PathBuf::from(value(args, &mut index, "--agent-socket")?));
            }
            _ if arg == "--preview-glb" || arg.starts_with("--preview-glb=") => {
                if preview.is_some() {
                    return Err("--preview-glb may only occur once".into());
                }
                preview = Some(PathBuf::from(value(args, &mut index, "--preview-glb")?));
            }
            other => return Err(format!("unrecognized argument '{other}'")),
        }
        index += 1;
    }
    if let Some(path) = preview {
        // Keep this exhaustive: a new CliArgs field requires an explicit preview policy.
        let CliArgs {
            mode: _,
            profile,
            config,
            api_port: _,
            agent_socket,
        } = &cli;
        if testzone || config.is_some() || *profile || agent_socket.is_some() {
            return Err("--preview-glb cannot be combined with --testzone, --config, --profile or --agent-socket".into());
        }
        cli.mode = StartupMode::Preview(path);
    } else if testzone {
        cli.mode = StartupMode::TestZone;
    }
    Ok(ParsedCli::Run(cli))
}
fn value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    let value = if let Some((_, value)) = args[*index].split_once('=') {
        value.to_owned()
    } else {
        match args.get(*index + 1) {
            Some(value) if !value.starts_with('-') => {
                *index += 1;
                value.clone()
            }
            _ => {
                let hint = match flag {
                    "--config" => "a profile name or config file path",
                    "--api-port" => "a TCP port 1-65535",
                    "--agent-socket" => "a filesystem path",
                    "--preview-glb" => "a GLB file path",
                    _ => "an argument value",
                };
                return Err(format!("{flag} requires a value ({hint})"));
            }
        }
    };
    if value.is_empty() {
        return Err(format!("{flag} requires a non-empty value"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<ParsedCli, String> {
        let args: Vec<String> = std::iter::once("eqoxide")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect();
        parse_args(&args)
    }
    fn run(args: &[&str]) -> CliArgs {
        match parse(args).unwrap() {
            ParsedCli::Run(cli) => cli,
            ParsedCli::Help => panic!("unexpected help"),
        }
    }
    #[test]
    fn default_and_legacy_modes_preserve_flags() {
        let cli = run(&[]);
        assert_eq!(cli.mode, StartupMode::Gameplay);
        assert_eq!(
            (
                cli.profile,
                cli.config,
                cli.api_port,
                cli.agent_socket
            ),
            (false, None, None, None)
        );
        let cli = run(&[
            "--testzone",
            "--config",
            "inspector",
            "--profile",
            "--api-port=65535",
            "--agent-socket",
            "control.sock",
        ]);
        assert_eq!(cli.mode, StartupMode::TestZone);
        assert!(cli.profile);
        assert_eq!(cli.config.as_deref(), Some("inspector"));
        assert_eq!(cli.api_port, Some(65535));
        assert_eq!(cli.agent_socket, Some(PathBuf::from("control.sock")));
    }
    #[test]
    fn both_preview_forms_are_explicit_and_config_free() {
        for flags in [
            vec!["--preview-glb", "scene.glb"],
            vec!["--preview-glb=scene.glb"],
        ] {
            let cli = run(&flags);
            assert_eq!(cli.mode, StartupMode::Preview(PathBuf::from("scene.glb")));
            assert_eq!(cli.config, None);
            assert_eq!(cli.api_port, None);
        }
        assert_eq!(
            run(&["--preview-glb", "scene with spaces.glb", "--api-port", "1"]).api_port,
            Some(1)
        );
    }
    #[test]
    fn preview_conflicts_are_order_independent() {
        for conflict in [
            vec!["--testzone"],
            vec!["--config", "missing.yml"],
            vec!["--profile"],
            vec!["--agent-socket", "control.sock"],
        ] {
            for preview_first in [true, false] {
                let mut flags = vec!["--preview-glb=scene.glb"];
                if preview_first {
                    flags.extend(&conflict);
                } else {
                    flags = conflict.iter().copied().chain(flags).collect();
                }
                assert!(
                    parse(&flags).unwrap_err().contains("--preview-glb"),
                    "{flags:?}"
                );
            }
        }
    }
    #[test]
    fn malformed_missing_duplicate_or_unknown_flags_fail() {
        for flags in [
            vec!["--preview-glb"],
            vec!["--preview-glb="],
            vec!["--preview-glb", "--testzone"],
            vec!["--preview-glb=a.glb", "--preview-glb=b.glb"],
            vec!["--preview-glb=a.glb", "unknown"],
            vec!["--unknown"],
            vec!["--config"],
            vec!["--config="],
            vec!["--agent-socket="],
            vec!["--api-port=0"],
            vec!["--api-port=65536"],
            vec!["--api-port=-1"],
            vec!["--api-port=no"],
            vec!["--api-port"],
            vec!["--api-port", "--help"],
        ] {
            assert!(parse(&flags).is_err(), "{flags:?}");
        }
    }
    #[test]
    fn help_is_a_successful_non_launch_action() {
        assert_eq!(parse(&["--help"]).unwrap(), ParsedCli::Help);
        assert_eq!(parse(&["-h"]).unwrap(), ParsedCli::Help);
        assert_eq!(
            parse(&["--preview-glb=scene.glb", "--help"]).unwrap(),
            ParsedCli::Help
        );
    }
    #[test]
    fn legacy_value_forms_and_last_value_behavior_remain_available() {
        let cli = run(&[
            "--config=first",
            "--config",
            "second",
            "--api-port=1",
            "--api-port=2",
            "--agent-socket=control.sock",
        ]);
        assert_eq!(cli.config.as_deref(), Some("second"));
        assert_eq!(cli.api_port, Some(2));
        assert_eq!(cli.agent_socket, Some(PathBuf::from("control.sock")));
    }
    #[test]
    fn missing_flag_values_keep_actionable_argument_hints() {
        for (flag, hint) in [
            ("--config", "a profile name or config file path"),
            ("--api-port", "a TCP port 1-65535"),
            ("--agent-socket", "a filesystem path"),
            ("--preview-glb", "a GLB file path"),
        ] {
            for args in [vec![flag], vec![flag, "--help"]] {
                assert!(parse(&args).unwrap_err().contains(hint));
            }
            assert!(parse(&[&format!("{flag}=")])
                .unwrap_err()
                .contains("non-empty"));
        }
    }
}
