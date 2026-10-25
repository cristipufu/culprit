//! Shared CLI discovery for terminal and desktop launches. Only the child environment is extended;
//! shell startup files are never loaded. Batch launchers use Rust's native Windows argument escaping.
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use culprit_core::{Error, Result};
use tokio::process::Command;

#[derive(Clone, Copy)]
pub(crate) enum Cli {
    Azure,
    Aws,
    Ssh,
}

#[derive(Clone, Copy)]
enum Platform {
    Macos,
    Linux,
    Windows,
}

impl Cli {
    fn names(self, platform: Platform) -> &'static [&'static str] {
        match (self, platform) {
            (Self::Azure, Platform::Windows) => &["az.cmd", "az.exe", "az.bat"],
            (Self::Aws, Platform::Windows) => &["aws.exe", "aws.cmd", "aws.bat"],
            (Self::Azure, _) => &["az"],
            (Self::Aws, _) => &["aws"],
            (Self::Ssh, Platform::Windows) => &["ssh.exe"],
            (Self::Ssh, _) => &["ssh"],
        }
    }
}

pub(crate) fn command(cli: Cli) -> Result<Command> {
    let platform = if cfg!(windows) {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::Macos
    } else {
        Platform::Linux
    };
    let home = dirs::home_dir();
    let mut extra = locations(cli, platform, home.as_deref(), |key| std::env::var_os(key));
    // ProxyCommand tools (for example AWS SSM) need the same GUI-safe child PATH as direct cloud calls.
    for other in [Cli::Azure, Cli::Aws, Cli::Ssh] {
        for path in locations(other, platform, home.as_deref(), |key| std::env::var_os(key)) {
            if !extra.contains(&path) {
                extra.push(path);
            }
        }
    }
    let inherited = std::env::var_os("PATH");
    build(cli, platform, inherited.as_deref(), &extra)
}

fn locations(
    cli: Cli,
    platform: Platform,
    home: Option<&Path>,
    env: impl Fn(&str) -> Option<OsString>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if !matches!(platform, Platform::Windows) {
        if let Some(bin) = env("XDG_BIN_HOME") {
            paths.push(bin.into());
        }
        if let Some(home) = home {
            paths.extend([home.join(".local/bin"), home.join("bin")]);
        }
        if matches!(platform, Platform::Macos) {
            paths.push("/opt/homebrew/bin".into());
        }
        paths.extend(["/usr/local/bin".into(), "/usr/bin".into(), "/bin".into(), "/usr/sbin".into(), "/sbin".into()]);
        if matches!(platform, Platform::Linux) {
            paths.extend(["/snap/bin".into(), "/home/linuxbrew/.linuxbrew/bin".into()]);
        }
    } else {
        for variable in ["SystemRoot", "WINDIR"] {
            if let Some(root) = env(variable) {
                let root = PathBuf::from(root);
                paths.extend([root.join("System32"), root.join("System32/WindowsPowerShell/v1.0")]);
            }
        }
        if matches!(cli, Cli::Ssh) {
            for variable in ["SystemRoot", "WINDIR"] {
                if let Some(root) = env(variable) {
                    paths.push(PathBuf::from(root).join("System32").join("OpenSSH"));
                }
            }
        }
        for variable in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = env(variable) {
                let root = PathBuf::from(root);
                paths.push(match cli {
                    Cli::Azure => root.join("Microsoft SDKs").join("Azure").join("CLI2").join("wbin"),
                    Cli::Aws => root.join("Amazon").join("AWSCLIV2"),
                    Cli::Ssh => root.join("OpenSSH"),
                });
                if matches!(cli, Cli::Aws) {
                    paths.push(root.join("Amazon").join("SessionManagerPlugin").join("bin"));
                }
            }
        }
        if let Some(root) = env("LOCALAPPDATA") {
            let root = PathBuf::from(root);
            if matches!(cli, Cli::Aws) {
                paths.push(root.join("Programs").join("Amazon").join("AWSCLIV2"));
            }
            paths.extend([
                root.join("Microsoft").join("WinGet").join("Links"),
                root.join("Microsoft").join("WindowsApps"),
            ]);
        }
        if let Some(root) = env("SCOOP") {
            paths.push(PathBuf::from(root).join("shims"));
        } else if let Some(home) = home {
            paths.push(home.join("scoop").join("shims"));
        }
        if let Some(root) = env("ChocolateyInstall") {
            paths.push(PathBuf::from(root).join("bin"));
        } else if let Some(root) = env("ProgramData") {
            paths.push(PathBuf::from(root).join("chocolatey").join("bin"));
        }
    }
    paths.retain(|path| path.is_absolute());
    paths
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else { return false };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}

fn build(cli: Cli, platform: Platform, inherited: Option<&OsStr>, extra: &[PathBuf]) -> Result<Command> {
    let mut paths: Vec<PathBuf> =
        inherited.into_iter().flat_map(std::env::split_paths).filter(|path| path.is_absolute()).collect();
    for path in extra {
        if !paths.contains(path) {
            paths.push(path.clone());
        }
    }
    let names = cli.names(platform);
    let binary =
        paths.iter().flat_map(|dir| names.iter().map(move |name| dir.join(name))).find(|path| executable(path));
    let binary = binary.ok_or_else(|| match cli {
        Cli::Azure => Error::not_found("The Azure CLI was not found in PATH or standard install locations")
            .with_hint("Install the Azure CLI and run az login"),
        Cli::Aws => Error::not_found("The AWS CLI was not found in PATH or standard install locations")
            .with_hint("Install AWS CLI v2 and sign in with aws login or aws sso login"),
        Cli::Ssh => Error::not_found("Could not find the OpenSSH client")
            .with_hint("Install OpenSSH Client on Windows, or the openssh client package on Linux; macOS includes ssh"),
    })?;
    let mut command = Command::new(binary);
    if let Ok(path) = std::env::join_paths(&paths) {
        command.env("PATH", path);
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "culprit-cli-path-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn cli(&self, dir: &str, name: &str) -> PathBuf {
            let path = self.0.join(dir).join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"test CLI").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn child_path(command: &Command) -> OsString {
        command.as_std().get_envs().find(|(key, _)| *key == "PATH").unwrap().1.unwrap().into()
    }

    #[test]
    fn windows_openssh_is_found_without_relying_on_the_gui_path() {
        let temp = Temp::new();
        let chosen = temp.cli("Windows/System32/OpenSSH", "ssh.exe");
        let paths = locations(Cli::Ssh, Platform::Windows, None, |key| {
            (key == "SystemRoot").then(|| temp.0.join("Windows").into_os_string())
        });
        assert_eq!(build(Cli::Ssh, Platform::Windows, None, &paths).unwrap().as_std().get_program(), chosen);
    }

    #[test]
    fn relative_path_entries_and_missing_tools_cannot_fall_back_to_the_current_directory() {
        let temp = Temp::new();
        let inherited = std::env::join_paths([PathBuf::from("."), PathBuf::from("relative-tools")]).unwrap();
        assert!(build(Cli::Ssh, Platform::Windows, Some(&inherited), &[]).is_err());
        let chosen = temp.cli("absolute-tools", "ssh.exe");
        assert_eq!(
            build(Cli::Ssh, Platform::Windows, Some(&inherited), &[temp.0.join("absolute-tools")])
                .unwrap()
                .as_std()
                .get_program(),
            chosen
        );
    }

    #[test]
    fn inherited_path_precedes_fallbacks_and_is_extended_only_for_the_child() {
        let temp = Temp::new();
        let chosen = temp.cli("custom", "az");
        temp.cli("fallback", "az");
        let inherited = std::env::join_paths([temp.0.join("custom")]).unwrap();
        let original = std::env::var_os("PATH");
        let command = build(Cli::Azure, Platform::Linux, Some(&inherited), &[temp.0.join("fallback")]).unwrap();
        assert_eq!(command.as_std().get_program(), chosen);
        assert_eq!(
            std::env::split_paths(&child_path(&command)).collect::<Vec<_>>(),
            vec![temp.0.join("custom"), temp.0.join("fallback")]
        );
        assert_eq!(std::env::var_os("PATH"), original);
    }

    #[test]
    fn gui_paths_find_user_installs() {
        for platform in [Platform::Macos, Platform::Linux] {
            let temp = Temp::new();
            let chosen = temp.cli(".local/bin", "aws");
            let paths = locations(Cli::Aws, platform, Some(&temp.0), |_| None);
            let command = build(Cli::Aws, platform, None, &paths).unwrap();
            assert_eq!(command.as_std().get_program(), chosen);
        }
    }

    #[cfg(unix)]
    #[test]
    fn macos_locations_include_both_homebrew_install_prefixes() {
        let paths = locations(Cli::Azure, Platform::Macos, None, |_| None);
        assert!(
            paths.contains(&PathBuf::from("/opt/homebrew/bin")) && paths.contains(&PathBuf::from("/usr/local/bin"))
        );
    }

    #[test]
    fn windows_installer_locations_include_batch_launchers_and_per_user_aws() {
        let temp = Temp::new();
        let az = temp.cli("Program Files/Microsoft SDKs/Azure/CLI2/wbin", "az.cmd");
        let aws = temp.cli("Local AppData/Programs/Amazon/AWSCLIV2", "aws.exe");
        let env = |key: &str| match key {
            "ProgramFiles" => Some(temp.0.join("Program Files").into_os_string()),
            "LOCALAPPDATA" => Some(temp.0.join("Local AppData").into_os_string()),
            _ => None,
        };
        for (cli, expected) in [(Cli::Azure, az), (Cli::Aws, aws)] {
            let paths = locations(cli, Platform::Windows, None, env);
            assert_eq!(build(cli, Platform::Windows, None, &paths).unwrap().as_std().get_program(), expected);
        }
    }

    #[test]
    fn windows_path_executables_override_installer_defaults() {
        let temp = Temp::new();
        let chosen = temp.cli("portable tools", "az.exe");
        temp.cli("system", "az.cmd");
        let path = std::env::join_paths([temp.0.join("portable tools")]).unwrap();
        assert_eq!(
            build(Cli::Azure, Platform::Windows, Some(&path), &[temp.0.join("system")]).unwrap().as_std().get_program(),
            chosen
        );
    }

    #[test]
    fn xdg_bin_home_and_windows_system_installers_are_found() {
        let temp = Temp::new();
        let aws = temp.cli("xdg bin", "aws");
        let paths = locations(Cli::Aws, Platform::Linux, None, |key| {
            (key == "XDG_BIN_HOME").then(|| temp.0.join("xdg bin").into_os_string())
        });
        assert_eq!(build(Cli::Aws, Platform::Linux, None, &paths).unwrap().as_std().get_program(), aws);

        for (cli, variable, dir, name) in [
            (Cli::Azure, "ProgramFiles(x86)", "Program Files (x86)/Microsoft SDKs/Azure/CLI2/wbin", "az.cmd"),
            (Cli::Aws, "ProgramW6432", "Program Files/Amazon/AWSCLIV2", "aws.exe"),
        ] {
            let expected = temp.cli(dir, name);
            let paths = locations(cli, Platform::Windows, None, |key| {
                (key == variable).then(|| {
                    temp.0
                        .join(if variable == "ProgramFiles(x86)" { "Program Files (x86)" } else { "Program Files" })
                        .into_os_string()
                })
            });
            assert_eq!(build(cli, Platform::Windows, None, &paths).unwrap().as_std().get_program(), expected);
        }
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn resolved_launchers_run_from_paths_with_spaces_and_preserve_arguments() {
        let temp = Temp::new();
        #[cfg(unix)]
        let (platform, name, script, expected) =
            (Platform::Linux, "az", "#!/usr/bin/env sh\nprintf '%s\\n' \"$@\"\n", "alpha beta\nnext\n");
        #[cfg(windows)]
        let (platform, name, script, expected) = (
            Platform::Windows,
            "az.cmd",
            "@echo off\r\nif not \"%~1\"==\"alpha beta\" exit /b 7\r\nif not \"%~2\"==\"next\" exit /b 8\r\necho fixture ok\r\n",
            "fixture ok\r\n",
        );
        let path = temp.cli("tools & spaces", name);
        std::fs::write(&path, script).unwrap();
        let mut paths = vec![path.parent().unwrap().to_owned()];
        paths.extend(locations(Cli::Azure, platform, None, |_| None));
        let mut command = build(Cli::Azure, platform, None, &paths).unwrap();
        command.args(["alpha beta", "next"]);
        let output = crate::cloud_process::output(command).await.unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }

    #[test]
    fn a_new_install_is_found_without_restarting_or_loading_shell_profiles() {
        let temp = Temp::new();
        let paths = vec![temp.0.join("tools")];
        assert!(build(Cli::Azure, Platform::Linux, None, &paths).is_err());
        let installed = temp.cli("tools", "az");
        assert_eq!(build(Cli::Azure, Platform::Linux, None, &paths).unwrap().as_std().get_program(), installed);
    }

    #[cfg(unix)]
    #[test]
    fn nonexecutable_files_do_not_hide_a_working_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let temp = Temp::new();
        let invalid = temp.cli("bad", "az");
        let chosen = temp.cli("good", "az");
        std::fs::set_permissions(&invalid, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path = std::env::join_paths([temp.0.join("bad")]).unwrap();
        assert_eq!(
            build(Cli::Azure, Platform::Linux, Some(&path), &[temp.0.join("good")]).unwrap().as_std().get_program(),
            chosen
        );
    }
}
