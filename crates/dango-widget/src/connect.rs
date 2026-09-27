//! One-click "connect" for the built-in balls: start the vendor's *own*
//! login — its official CLI in a visible Terminal window, or its App — and
//! keep reading whatever credentials that login leaves behind. Dango
//! never speaks any vendor's login protocol itself.
//!
//! Commands are fixed per plan (no user input reaches them). A CLI login runs
//! from a `.command` script in the app's own directory so Terminal gives it a
//! real TTY and the user's own shell environment.

use std::path::{Path, PathBuf};

/// How a plan gets (re)connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    /// Official CLI and its login arguments, tried first when installed.
    pub cli: Option<(&'static str, &'static [&'static str])>,
    /// macOS App to open when there is no CLI (or as the only way).
    pub app: Option<&'static str>,
    /// Where to get it when neither is installed.
    pub download: &'static str,
}

pub fn recipe(plan_id: &str) -> Option<Recipe> {
    Some(match plan_id {
        "claude" => Recipe {
            cli: None,
            app: Some("Claude"),
            download: "https://claude.ai/download",
        },
        "dim" => Recipe {
            cli: None,
            app: Some("DimAgent"),
            download: "https://dimagent.cn",
        },
        "grok" => Recipe {
            cli: Some(("grok", &["login"])),
            app: None,
            download: "https://x.ai/cli",
        },
        "haze" => Recipe {
            cli: None,
            app: Some("Haze"),
            download: "https://usehaze.ai",
        },
        "cursor" => Recipe {
            cli: Some(("cursor-agent", &["login"])),
            app: Some("Cursor"),
            download: "https://cursor.com/downloads",
        },
        "devin" => Recipe {
            cli: Some(("devin", &["auth", "login"])),
            app: Some("Devin"),
            download: "https://devin.ai",
        },
        // `droid` has no login subcommand; the App (or droid's own
        // interactive first run) signs in.
        "factory" => Recipe {
            cli: None,
            app: Some("Factory"),
            download: "https://factory.ai",
        },
        _ => return None,
    })
}

/// Where a CLI may live. launchd gives us a bare PATH, so look in the usual
/// install spots instead of trusting it.
fn find_cli(name: &str, home: &Path) -> Option<PathBuf> {
    [
        home.join(".local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".cursor/bin"),
        home.join(".devin/bin"),
        home.join(".grok/bin"),
    ]
    .into_iter()
    .map(|dir| dir.join(name))
    .find(|path| path.is_file())
}

fn app_installed(app: &str, home: &Path) -> bool {
    [PathBuf::from("/Applications"), home.join("Applications")]
        .iter()
        .any(|dir| dir.join(format!("{app}.app")).exists())
}

/// Single-quote for zsh (the path is ours, but a home dir may have spaces).
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn login_script(label: &str, cli: &Path, args: &[&str]) -> String {
    let command = std::iter::once(shell_quote(&cli.to_string_lossy()))
        .chain(args.iter().map(|arg| shell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "#!/bin/zsh -l\n\
         echo '用 {label} 官方命令登录：浏览器会打开，授权后回到这里。'\n\
         echo\n\
         {command}\n\
         echo\n\
         echo '好了就回到 Dango，小球会自己刷新。这个窗口可以关掉。'\n"
    )
}

/// Is there anything on this machine a built-in ball could read from?
pub fn installed(plan_id: &str, home: &Path) -> bool {
    if plan_id == "antigravity" {
        return home.join(".antigravity-bridge").exists() || app_installed("Antigravity", home);
    }
    recipe(plan_id).is_some_and(|recipe| {
        recipe
            .cli
            .is_some_and(|(cli, _)| find_cli(cli, home).is_some())
            || recipe.app.is_some_and(|app| app_installed(app, home))
    })
}

/// First run: start with the balls this machine can actually fill; the rest
/// wait in "添加小球" instead of crying on the capsule.
pub fn first_run_hidden(home: &Path) -> Vec<String> {
    dango_lib::settings::BUILTIN_PLANS
        .iter()
        .filter(|plan| !installed(plan, home))
        .map(|plan| plan.to_string())
        .collect()
}

/// What `connect` did, for the page to say.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    /// `terminal` | `app` | `download`
    pub action: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

pub fn connect(plan_id: &str) -> Result<Outcome, String> {
    let recipe = recipe(plan_id).ok_or_else(|| format!("{plan_id} 没有一键连接"))?;
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is unavailable")?);
    if let Some((cli_name, args)) = recipe.cli {
        if let Some(cli) = find_cli(cli_name, &home) {
            let dir = dango_lib::settings::settings_dir()?.join("connect");
            std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            let script = dir.join(format!("{plan_id}-login.command"));
            let label = recipe.app.unwrap_or(cli_name);
            std::fs::write(&script, login_script(label, &cli, args))
                .map_err(|error| error.to_string())?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| error.to_string())?;
            open(&[script.as_os_str()])?;
            return Ok(Outcome {
                action: "terminal",
                message: format!("已在终端里运行 {cli_name} 的官方登录"),
                url: None,
            });
        }
    }
    if let Some(app) = recipe.app.filter(|app| app_installed(app, &home)) {
        open(&[std::ffi::OsStr::new("-a"), std::ffi::OsStr::new(app)])?;
        return Ok(Outcome {
            action: "app",
            message: format!("已打开 {app}，在里面登录就行"),
            url: None,
        });
    }
    Ok(Outcome {
        action: "download",
        message: "这台电脑上没装它，先去官网下载".into(),
        url: Some(recipe.download.into()),
    })
}

fn open(args: &[&std::ffi::OsStr]) -> Result<(), String> {
    let status = std::process::Command::new("/usr/bin/open")
        .args(args)
        .status()
        .map_err(|error| format!("open: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("open 失败（{status}）"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_ball_but_gemini_has_a_recipe() {
        for plan in ["claude", "haze", "cursor", "devin", "factory"] {
            let recipe = recipe(plan).unwrap();
            assert!(recipe.cli.is_some() || recipe.app.is_some(), "{plan}");
            assert!(recipe.download.starts_with("https://"));
        }
        // Gemini connects through its own "+ 添加账号"; custom balls take a key.
        assert!(recipe("antigravity").is_none());
        assert!(recipe("custom-deepseek").is_none());
        assert!(recipe("../etc").is_none());
    }

    #[test]
    fn the_login_script_quotes_paths_with_spaces_and_quotes() {
        let script = login_script(
            "Cursor",
            Path::new("/Users/a b/it's/cursor-agent"),
            &["login"],
        );
        assert!(script.contains(r"'/Users/a b/it'\''s/cursor-agent' 'login'"));
        assert!(script.starts_with("#!/bin/zsh -l\n"));
    }

    #[test]
    fn a_first_run_hides_what_this_machine_has_nothing_for() {
        // Only look under the temp home: `app_installed` also checks
        // /Applications, so pick plans by what the fake home provides and
        // assert on the ones that can only come from it.
        let home = std::env::temp_dir().join(format!("dango-connect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        std::fs::write(home.join(".local/bin/cursor-agent"), b"").unwrap();
        std::fs::create_dir_all(home.join(".antigravity-bridge")).unwrap();
        assert!(installed("cursor", &home));
        assert!(installed("antigravity", &home));
        let hidden = first_run_hidden(&home);
        assert!(!hidden.contains(&"cursor".to_string()));
        assert!(!hidden.contains(&"antigravity".to_string()));
        let _ = std::fs::remove_dir_all(&home);
    }
}
