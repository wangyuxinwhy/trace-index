//! Opt-in user-level OS service management for `trace-index index watch`.
//!
//! Installing the `trace-index` binary never registers any background process.
//! A user who wants the index kept fresh automatically can explicitly run
//! `trace-index index watch install` (and remove it cleanly at any time with
//! `trace-index index watch uninstall`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::storage::db::display_database_path;

/// Result returned by `index watch install` and `index watch uninstall`.
#[derive(Debug, Serialize)]
pub struct WatchServiceActionReport {
    pub action: String,
    pub platform: String,
    pub service_id: String,
    pub service_path: String,
    pub database: String,
    pub dry_run: bool,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loaded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

/// Snapshot of whether a user-level OS service definition is installed for one database.
#[derive(Debug, Clone)]
pub(crate) struct InstalledServiceInfo {
    pub(crate) service_id: String,
    pub(crate) service_path: PathBuf,
    pub(crate) installed: bool,
}

/// Configuration rendered into the OS service definition.
#[derive(Debug, Clone)]
pub(crate) struct WatchServiceSpec {
    pub(crate) binary_path: PathBuf,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) database_path: PathBuf,
    pub(crate) debounce_ms: u64,
    pub(crate) max_wait_ms: u64,
    pub(crate) reconcile_secs: u64,
}

/// Read-only inspection of whether a user-level OS service is installed for `database_path`.
pub(crate) fn inspect_service(database_path: &Path) -> Option<InstalledServiceInfo> {
    let descriptor = service_descriptor(database_path).ok()?;
    let installed = descriptor.service_path.is_file();
    Some(InstalledServiceInfo {
        service_id: descriptor.service_id,
        service_path: descriptor.service_path,
        installed,
    })
}

/// Installs (or previews with `dry_run`) a user-level OS background service for `index watch`.
pub(crate) fn install_service(
    spec: &WatchServiceSpec,
    dry_run: bool,
) -> Result<WatchServiceActionReport> {
    let descriptor = service_descriptor(&spec.database_path)?;
    let definition = render_service_definition(&descriptor, spec)?;

    if dry_run {
        return Ok(WatchServiceActionReport {
            action: "install".to_owned(),
            platform: descriptor.platform.to_owned(),
            service_id: descriptor.service_id,
            service_path: display_database_path(&descriptor.service_path),
            database: display_database_path(&spec.database_path),
            dry_run: true,
            installed: descriptor.service_path.is_file(),
            loaded: None,
            log_path: descriptor.log_path.as_deref().map(display_database_path),
            definition: Some(definition),
        });
    }

    if let Some(parent) = descriptor.service_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create service directory {}", parent.display()))?;
    }
    if let Some(parent) = descriptor.log_path.as_deref().and_then(Path::parent) {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create watch log directory {}", parent.display())
        })?;
    }
    fs::write(&descriptor.service_path, &definition).with_context(|| {
        format!(
            "failed to write watch service definition {}",
            descriptor.service_path.display()
        )
    })?;

    load_os_service(&descriptor)?;

    Ok(WatchServiceActionReport {
        action: "install".to_owned(),
        platform: descriptor.platform.to_owned(),
        service_id: descriptor.service_id,
        service_path: display_database_path(&descriptor.service_path),
        database: display_database_path(&spec.database_path),
        dry_run: false,
        installed: true,
        loaded: Some(true),
        log_path: descriptor.log_path.as_deref().map(display_database_path),
        definition: None,
    })
}

/// Uninstalls (or previews with `dry_run`) the user-level OS background service for `database_path`.
pub(crate) fn uninstall_service(
    database_path: &Path,
    dry_run: bool,
) -> Result<WatchServiceActionReport> {
    let descriptor = service_descriptor(database_path)?;
    let existed = descriptor.service_path.is_file();

    if dry_run {
        return Ok(WatchServiceActionReport {
            action: "uninstall".to_owned(),
            platform: descriptor.platform.to_owned(),
            service_id: descriptor.service_id,
            service_path: display_database_path(&descriptor.service_path),
            database: display_database_path(database_path),
            dry_run: true,
            installed: existed,
            loaded: None,
            log_path: None,
            definition: None,
        });
    }

    unload_os_service(&descriptor)?;
    if existed {
        fs::remove_file(&descriptor.service_path).with_context(|| {
            format!(
                "failed to remove watch service definition {}",
                descriptor.service_path.display()
            )
        })?;
    }
    reload_after_uninstall(&descriptor);

    Ok(WatchServiceActionReport {
        action: "uninstall".to_owned(),
        platform: descriptor.platform.to_owned(),
        service_id: descriptor.service_id,
        service_path: display_database_path(&descriptor.service_path),
        database: display_database_path(database_path),
        dry_run: false,
        installed: false,
        loaded: Some(false),
        log_path: None,
        definition: None,
    })
}

struct ServiceDescriptor {
    platform: &'static str,
    service_id: String,
    service_path: PathBuf,
    log_path: Option<PathBuf>,
}

fn database_suffix(database_path: &Path) -> String {
    let normalized = display_database_path(database_path);
    let hash = blake3::hash(normalized.as_bytes()).to_hex();
    hash.as_str()[..8].to_owned()
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set; cannot locate user service directory")
}

fn service_descriptor(database_path: &Path) -> Result<ServiceDescriptor> {
    let home = home_dir()?;
    let tag = database_suffix(database_path);
    if cfg!(target_os = "macos") {
        let service_id = format!("dev.trace-index.watch.{tag}");
        let service_path = home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{service_id}.plist"));
        let log_path = home
            .join("Library")
            .join("Logs")
            .join("trace-index")
            .join(format!("{service_id}.log"));
        return Ok(ServiceDescriptor {
            platform: "macos-launchd",
            service_id,
            service_path,
            log_path: Some(log_path),
        });
    }
    if cfg!(target_os = "linux") {
        let service_id = format!("trace-index-watch-{tag}.service");
        let config_root = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map_or_else(|| home.join(".config"), PathBuf::from);
        let service_path = config_root.join("systemd").join("user").join(&service_id);
        return Ok(ServiceDescriptor {
            platform: "linux-systemd-user",
            service_id,
            service_path,
            log_path: None,
        });
    }
    bail!(
        "user service installation is supported on macOS (launchd) and Linux (systemd --user); \
         run `trace-index index watch run` directly under your preferred supervisor"
    )
}

fn watch_argv(spec: &WatchServiceSpec) -> Vec<String> {
    let mut args = vec![display_database_path(&spec.binary_path)];
    if let Some(config_path) = spec.config_path.as_deref() {
        args.push("--config".to_owned());
        args.push(display_database_path(config_path));
    }
    args.push("--db".to_owned());
    args.push(display_database_path(&spec.database_path));
    args.push("index".to_owned());
    args.push("watch".to_owned());
    args.push("run".to_owned());
    args.push("--debounce-ms".to_owned());
    args.push(spec.debounce_ms.to_string());
    args.push("--max-wait-ms".to_owned());
    args.push(spec.max_wait_ms.to_string());
    args.push("--reconcile-secs".to_owned());
    args.push(spec.reconcile_secs.to_string());
    args
}

fn render_service_definition(
    descriptor: &ServiceDescriptor,
    spec: &WatchServiceSpec,
) -> Result<String> {
    let args = watch_argv(spec);
    match descriptor.platform {
        "macos-launchd" => {
            let log_path = descriptor
                .log_path
                .as_deref()
                .context("macos-launchd service descriptor missing log path")?;
            Ok(render_launchd_plist(
                &descriptor.service_id,
                &args,
                log_path,
            ))
        }
        "linux-systemd-user" => Ok(render_systemd_unit(&args)),
        other => bail!("unsupported watch service platform {other}"),
    }
}

fn render_launchd_plist(service_id: &str, args: &[String], log_path: &Path) -> String {
    let mut rendered_args = String::new();
    for arg in args {
        rendered_args.push_str("    <string>");
        rendered_args.push_str(&xml_escape(arg));
        rendered_args.push_str("</string>\n");
    }
    let log_str = xml_escape(&display_database_path(log_path));
    let label = xml_escape(service_id);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{rendered_args}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Background</string>
  <key>LowPriorityIO</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log_str}</string>
  <key>StandardErrorPath</key>
  <string>{log_str}</string>
</dict>
</plist>
"#
    )
}

fn render_systemd_unit(args: &[String]) -> String {
    let exec_start = args
        .iter()
        .map(|arg| systemd_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
         Description=Trace Index Event-Driven Watcher\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec_start}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         Nice=10\n\
         IOSchedulingClass=idle\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn systemd_quote(arg: &str) -> String {
    let escaped = arg.replace('\\', "\\\\").replace('"', "\\\"");
    if escaped.contains(char::is_whitespace) || escaped.is_empty() {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

fn current_uid() -> Result<String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .context("failed to determine current user id via `id -u`")?;
    if !output.status.success() {
        bail!("`id -u` exited with status {}", output.status);
    }
    let uid = String::from_utf8(output.stdout)
        .context("`id -u` output is not UTF-8")?
        .trim()
        .to_owned();
    if uid.is_empty() {
        bail!("`id -u` returned an empty user id");
    }
    Ok(uid)
}

fn load_os_service(descriptor: &ServiceDescriptor) -> Result<()> {
    match descriptor.platform {
        "macos-launchd" => {
            const LAUNCHD_BOOTSTRAP_ATTEMPTS: usize = 10;
            const LAUNCHD_BOOTSTRAP_RETRY_DELAY: std::time::Duration =
                std::time::Duration::from_millis(150);

            let domain = format!("gui/{}", current_uid()?);
            let target = format!("{domain}/{}", descriptor.service_id);
            // Unload any previous instance first. `launchctl bootout` tears down
            // running jobs asynchronously, so retry transient `bootstrap` exit-5
            // (EIO) failures while launchd finishes releasing the service slot
            // (including right after a separate `index watch uninstall`).
            let _ = Command::new("launchctl")
                .args(["bootout", &target])
                .output();
            let mut last_output = None;
            for attempt in 0..LAUNCHD_BOOTSTRAP_ATTEMPTS {
                if attempt > 0 {
                    std::thread::sleep(LAUNCHD_BOOTSTRAP_RETRY_DELAY);
                }
                let output = Command::new("launchctl")
                    .arg("bootstrap")
                    .arg(&domain)
                    .arg(&descriptor.service_path)
                    .output()
                    .context("failed to execute `launchctl bootstrap`")?;
                if output.status.success() {
                    return Ok(());
                }
                let retryable_eio = output.status.code() == Some(5);
                last_output = Some(output);
                if !retryable_eio {
                    break;
                }
            }
            if let Some(output) = last_output {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!(
                    "`launchctl bootstrap` failed with status {}: {}",
                    output.status,
                    stderr.trim()
                );
            }
            Ok(())
        }
        "linux-systemd-user" => {
            run_systemctl(&["--user", "daemon-reload"])?;
            run_systemctl(&["--user", "enable", "--now", &descriptor.service_id])
        }
        other => bail!("unsupported watch service platform {other}"),
    }
}

fn unload_os_service(descriptor: &ServiceDescriptor) -> Result<()> {
    match descriptor.platform {
        "macos-launchd" => {
            let domain = format!("gui/{}", current_uid()?);
            let target = format!("{domain}/{}", descriptor.service_id);
            let _ = Command::new("launchctl")
                .args(["bootout", &target])
                .output();
            Ok(())
        }
        "linux-systemd-user" => {
            let _ = Command::new("systemctl")
                .args(["--user", "disable", "--now", &descriptor.service_id])
                .output();
            Ok(())
        }
        other => bail!("unsupported watch service platform {other}"),
    }
}

fn reload_after_uninstall(descriptor: &ServiceDescriptor) {
    if descriptor.platform == "linux-systemd-user" {
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
    }
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("failed to execute `systemctl {}`", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`systemctl {}` failed with status {}: {}",
            args.join(" "),
            output.status,
            stderr.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        WatchServiceSpec, database_suffix, render_launchd_plist, render_systemd_unit, watch_argv,
    };

    #[test]
    fn distinct_databases_get_deterministic_isolated_service_suffixes() {
        let a = database_suffix(Path::new("/tmp/a.sqlite"));
        let a_again = database_suffix(Path::new("/tmp/a.sqlite"));
        let b = database_suffix(Path::new("/tmp/b.sqlite"));
        assert_eq!(a.len(), 8);
        assert_eq!(a, a_again);
        assert_ne!(a, b);
    }

    #[test]
    fn renders_valid_launchd_plist_and_systemd_unit_with_explicit_args() {
        let spec = WatchServiceSpec {
            binary_path: Path::new("/usr/local/bin/trace-index").to_path_buf(),
            config_path: Some(Path::new("/Users/me/.config/trace-index/config.toml").to_path_buf()),
            database_path: Path::new("/Users/me/my traces/index.sqlite").to_path_buf(),
            debounce_ms: 2500,
            max_wait_ms: 12_000,
            reconcile_secs: 300,
        };
        let args = watch_argv(&spec);
        let plist = render_launchd_plist(
            "dev.trace-index.watch.deadbeef",
            &args,
            Path::new("/Users/me/Library/Logs/trace-index/watch.log"),
        );
        assert!(plist.contains("<string>dev.trace-index.watch.deadbeef</string>"));
        assert!(plist.contains("<string>Background</string>"));
        assert!(plist.contains("<key>LowPriorityIO</key>"));
        assert!(plist.contains("<string>2500</string>"));
        assert!(plist.contains("<string>/Users/me/my traces/index.sqlite</string>"));

        let unit = render_systemd_unit(&args);
        assert!(unit.contains("ExecStart=/usr/local/bin/trace-index --config"));
        assert!(unit.contains("\"/Users/me/my traces/index.sqlite\""));
        assert!(unit.contains("Nice=10"));
        assert!(unit.contains("IOSchedulingClass=idle"));
    }
}
