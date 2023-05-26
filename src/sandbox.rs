use crate::config::SandboxConfig;
use crate::config::SandboxKind;
use anyhow::Context;
use anyhow::Result;
use std::ffi::OsStr;
use std::path::Path;

mod bubblewrap;

pub(crate) trait Sandbox {
    /// Runs `binary` inside the sandbox.
    fn run(&self, binary: &Path) -> Result<std::process::Output>;

    /// Bind a tmpfs at `dir`.
    fn tmpfs(&mut self, dir: &Path);

    /// Set the environment variable `var` to `value`.
    fn set_env(&mut self, var: &OsStr, value: &OsStr);

    /// Bind `dir` into the sandbox read-only.
    fn ro_bind(&mut self, dir: &Path);

    /// Bind `dir` into the sandbox writable.
    fn writable_bind(&mut self, dir: &Path);

    /// Append a sandbox-specific argument.
    fn arg(&mut self, arg: &OsStr);

    /// Pass through the value of `env_var_name`
    fn pass_env(&mut self, env_var_name: &str) {
        if let Ok(value) = std::env::var(env_var_name) {
            self.set_env(OsStr::new(env_var_name), OsStr::new(&value));
        }
    }

    /// Pass through all cargo environment variables.
    fn pass_cargo_env(&mut self) {
        self.pass_env("OUT_DIR");
        for (var, value) in std::env::vars_os() {
            if var.to_str().map(is_cargo_env).unwrap_or(false) {
                self.set_env(OsStr::new(&var), OsStr::new(&value));
            }
        }
    }
}

pub(crate) fn from_config(config: &SandboxConfig) -> Result<Option<Box<dyn Sandbox>>> {
    let mut sandbox = match &config.kind {
        SandboxKind::Disabled | SandboxKind::Inherit => return Ok(None),
        SandboxKind::Bubblewrap => Box::<bubblewrap::Bubblewrap>::default(),
    };
    for dir in &config.allow_read {
        sandbox.ro_bind(Path::new(dir));
    }
    let home = std::env::var("HOME").context("Couldn't get HOME env var")?;
    // TODO: Reasses if we want to list these here or just have the user list them in
    // their allow_read config.
    sandbox.ro_bind(Path::new("/usr"));
    sandbox.ro_bind(Path::new("/lib"));
    sandbox.ro_bind(Path::new("/lib64"));
    sandbox.ro_bind(Path::new("/bin"));
    sandbox.ro_bind(Path::new("/etc/alternatives"));
    // Note, we don't bind all of ~/.cargo because it might contain
    // crates.io credentials, which we'd like to avoid exposing.
    sandbox.ro_bind(Path::new(&format!("{home}/.cargo/bin")));
    sandbox.ro_bind(Path::new(&format!("{home}/.cargo/git")));
    sandbox.ro_bind(Path::new(&format!("{home}/.cargo/registry")));
    sandbox.ro_bind(Path::new(&format!("{home}/.rustup")));
    sandbox.tmpfs(Path::new("/var"));
    sandbox.tmpfs(Path::new("/tmp"));
    sandbox.tmpfs(Path::new("/run"));
    sandbox.tmpfs(Path::new("/usr/share"));
    sandbox.set_env(OsStr::new("USER"), OsStr::new("user"));
    sandbox.pass_env("PATH");
    sandbox.pass_env("HOME");
    for arg in &config.extra_args {
        sandbox.arg(OsStr::new(arg));
    }
    Ok(Some(sandbox))
}

fn is_cargo_env(var: &str) -> bool {
    if var == "RUSTC_WRAPPER" {
        return false;
    }
    var.starts_with("CARGO") || var.starts_with("RUSTC") || var == "TARGET"
}
