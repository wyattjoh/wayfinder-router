use std::path::PathBuf;

pub const LAUNCHD_LABEL: &str = "com.wayfinder-router.gateway";
pub const SYSTEMD_UNIT_NAME: &str = "wayfinder-router.service";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServicePlatform {
    Macos,
    Linux,
    Other,
}

pub fn detect_platform(platform: Option<&str>) -> ServicePlatform {
    let platform = platform.unwrap_or(std::env::consts::OS);
    if platform == "macos" || platform == "darwin" {
        return ServicePlatform::Macos;
    }
    if platform.starts_with("linux") {
        return ServicePlatform::Linux;
    }
    ServicePlatform::Other
}

pub fn launchd_plist(program_args: &[String], label: &str, log_dir: &str) -> String {
    let args_xml = program_args
        .iter()
        .map(|arg| format!("      <string>{}</string>", xml_escape(arg)))
        .collect::<Vec<_>>()
        .join("\n");
    let logs = expand_home(log_dir).trim_end_matches('/').to_owned();
    [
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>".to_owned(),
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">".to_owned(),
        "<plist version=\"1.0\">".to_owned(),
        "<dict>".to_owned(),
        "  <key>Label</key>".to_owned(),
        format!("  <string>{}</string>", xml_escape(label)),
        "  <key>ProgramArguments</key>".to_owned(),
        "  <array>".to_owned(),
        args_xml,
        "  </array>".to_owned(),
        "  <key>RunAtLoad</key>".to_owned(),
        "  <true/>".to_owned(),
        "  <key>KeepAlive</key>".to_owned(),
        "  <true/>".to_owned(),
        "  <key>StandardOutPath</key>".to_owned(),
        format!("  <string>{logs}/wayfinder-router.log</string>"),
        "  <key>StandardErrorPath</key>".to_owned(),
        format!("  <string>{logs}/wayfinder-router.err.log</string>"),
        "</dict>".to_owned(),
        "</plist>".to_owned(),
        String::new(),
    ]
    .join("\n")
}

pub fn systemd_unit(program_args: &[String], description: &str) -> String {
    let exec_start = program_args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
Description={description}\n\
After=network-online.target\n\
\n\
[Service]\n\
ExecStart={exec_start}\n\
Restart=on-failure\n\
RestartSec=2\n\
\n\
[Install]\n\
WantedBy=default.target\n"
    )
}

pub fn agent_path(home: Option<PathBuf>) -> PathBuf {
    home.unwrap_or_else(home_dir)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

pub fn systemd_unit_path(home: Option<PathBuf>) -> PathBuf {
    home.unwrap_or_else(home_dir)
        .join(".config")
        .join("systemd")
        .join("user")
        .join(SYSTEMD_UNIT_NAME)
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b':' | b'=')
        })
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn expand_home(path: &str) -> String {
    if path == "~" {
        return home_dir().to_string_lossy().into_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest).to_string_lossy().into_owned();
    }
    path.to_owned()
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_detection_matches_python() {
        assert_eq!(detect_platform(Some("darwin")), ServicePlatform::Macos);
        assert_eq!(detect_platform(Some("macos")), ServicePlatform::Macos);
        assert_eq!(detect_platform(Some("linux")), ServicePlatform::Linux);
        assert_eq!(detect_platform(Some("linux2")), ServicePlatform::Linux);
        assert_eq!(detect_platform(Some("win32")), ServicePlatform::Other);
    }

    #[test]
    fn launchd_plist_escapes_args_and_expands_logs() {
        let plist = launchd_plist(
            &["/bin/x & <y>".to_owned(), "serve".to_owned()],
            LAUNCHD_LABEL,
            "~/Library/Logs",
        );
        assert!(plist.starts_with("<?xml version=\"1.0\""));
        assert!(plist.contains(&format!("<string>{LAUNCHD_LABEL}</string>")));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(plist.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(plist.contains("<string>/bin/x &amp; &lt;y&gt;</string>"));
        assert!(!plist.contains("<string>~/"));
        assert!(plist.contains("/Library/Logs/wayfinder-router.log"));
    }

    #[test]
    fn systemd_unit_quotes_arguments() {
        let unit = systemd_unit(
            &[
                "/opt/my router/wayfinder-router".to_owned(),
                "serve".to_owned(),
            ],
            "Wayfinder router gateway",
        );
        assert!(unit.contains("ExecStart='/opt/my router/wayfinder-router' serve"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn unit_paths_use_given_home() {
        let home = PathBuf::from("/home/tester");
        assert_eq!(
            agent_path(Some(home.clone())),
            home.join("Library/LaunchAgents/com.wayfinder-router.gateway.plist")
        );
        assert_eq!(
            systemd_unit_path(Some(home.clone())),
            home.join(".config/systemd/user/wayfinder-router.service")
        );
    }
}
