use crate::utils::wrap_output;

use super::{
    utils, ServiceInstallCtx, ServiceLevel, ServiceManager, ServiceStartCtx, ServiceStopCtx,
    ServiceUninstallCtx,
};
use plist::{Dictionary, Value};
use std::{
    borrow::Cow,
    ffi::OsStr,
    io,
    path::PathBuf,
    process::{Command, Output, Stdio}
};

static LAUNCHCTL: &str = "launchctl";
const PLIST_FILE_PERMISSIONS: u32 = 0o644;

/// Target domain provided as the second argument to 'launchctl <domain-target>'
/// man launchctl
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchdTargetDomain {
    /// gui/<uid>/[service-name]
    UserGui,
    // /// user/<uid>/[service-name]
    // User, 
    // /// login/<asid>/[service-name]
    // UserAsid, 
    // /// pid/<pid>/[service-name]
    // Process,
    /// system/[service-name]
    // SystemAgent, 
    /// system/[service-name]
    System, 
}


/// Directory at which the plist file is located.
pub struct LaunchdDirectory(PathBuf);
impl LaunchdDirectory {
    /// Per-user agents provided by the user.
    pub fn user_agent(username: &str) -> Self {
        Self(PathBuf::from(&format!("/Users/{username}/Library/LaunchAgents")))
    }

    /// Per-user agents provided by the administrator.
    pub fn admin_agent() -> Self {
        Self(PathBuf::from("/Library/LaunchAgents"))
    }

    /// System wide daemons provided by the administrator.
    pub fn admin_daemon() -> Self {
        Self(PathBuf::from("/Library/LaunchDaemons"))
    }

    /// OS X Per-user agents.
    pub fn system_agent() -> Self {
        Self(PathBuf::from("/System/Library/LaunchAgents"))
    }

    /// OS X System wide daemons.
    pub fn system_daemon() -> Self {
        Self(PathBuf::from("/System/Library/LaunchDaemons"))
    }
}

/// Configuration settings tied to launchd services
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchdConfig {
    pub install: LaunchdInstallConfig,
}

/// Configuration settings tied to launchd services during installation
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchdInstallConfig {
    /// If true, will include `KeepAlive` flag set to true
    pub keep_alive: bool,
}

impl Default for LaunchdInstallConfig {
    fn default() -> Self {
        Self { keep_alive: true }
    }
}

/// Implementation of [`ServiceManager`] for MacOS's [Launchd](https://en.wikipedia.org/wiki/Launchd)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchdServiceManager {
    /// For user-level context, provide user <uid> and <username>. 
    pub user: Option<UserContext>,
    /// Targe domain, which is provided to `launchctl print/bootstrap/bootout/..` as the second argument.
    pub target_domain: LaunchdTargetDomain,
    /// Configuration settings tied to launchd services
    pub config: LaunchdConfig,
}

impl Default for LaunchdServiceManager {
    fn default() -> Self {
        Self { user: None, target_domain: LaunchdTargetDomain::System, config: LaunchdConfig::default()}
    }
}
pub type UserContext = (u32, String);
impl LaunchdServiceManager {
    /// Creates a new manager instance working with system services
    pub fn system() -> Self {
        Self::default()
    }

    /// Creates a new manager instance working with user services
    pub fn user(uid: u32, username: String) -> Self {
        Self {
            user: Some((uid, username)),
            target_domain: LaunchdTargetDomain::UserGui,
            config: LaunchdConfig::default()
        }
    }

    /// Update manager to use the specified config
    pub fn with_config(self, config: LaunchdConfig) -> Self {
        Self {
            config,
            user: self.user,
            target_domain: self.target_domain,
        }
    }

    fn get_plist_path(&self, qualified_name: &str) -> PathBuf {
        let dir = match (&self.user, &self.target_domain) {
            (None, LaunchdTargetDomain::System) => LaunchdDirectory::admin_daemon(),
            (Some((_, username)), LaunchdTargetDomain::UserGui) => LaunchdDirectory::user_agent(username.as_str()),
            _ => unimplemented!("todo!")
        };
        dir.0.join(format!("{qualified_name}.plist"))
    }

    fn domain_target_string(&self) -> String {
        match &self.target_domain {
            LaunchdTargetDomain::System => "system".to_string(),
            LaunchdTargetDomain::UserGui => format!("gui/{}", self.user.as_ref().map(|(uid, _)| uid.clone()).expect("must have user context for a UserGui target_domain"))
        }
    }

    fn get_plist_contents(&self, ctx: &ServiceInstallCtx) -> Vec<u8> {
        match &ctx.contents {
            Some(contents) => contents.clone().into_bytes(),
            None => make_plist(
                &self.config.install,
                &ctx.label.to_qualified_name(),
                ctx.cmd_iter(),
                self.user.clone().map(|(_, username)| username),
                ctx.working_directory.clone(),
                ctx.environment.clone(),
                ctx.autostart,
                ctx.disable_restart_on_failure
            ).into_bytes(),
        }
    }

}

impl ServiceManager for LaunchdServiceManager {
    fn available(&self) -> io::Result<bool> {
        match which::which(LAUNCHCTL) {
            Ok(_) => Ok(true),
            Err(which::Error::CannotFindBinaryPath) => Ok(false),
            Err(x) => Err(io::Error::new(io::ErrorKind::Other, x)),
        }
    }

    fn status(&self, ctx: crate::ServiceStatusCtx) -> io::Result<crate::ServiceStatus> {
        let full_identifier = format!("{}/{}", self.domain_target_string(), ctx.label.to_qualified_name());
        let output = launchctl(&["print", &full_identifier])?;
        
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Check for service not installed (best-effort heuristic)
        if !output.status.success() {
            if (output.status.code() == Some(64) || output.status.code() == Some(113)) &&
            (stderr.contains("Could not find service") || stdout.contains("Could not find service")) {
                return Ok(crate::ServiceStatus::NotInstalled);
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "launchctl failed with exit code {:?}: {}",
                        output.status.code(),
                        if !stderr.trim().is_empty() { stderr } else { stdout }
                    ),
                ));
            }
        }

        // Use stdout if available, fallback to stderr
        let out: Cow<str> = if !stdout.trim().is_empty() { Cow::Borrowed(&stdout) } else { Cow::Borrowed(&stderr) };

        // Parse lines containing "state"
        for line in out.lines().map(str::trim) {
            if line.contains("state") {
                if line.contains("running") && !line.contains("not running") {
                    return Ok(crate::ServiceStatus::Running);
                } else if line.contains("not running") {
                    return Ok(crate::ServiceStatus::Stopped(None));
                }
            }
        }

        // Fallback: assume stopped if no state info found, and return the stdout to user for more information
        Ok(crate::ServiceStatus::Stopped(Some(out.to_string())))
    }

    fn install(&self, ctx: ServiceInstallCtx) -> io::Result<()> {
        let identifier = ctx.label.to_qualified_name();
        let plist_path = self.get_plist_path(&identifier);
        let plist_bytes = self.get_plist_contents(&ctx);

        // Uninstall the same service with in the same domain if it exist.
        self.uninstall(ServiceUninstallCtx { label:  ctx.label});

        // Write the new `/.plist`
        if let Some(p) = plist_path.parent() {
            std::fs::create_dir_all(p)?;
        }
        
        utils::write_file(
            plist_path.as_path(),
            &plist_bytes,
            PLIST_FILE_PERMISSIONS,
        )?;

        // Load the service.
        wrap_output(launchctl(&["bootstrap", &self.domain_target_string(), &plist_path.to_string_lossy().to_string()])?)?;

        Ok(())
    }

    fn uninstall(&self, ctx: ServiceUninstallCtx) -> io::Result<()> {
        let identifier = ctx.label.to_qualified_name();
        let plist_path = self.get_plist_path(&identifier);

        // Unload the service        
        wrap_output(launchctl(&["bootout", &self.domain_target_string(), &plist_path.to_string_lossy().to_string()])?)?;

        // Remove the .plist file
        std::fs::remove_file(plist_path)?;

        Ok(())
    }

    fn start(&self, ctx: ServiceStartCtx) -> io::Result<()> { 
        let full_identifier = format!("{}/{}", self.domain_target_string(), ctx.label.to_qualified_name());
        wrap_output(launchctl(&["kickstart", &full_identifier])?)?;
        Ok(())
    }

    /// Stops a service.
    /// To stop a service with "KeepAlive" enabled, call `uninstall` instead.
    fn stop(&self, ctx: ServiceStopCtx) -> io::Result<()> {
        let full_identifier = format!("{}/{}", self.domain_target_string(), ctx.label.to_qualified_name());
        let args: &[&str; 0] = &[];
        #[cfg(target_os = "macos")]
        let args = &["kill", ctx.signal.to_str(), &full_identifier];
    
        let output =  launchctl(args)?;
        match output.status.code() {
            Some(0) => Ok(()),
            Some(3) => Ok(()),
            _ => {
                wrap_output(output)?;
                Ok(())
            }
        }
    }

    fn level(&self) -> ServiceLevel {
        match self.target_domain {
            LaunchdTargetDomain::System => ServiceLevel::System,
            LaunchdTargetDomain::UserGui => ServiceLevel::User
        }
    }

    fn set_level(&mut self, level: ServiceLevel) -> io::Result<()> {
        match level {
            ServiceLevel::System => {
                self.target_domain = LaunchdTargetDomain::System
            },
            ServiceLevel::User => {
                self.target_domain = LaunchdTargetDomain::UserGui
            },
        }
        Ok(())
    }
}

fn launchctl(args: &[&str]) -> io::Result<Output> {
    let mut cmd = Command::new(LAUNCHCTL);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for arg in args {
        cmd.arg(arg);
    }

    cmd.output()
}

fn make_plist<'a>(
    config: &LaunchdInstallConfig,
    label: &str,
    args: impl Iterator<Item = &'a OsStr>,
    username: Option<String>,
    working_directory: Option<PathBuf>,
    environment: Option<Vec<(String, String)>>,
    autostart: bool,
    disable_restart_on_failure: bool,
) -> String {
    let mut dict = Dictionary::new();

    dict.insert("Label".to_string(), Value::String(label.to_string()));

    let program_arguments: Vec<Value> = args
        .map(|arg| Value::String(arg.to_string_lossy().into_owned()))
        .collect();
    dict.insert(
        "ProgramArguments".to_string(),
        Value::Array(program_arguments),
    );

    if !disable_restart_on_failure {
        dict.insert("KeepAlive".to_string(), Value::Boolean(config.keep_alive));
    }

    if let Some(username) = username {
        dict.insert("UserName".to_string(), Value::String(username));
    }

    if let Some(working_dir) = working_directory {
        dict.insert(
            "WorkingDirectory".to_string(),
            Value::String(working_dir.to_string_lossy().to_string()),
        );
    }

    if let Some(env_vars) = environment {
        let env_dict: Dictionary = env_vars
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        dict.insert(
            "EnvironmentVariables".to_string(),
            Value::Dictionary(env_dict),
        );
    }

    if autostart {
        dict.insert("RunAtLoad".to_string(), Value::Boolean(true));
    } else {
        dict.insert("RunAtLoad".to_string(), Value::Boolean(false));
    }

    let plist = Value::Dictionary(dict);

    let mut buffer = Vec::new();
    plist.to_writer_xml(&mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}
